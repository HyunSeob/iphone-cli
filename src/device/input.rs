//! Touch, keyboard and hardware-button input.
//!
//! The phone drops every synthetic HID event unless a displayservice media
//! stream is active, so an [`InputSession`] first starts a screen-sharing
//! stream (audio + video, the way Xcode's Devices window does), throws the RTP
//! away, and then talks to the universal HID service.
//!
//! The media-session bring-up is adapted from the `coredevice_stream` example
//! tool in jkcoxson/idevice (MIT, see LICENSE.idevice).

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use idevice::{
    IdeviceError, ReadWrite, RsdService,
    core_device::{
        CoreDeviceError, build_screen_audio_offer, build_start_audio_parameters,
        hid::{
            ButtonState, DIGITIZER_SURFACE_MAIN_TOUCHSCREEN, HidSurface, IndigoHidClient, KeyboardUsage,
            MainKeyboardService, TOUCHSCREEN_STATE_CONTACT, TOUCHSCREEN_STATE_RELEASE, TouchscreenContact,
            UniversalHidServiceClient, build_multitouch_report, build_touchscreen_report,
        },
    },
    rsd::RsdHandshake,
};

use crate::{
    device::{
        display::{ActiveDisplay, Canvas, DisplayInfo, report_point},
        media::{self, CLIENT_SUPPORTED_FEATURES, MediaControl},
        pinch,
    },
    tcp::handle::{AdapterHandle, UdpSocketHandle},
};

/// Longest wait for the phone to make a new connection's HID devices.
const DEVICES_READY: Duration = Duration::from_secs(5);
/// Time between the phone having made its HID devices and the first touch.
/// The phone makes a connection's devices one after another and the system
/// then opens them; a touch that comes before the touchscreen is open is held
/// and delivered with the rest all at once, too fast for an edge swipe
/// (2026-11-12, dtuhidd's log). The Mac cannot see the opening, so this is
/// measured: on battery over Wi-Fi with a new session for every command, 0 of
/// 40 taps and 0 of 40 swipes back lost (it was 2.5 s and some 9% lost while
/// every session made three connections). See how-it-works §3.
const WARMUP: Duration = Duration::from_millis(300);
/// Keep the touch connection quiet for this long after a release; closing or
/// reusing it earlier makes the phone drop some taps. The wait happens before
/// the next input (and before closing), not after each one, so the caller can
/// already ask for the new screen meanwhile.
const SETTLE: Duration = Duration::from_millis(300);
/// Shortest pause between one release and the next touch on the open
/// connection, so two taps are not read as a double tap or a drag.
const TOUCH_GAP: Duration = Duration::from_millis(150);

/// Longest extra wait at the start of a session for the media stream's
/// first packets.
const FIRST_PACKET: Duration = Duration::from_secs(3);

/// Time between registering the temporary keyboard and its first key.
const KEYBOARD_ATTACH: Duration = Duration::from_millis(700);

/// `WARMUP`, or `IPH_INPUT_WARMUP_MS` when set (for tuning).
fn warmup() -> Duration {
    std::env::var("IPH_INPUT_WARMUP_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(WARMUP)
}

/// Wait until the phone has made this connection's HID devices: when it
/// answers the list of them. For the trace: when, or why not; and the list.
async fn devices_ready<R: ReadWrite>(client: &mut UniversalHidServiceClient<R>) -> (String, Option<Vec<HidSurface>>) {
    let began = now_ms();
    match tokio::time::timeout(DEVICES_READY, client.list_connected_services()).await {
        Ok(Ok(list)) => (format!("{} ms", now_ms().saturating_sub(began)), Some(list)),
        Ok(Err(e)) => (format!("error {e:?}"), None),
        Err(_) => ("timed out".to_string(), None),
    }
}

/// The touchscreens among a connection's HID surfaces, one per display, each
/// with the uniqueId of its display: `CoreDevice touchscreen(123-456-789)`.
/// A phone with one display lists one, `CoreDevice touchscreen(nil)`.
fn touchscreens(surfaces: &[HidSurface]) -> Vec<(u64, String)> {
    surfaces
        .iter()
        .filter_map(|s| {
            let id = s.product.as_deref()?.strip_prefix("CoreDevice touchscreen(")?.strip_suffix(')')?;
            Some((s.service_id, id.to_string()))
        })
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct Drain {
    last_packet_ms: Arc<AtomicU64>,
    /// When each socket (audio, video) got its first packet; 0 = not yet.
    first_packet_ms: Vec<Arc<AtomicU64>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drain {
    fn start(sockets: Vec<UdpSocketHandle>) -> Self {
        let last_packet_ms = Arc::new(AtomicU64::new(0));
        let first_packet_ms: Vec<Arc<AtomicU64>> =
            sockets.iter().map(|_| Arc::new(AtomicU64::new(0))).collect();
        // Bytes and packets each stream has delivered (audio, then video when
        // it is on), for the trace: the media crosses the same tunnel as
        // everything else.
        let counts: Vec<Arc<(AtomicU64, AtomicU64)>> =
            sockets.iter().map(|_| Arc::new((AtomicU64::new(0), AtomicU64::new(0)))).collect();
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = sockets
            .into_iter()
            .zip(first_packet_ms.iter().cloned())
            .zip(counts.iter().cloned())
            .map(|((s, first), count)| {
                let last = last_packet_ms.clone();
                tokio::spawn(async move {
                    // The media itself is not needed; only that it keeps coming.
                    while let Ok(packet) = s.recv().await {
                        let now = now_ms();
                        last.store(now, Ordering::Relaxed);
                        let _ = first.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
                        count.0.fetch_add(packet.data.len() as u64, Ordering::Relaxed);
                        count.1.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        if std::env::var("IPH_TRACE").is_ok_and(|v| v == "1") {
            tasks.push(tokio::spawn(async move {
                let mut before: Vec<(u64, u64)> = counts.iter().map(|_| (0, 0)).collect();
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let now: Vec<(u64, u64)> = counts
                        .iter()
                        .map(|c| (c.0.load(Ordering::Relaxed), c.1.load(Ordering::Relaxed)))
                        .collect();
                    let rates: Vec<String> = now
                        .iter()
                        .zip(&before)
                        .map(|(n, b)| format!("{} B/{} pkts", n.0 - b.0, n.1 - b.1))
                        .collect();
                    crate::device::ax::trace(|| format!("media in the last second: {}", rates.join(", ")));
                    before = now;
                }
            }));
        }
        Self {
            last_packet_ms,
            first_packet_ms,
            tasks,
        }
    }

    /// Whether every stream has delivered something.
    fn flowing(&self) -> bool {
        self.first_packet_ms.iter().all(|f| f.load(Ordering::Relaxed) > 0)
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Flick,
    Place,
}

/// Where one gesture's touch reports go.
#[derive(Debug, Clone, PartialEq)]
pub enum TouchTarget {
    /// The phone's one touchscreen, touched the way it always has been:
    /// points scale by the size the reading gave.
    Single,
    /// The touchscreen of the display in use, on a phone with several.
    Display { service: u64, display: ActiveDisplay },
}

impl TouchTarget {
    /// Report coordinates for `at`, a point on `on`.
    fn place(&self, on: &Canvas, at: (f64, f64)) -> (u16, u16) {
        match self {
            Self::Single => (normalize(at.0, on.size.0), normalize(at.1, on.size.1)),
            Self::Display { display, .. } => report_point(at, on.space, display),
        }
    }

    /// `plan`, whose contacts are normalized on `on`, in report coordinates.
    /// `None` when they already are.
    fn place_plan(&self, on: &Canvas, plan: &pinch::Plan) -> Option<pinch::Plan> {
        if matches!(self, Self::Single) {
            return None;
        }
        let place = |c: TouchscreenContact| {
            let at = (c.x as f64 / 65535.0 * on.size.0, c.y as f64 / 65535.0 * on.size.1);
            let (x, y) = self.place(on, at);
            TouchscreenContact { x, y, ..c }
        };
        let frames = plan.frames.iter().map(|f| pinch::Frame { at: f.at, contacts: f.contacts.map(place) }).collect();
        Some(pinch::Plan { frames, ..plan.clone() })
    }
}

/// Why a gesture was not sent: where it would land is not known.
#[derive(Debug)]
pub enum Unaimed {
    /// Which display is in use, or which touchscreen is its.
    Unknown(String),
    /// The display is not as the reading the points came from saw it: the
    /// phone was folded, unfolded or turned since, or the reading did not
    /// know the display.
    Changed { read: String, now: String },
}

impl std::fmt::Display for Unaimed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(why) => write!(f, "the display in use is not known: {why}"),
            Self::Changed { read, now } => {
                write!(f, "the screen was read as {read} and is {now} now")
            }
        }
    }
}

impl std::error::Error for Unaimed {}

/// One touch report.
enum Report<'a> {
    /// One finger: `TOUCHSCREEN_STATE_CONTACT` or `TOUCHSCREEN_STATE_RELEASE` at a point.
    One(u8, u16, u16),
    /// The fingers of a pinch.
    Pair(&'a [TouchscreenContact]),
}

/// Whether the phone has a touchscreen per display: by this connection's
/// list, or when the phone did not list them, by what was seen before.
fn several(touchscreens: Option<&[(u64, String)]>, several_known: bool) -> bool {
    match touchscreens {
        Some(list) => list.len() > 1,
        None => several_known,
    }
}

/// The touchscreen of `display`, for points of `on`, the space of the
/// reading they came from, which must have been read on this display,
/// turned as it is, and still have the size it had then.
fn aim_at(touchscreens: &[(u64, String)], display: ActiveDisplay, on: Option<&Canvas>) -> Result<TouchTarget, Unaimed> {
    let service = touchscreens
        .iter()
        .find(|(_, id)| *id == display.unique_id)
        .map(|(service, _)| *service)
        .ok_or_else(|| Unaimed::Unknown(format!("no touchscreen belongs to display {}", display.unique_id)))?;
    if let Some(on) = on
        && !on.is_on(&display)
    {
        return Err(Unaimed::Changed { read: on.describe(), now: display.canvas(on.space).describe() });
    }
    Ok(TouchTarget::Display { service, display })
}

/// Where to aim when the display in use may have to be asked for (`ask`).
/// A phone with one touchscreen is never asked.
async fn aim_with(
    touchscreens: Option<&[(u64, String)]>,
    several_known: bool,
    on: Option<&Canvas>,
    ask: impl AsyncFnOnce() -> Result<ActiveDisplay, String>,
) -> Result<TouchTarget, Unaimed> {
    if !several(touchscreens, several_known) {
        return Ok(TouchTarget::Single);
    }
    let display = ask().await.map_err(Unaimed::Unknown)?;
    aim_at(touchscreens.unwrap_or_default(), display, on)
}

/// The touchscreen a report goes to, and the report. With one display that
/// is the main touchscreen, which the library's `send_touchscreen` and
/// `send_multitouch` write to with these same reports.
fn route(target: &TouchTarget, report: Report<'_>, timestamp: Option<u64>) -> Result<(u64, Vec<u8>), CoreDeviceError> {
    let service = match target {
        TouchTarget::Single => DIGITIZER_SURFACE_MAIN_TOUCHSCREEN,
        TouchTarget::Display { service, .. } => *service,
    };
    let bytes = match report {
        Report::One(state, x, y) => build_touchscreen_report(state, x, y, timestamp),
        Report::Pair(contacts) => build_multitouch_report(contacts, timestamp)?,
    };
    Ok((service, bytes))
}

/// Every touch report leaves through here, for the touchscreen `target`
/// names.
async fn touch(
    hid: &mut UniversalHidServiceClient<Box<dyn ReadWrite>>,
    target: &TouchTarget,
    report: Report<'_>,
) -> Result<(), IdeviceError> {
    let (service, bytes) = route(target, report, None)?;
    hid.send_report(service, bytes).await
}

pub struct InputSession {
    media: MediaControl,
    hid: UniversalHidServiceClient<Box<dyn ReadWrite>>,
    /// A second connection for the keyboard, so its request/response traffic
    /// stays off the touch connection. Opened on the first key.
    keyboard: Option<UniversalHidServiceClient<Box<dyn ReadWrite>>>,
    /// The temporary keyboard while it is registered.
    board: Option<MainKeyboardService>,
    /// For the hardware buttons. Opened on the first button.
    indigo: Option<IndigoHidClient<Box<dyn ReadWrite>>>,
    /// To open those two later.
    adapter: AdapterHandle,
    rsd: RsdHandshake,
    drain: Drain,
    pub started: Instant,
    last_release: Option<Instant>,
    /// Touch and button reports written so far.
    reports: u64,
    /// The two fingers of a pinch while they may be down on the phone.
    pair_held: Option<[TouchscreenContact; 2]>,
    /// Where those fingers are.
    pair_target: TouchTarget,
    /// This connection's touchscreens, as `(service id, display uniqueId)`,
    /// when the phone listed them.
    touchscreens: Option<Vec<(u64, String)>>,
    /// The phone was seen to have several touchscreens before. When this
    /// connection's list did not come, that stands.
    several_known: bool,
    /// To ask which display is in use. Opened when first needed.
    displays: Option<DisplayInfo>,
    /// Names `session_id` while this session's streams may be running.
    marker: PathBuf,
    session_id: uuid::Uuid,
}

/// Normalized HID coordinate (0..=65535) for a point in logical screen points.
pub fn normalize(v: f64, extent: f64) -> u16 {
    if extent <= 0.0 {
        return 0;
    }
    ((v / extent) * 65535.0).round().clamp(0.0, 65535.0) as u16
}

impl InputSession {
    /// `video`: also start the screen video stream. The audio stream alone
    /// sets up the screen-sharing session that makes touches count
    /// (2026-11-11: taps, drags, long presses, keys and buttons all work
    /// without the video; 24 of 24 first drags on a new session either way).
    /// The video is up to 6 Mbit/s while the screen moves. Over Wi-Fi the
    /// tunnel carried a seventh of that, and everything else queued behind it.
    ///
    /// `marker`: the file that names the sessions whose streams may run on
    /// the phone (see [`media::marked`]).
    ///
    /// `several_known`: the phone was seen to have more than one touchscreen.
    pub async fn open(
        adapter: &mut AdapterHandle,
        rsd: &mut RsdHandshake,
        video: bool,
        marker: PathBuf,
        several_known: bool,
    ) -> Result<Self> {
        let began = Instant::now();
        let began_ms = now_ms();
        let mut media = MediaControl::connect(adapter, rsd).await?;

        let audio_udp = adapter.bind_udp(0).await?;
        let receiver_ip = adapter.host_ip().to_string();
        let sender_ip = adapter.peer_ip().to_string();

        let session_id = uuid::Uuid::new_v4();
        media::mark(&marker, &session_id.to_string());

        let audio_call = uuid::Uuid::new_v4().to_string().to_uppercase();
        let audio_offer = build_screen_audio_offer(&audio_call, &media::call_info())
            .map_err(|e| anyhow!("audio offer: {e:?}"))?;
        let audio = media
            .start(build_start_audio_parameters(
                &receiver_ip,
                audio_udp.local_port(),
                &sender_ip,
                50000,
                audio_offer,
                CLIENT_SUPPORTED_FEATURES,
                session_id,
            ))
            .await;
        let mut sockets = Vec::new();
        let mut video = video;
        match audio {
            Ok(()) => sockets.push(audio_udp),
            // CoreDevice 9022 "The device microphone or camera is currently
            // in use" (a call, a recording): the phone refuses the audio
            // stream, so the video stream alone sets up the session.
            Err(e) if format!("{e:?}").contains("Integer(9022)") => video = true,
            Err(e) => return Err(anyhow!("start audio stream: {e:?}")),
        }
        if video {
            sockets.push(media::start_video(&mut media, adapter, 1, session_id).await?);
        }

        let drain = Drain::start(sockets);

        // Only the touch connection now: every HID connection makes its own
        // five devices on the phone (a touchscreen, a keyboard, buttons...),
        // one after another, and a touch waits until the phone has opened
        // them. The keyboard and the buttons connect when first used.
        let mut hid = UniversalHidServiceClient::connect_rsd(adapter, rsd)
            .await
            .map_err(|e| anyhow!("hid service: {e:?}"))?;
        let (hid_ready, listed) = devices_ready(&mut hid).await;
        let touchscreens = listed.as_deref().map(touchscreens);
        crate::device::ax::trace(|| format!("input: touchscreens {touchscreens:?}"));
        let mut session = Self {
            media,
            hid,
            keyboard: None,
            board: None,
            indigo: None,
            adapter: adapter.clone(),
            rsd: rsd.clone(),
            drain,
            started: Instant::now(),
            last_release: None,
            reports: 0,
            pair_held: None,
            pair_target: TouchTarget::Single,
            touchscreens,
            several_known,
            displays: None,
            marker,
            session_id,
        };
        // A "finger up" with no finger down does nothing on the phone, but it
        // is a first report, and the first report on a new connection is
        // sometimes lost, taking a whole gesture with it. Measured with a new
        // session for every scroll: 7 of 40 first drags lost without this,
        // 0 of 60 with it (iOS 27.0.1). `IPH_INPUT_PRIME=0` turns it off.
        let prime = std::env::var("IPH_INPUT_PRIME").map(|v| v != "0").unwrap_or(true);
        let primer = if prime {
            let aimed = session.aim(None).await;
            if let Err(e) = &aimed {
                crate::device::ax::trace(|| format!("input: not primed: {e}"));
            }
            aimed.ok()
        } else {
            None
        };
        if let Some(target) = &primer {
            let _ = touch(&mut session.hid, target, Report::One(TOUCHSCREEN_STATE_RELEASE, 65535, 65535)).await;
        }
        tokio::time::sleep(warmup()).await;
        // Touches only count while the phone is sending the stream. It has
        // usually begun by now; when it has not, wait for it a little.
        let waiting = Instant::now();
        while !session.drain.flowing() && waiting.elapsed() < FIRST_PACKET {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        crate::device::ax::trace(|| {
            let firsts: Vec<String> = session
                .drain
                .first_packet_ms
                .iter()
                .map(|f| match f.load(Ordering::Relaxed) {
                    0 => "none".to_string(),
                    at => format!("{} ms", at.saturating_sub(began_ms)),
                })
                .collect();
            format!(
                "input: session open after {} ms, devices ready at {hid_ready}, first audio/video packet at {}",
                began.elapsed().as_millis(),
                firsts.join(" / ")
            )
        });
        if let Some(target) = &primer {
            let _ = touch(&mut session.hid, target, Report::One(TOUCHSCREEN_STATE_RELEASE, 65535, 65535)).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
        }

        session.started = Instant::now();
        Ok(session)
    }

    /// How many touchscreens this connection has, when the phone listed them.
    pub fn touchscreen_count(&self) -> Option<usize> {
        self.touchscreens.as_ref().map(Vec::len)
    }

    /// Where a gesture's touches go. A phone with one touchscreen needs no
    /// asking. On one with several, the display in use is asked for now, and
    /// `on`, the space of the reading the points came from, must have been
    /// read on it, turned as it is, and still have the size it had then.
    async fn aim(&mut self, on: Option<&Canvas>) -> Result<TouchTarget, Unaimed> {
        let (touchscreens, several_known) = (self.touchscreens.clone(), self.several_known);
        let target = aim_with(touchscreens.as_deref(), several_known, on, async || self.active_display().await).await?;
        crate::device::ax::trace(|| match &target {
            TouchTarget::Display { service, display } => format!(
                "touch: display {} {:?}, panel {}x{}, touchscreen {service}",
                display.unique_id, display.rotation, display.panel.0, display.panel.1
            ),
            TouchTarget::Single => "touch: one touchscreen".into(),
        });
        Ok(target)
    }

    async fn active_display(&mut self) -> Result<ActiveDisplay, String> {
        let mut link = match self.displays.take() {
            Some(link) => link,
            None => DisplayInfo::open(&self.adapter, &self.rsd).await?,
        };
        let display = link.active().await?;
        self.displays = Some(link);
        Ok(display)
    }

    pub(crate) fn session_id(&self) -> String {
        self.session_id.to_string()
    }

    /// Milliseconds since the phone last sent a media packet, if it ever did.
    pub fn media_idle_ms(&self) -> Option<u64> {
        let last = self.drain.last_packet_ms.load(Ordering::Relaxed);
        (last > 0).then(|| now_ms().saturating_sub(last))
    }

    async fn wait_since_release(&mut self, gap: Duration) {
        if let Some(t) = self.last_release {
            let since = t.elapsed();
            if since < gap {
                tokio::time::sleep(gap - since).await;
            }
        }
    }

    /// Before closing: let the last release sink in.
    async fn settle_previous(&mut self) {
        self.wait_since_release(SETTLE).await;
    }

    /// Before a new touch: keep it apart from the previous one.
    async fn gap(&mut self) {
        self.wait_since_release(TOUCH_GAP).await;
    }

    /// How many touch and button reports this session has written. A gesture
    /// that fails without this number moving sent nothing to the phone.
    pub fn reports_sent(&self) -> u64 {
        self.reports
    }

    async fn contact(&mut self, target: &TouchTarget, x: u16, y: u16) -> Result<()> {
        crate::device::ax::trace(|| format!("contact {x},{y}"));
        touch(&mut self.hid, target, Report::One(TOUCHSCREEN_STATE_CONTACT, x, y))
            .await
            .map_err(|e| anyhow!("touch contact: {e:?}"))?;
        self.reports += 1;
        Ok(())
    }

    async fn release(&mut self, target: &TouchTarget, x: u16, y: u16) -> Result<()> {
        let r = touch(&mut self.hid, target, Report::One(TOUCHSCREEN_STATE_RELEASE, x, y))
            .await
            .map_err(|e| anyhow!("touch release: {e:?}"));
        crate::device::ax::trace(|| "touch released".into());
        self.last_release = Some(Instant::now());
        if r.is_ok() {
            self.reports += 1;
        }
        r
    }

    /// One finger down at `at`, a point on `on`, held for `hold`, then lifted.
    pub async fn tap(&mut self, on: &Canvas, at: (f64, f64), hold: Duration) -> Result<()> {
        let target = self.aim(Some(on)).await?;
        let (x, y) = target.place(on, at);
        self.gap().await;
        crate::device::ax::trace(|| format!("tap down {x},{y}"));
        self.contact(&target, x, y).await?;
        tokio::time::sleep(hold).await;
        self.release(&target, x, y).await
    }

    pub async fn double_tap(&mut self, on: &Canvas, at: (f64, f64)) -> Result<()> {
        let target = self.aim(Some(on)).await?;
        let (x, y) = target.place(on, at);
        self.gap().await;
        for i in 0..2 {
            self.contact(&target, x, y).await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.release(&target, x, y).await?;
            if i == 0 {
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        }
        Ok(())
    }

    /// Move one finger from `from` to `to` over `duration`, then lift.
    ///
    /// [`Motion::Flick`] moves at constant speed and lifts while moving, so a
    /// list keeps coasting (a swipe). [`Motion::Place`] slows to a stop before
    /// lifting, so the content ends up moved by the finger's distance and
    /// stays there (a scroll step, a drag-and-drop).
    pub async fn drag(
        &mut self,
        on: &Canvas,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
        motion: Motion,
    ) -> Result<()> {
        let target = self.aim(Some(on)).await?;
        let (from, to) = (target.place(on, from), target.place(on, to));
        self.gap().await;
        let steps = (duration.as_millis() / 16).clamp(4, 240) as u32;
        let start = Instant::now();
        crate::device::ax::trace(|| format!("drag start {from:?} -> {to:?}"));
        let mut sent_any = false;
        let mut last = from;
        let res: Result<()> = async {
            for i in 0..=steps {
                let t = i as f64 / steps as f64;
                let eased = match motion {
                    Motion::Flick => t,
                    // Cubic ease-out: fast at first, almost still at the end.
                    Motion::Place => 1.0 - (1.0 - t).powi(3),
                };
                let x = (from.0 as f64 + (to.0 as f64 - from.0 as f64) * eased).round() as u16;
                let y = (from.1 as f64 + (to.1 as f64 - from.1 as f64) * eased).round() as u16;
                self.contact(&target, x, y).await?;
                sent_any = true;
                last = (x, y);
                let due = duration.mul_f64(t);
                let elapsed = start.elapsed();
                if due > elapsed {
                    tokio::time::sleep(due - elapsed).await;
                }
            }
            if motion == Motion::Place {
                // Rest on the end point; the last samples must say "not
                // moving" or the list flings on release.
                for _ in 0..4 {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    self.contact(&target, to.0, to.1).await?;
                }
            }
            Ok(())
        }
        .await;
        if sent_any {
            // Always lift, also after a failed sample.
            let lifted = self.release(&target, last.0, last.1).await;
            res?;
            lifted?;
        } else {
            res?;
        }
        Ok(())
    }

    /// Two fingers moving together or turning, then lifted. Every frame counts
    /// in [`reports_sent`](Self::reports_sent). The plan's contacts are
    /// normalized on `on`.
    pub async fn pinch(&mut self, on: &Canvas, plan: &pinch::Plan) -> Result<()> {
        let target = self.aim(Some(on)).await?;
        let placed = target.place_plan(on, plan);
        let plan = placed.as_ref().unwrap_or(plan);
        self.gap().await;
        crate::device::ax::trace(|| format!("pinch {} frames, span {:?}", plan.frames.len(), plan.span));
        self.pair_target = target.clone();
        let (hid, reports) = (&mut self.hid, &mut self.reports);
        let played = pinch::play(plan, &mut self.pair_held, async |contacts: &[TouchscreenContact]| -> Result<()> {
            touch(hid, &target, Report::Pair(contacts))
                .await
                .map_err(|e| anyhow!("touch pinch: {e:?}"))?;
            *reports += 1;
            Ok(())
        })
        .await;
        self.last_release = Some(Instant::now());
        played
    }

    /// Lift a pinch that was cut off, so no finger stays down on the phone.
    /// Bounded: a session given up after an error may have a dead connection.
    async fn lift_held(&mut self) {
        let Some(pair) = self.pair_held.take() else {
            return;
        };
        let lifted = pinch::lifted(&pair);
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            touch(&mut self.hid, &self.pair_target, Report::Pair(&lifted)),
        )
        .await;
        self.last_release = Some(Instant::now());
    }

    /// Press and release a hardware button (HID usage page / usage).
    pub async fn button(&mut self, usage_page: u64, usage: u64, hold: Duration) -> Result<()> {
        if self.indigo.is_none() {
            let indigo = IndigoHidClient::connect_rsd(&mut self.adapter, &mut self.rsd)
                .await
                .map_err(|e| anyhow!("indigo HID service unavailable: {e:?}"))?;
            self.indigo = Some(indigo);
            // Its devices come after those already asked for, so the touch
            // connection's answer comes once they are made too.
            let (ready, _) = devices_ready(&mut self.hid).await;
            tokio::time::sleep(WARMUP).await;
            crate::device::ax::trace(|| format!("input: buttons connected, devices ready at {ready}"));
        }
        let Some(indigo) = self.indigo.as_mut() else {
            unreachable!()
        };
        indigo
            .send_button(usage_page, usage, ButtonState::Down)
            .await
            .map_err(|e| anyhow!("button down: {e:?}"))?;
        self.reports += 1;
        tokio::time::sleep(hold).await;
        indigo
            .send_button(usage_page, usage, ButtonState::Up)
            .await
            .map_err(|e| anyhow!("button up: {e:?}"))?;
        self.last_release = Some(Instant::now());
        Ok(())
    }

    /// Register a temporary hardware keyboard with the phone. While it is
    /// registered iOS hides the on-screen keyboard, so pair every attach with
    /// [`keyboard_detach`](Self::keyboard_detach).
    pub async fn keyboard_attach(&mut self) -> Result<()> {
        if self.board.is_some() {
            return Ok(());
        }
        if self.keyboard.is_none() {
            let mut keyboard = UniversalHidServiceClient::connect_rsd(&mut self.adapter, &mut self.rsd)
                .await
                .map_err(|e| anyhow!("hid service (keyboard): {e:?}"))?;
            let (ready, _) = devices_ready(&mut keyboard).await;
            tokio::time::sleep(WARMUP).await;
            crate::device::ax::trace(|| format!("keyboard: connected, devices ready at {ready}"));
            self.keyboard = Some(keyboard);
        }
        let Some(keyboard) = self.keyboard.as_mut() else {
            unreachable!()
        };
        crate::device::ax::trace(|| "keyboard: registering".into());
        let board = keyboard
            .create_main_keyboard()
            .await
            .map_err(|e| anyhow!("register keyboard: {e}"))?;
        self.board = Some(board);
        // The phone needs a moment to attach the new keyboard; keys sent
        // 250 ms after registering were lost, 800 ms worked (iOS 27.0.1).
        tokio::time::sleep(KEYBOARD_ATTACH).await;
        crate::device::ax::trace(|| "keyboard: registered".into());
        Ok(())
    }

    /// Press key combinations one after another on the attached keyboard.
    /// Each is a HID keyboard usage with the modifier usages held around it,
    /// e.g. `(0x19, [0xE3])` for Command-V.
    pub async fn keyboard_press(&mut self, combos: &[(u64, Vec<u64>)]) -> Result<()> {
        let usage = |raw: u64| {
            KeyboardUsage::new(raw as u16).map_err(|e| anyhow!("key usage {raw:#x}: {e}"))
        };
        // Validate everything before the first key goes out.
        for (key, modifiers) in combos {
            usage(*key)?;
            for m in modifiers {
                usage(*m)?;
            }
        }
        let (Some(board), Some(keyboard)) = (self.board.as_mut(), self.keyboard.as_mut()) else {
            return Err(anyhow!("no keyboard attached"));
        };
        for (key, modifiers) in combos {
            crate::device::ax::trace(|| format!("keyboard: combo {key:#x}"));
            for m in modifiers {
                keyboard
                    .main_keyboard_key_down(board, usage(*m)?)
                    .await
                    .map_err(|e| anyhow!("modifier down: {e}"))?;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            keyboard
                .main_keyboard_key_down(board, usage(*key)?)
                .await
                .map_err(|e| anyhow!("key down: {e}"))?;
            tokio::time::sleep(Duration::from_millis(40)).await;
            // Everything up at once.
            keyboard
                .set_main_keyboard_usages(board, std::iter::empty())
                .await
                .map_err(|e| anyhow!("key up: {e}"))?;
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        Ok(())
    }

    /// Remove the temporary keyboard; this also releases any key still down
    /// and brings the on-screen keyboard back.
    pub async fn keyboard_detach(&mut self) -> Result<()> {
        let Some(mut board) = self.board.take() else {
            return Ok(());
        };
        let Some(keyboard) = self.keyboard.as_mut() else {
            return Ok(());
        };
        crate::device::ax::trace(|| "keyboard: removing".into());
        let removed = keyboard.remove_main_keyboard(&mut board).await;
        crate::device::ax::trace(|| "keyboard: removed".into());
        removed.map_err(|e| anyhow!("remove keyboard: {e}"))
    }

    /// Attach, press, detach.
    pub async fn keys(&mut self, combos: &[(u64, Vec<u64>)]) -> Result<()> {
        self.keyboard_attach().await?;
        let pressed = self.keyboard_press(combos).await;
        // Remove the keyboard whatever happened.
        let detached = self.keyboard_detach().await;
        pressed?;
        detached
    }

    /// One key with optional modifiers.
    pub async fn key(&mut self, usage: u64, modifiers: &[u64]) -> Result<()> {
        self.keys(&[(usage, modifiers.to_vec())]).await
    }

    pub async fn close(mut self) {
        self.lift_held().await;
        let _ = self.keyboard_detach().await;
        self.settle_previous().await;
        self.stop_media().await;
    }

    /// Stop the stream of a session given up after an error, without the
    /// orderly wait. A stream nobody stops keeps the phone encoding video.
    pub async fn abandon(mut self) {
        self.lift_held().await;
        self.stop_media().await;
    }

    /// Stop this session's streams and no other session's: a recording's
    /// video goes on.
    async fn stop_media(&mut self) {
        let stopped = self.media.stop_own(&self.session_id.to_string(), &self.marker).await;
        crate::device::ax::trace(|| format!("input: streams of session {} stopped: {stopped:?}", self.session_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::display::{Rotation, Space};

    fn surface(service_id: u64, product: &str) -> HidSurface {
        HidSurface { service_id, product: Some(product.into()), primary_usage: None, primary_usage_page: None }
    }

    /// What an iPhone Duo listed (iOS 27.2.1).
    fn duo() -> Vec<HidSurface> {
        vec![
            surface(257, "CoreDevice touchscreen(987-654-321)"),
            surface(258, "CoreDevice touchscreen(123-456-789)"),
            surface(512, "CoreDevice keyboard"),
            surface(1026, "CoreDevice mainScreenButtons"),
            surface(1280, "CoreDevice avpCustom"),
            surface(1281, "CoreDevice touchscreenGesture"),
        ]
    }

    fn inner(rotation: Rotation) -> ActiveDisplay {
        ActiveDisplay { unique_id: "123-456-789".into(), display_id: Some(2), panel: (669.0, 951.0), rotation }
    }

    fn cover() -> ActiveDisplay {
        ActiveDisplay { unique_id: "987-654-321".into(), display_id: Some(1), panel: (466.0, 678.0), rotation: Rotation::R0 }
    }

    /// A reading of a phone with one display, which does not say which one.
    fn unaware(space: Space, size: (f64, f64)) -> Canvas {
        Canvas { space, size, display: None }
    }

    #[test]
    fn a_phone_with_one_display_lists_one_touchscreen_and_a_duo_one_per_display() {
        let one = touchscreens(&[
            surface(257, "CoreDevice touchscreen(nil)"),
            surface(1281, "CoreDevice touchscreenGesture"),
        ]);
        assert_eq!(one, [(257, "nil".to_string())]);
        assert!(!several(Some(&one), false));
        let duo = touchscreens(&duo());
        assert_eq!(duo, [(257, "987-654-321".to_string()), (258, "123-456-789".to_string())]);
        assert!(several(Some(&duo), false));
    }

    #[test]
    fn without_a_list_only_a_phone_seen_with_several_touchscreens_counts_as_one_with_several() {
        assert!(several(None, true));
        assert!(!several(None, false));
        // The list, when it comes, decides.
        assert!(!several(Some(&touchscreens(&[surface(257, "CoreDevice touchscreen(nil)")])), true));
    }

    #[test]
    fn touches_go_to_the_touchscreen_of_the_display_in_use() {
        let list = touchscreens(&duo());
        for space in [Space::Screen, Space::Panel] {
            let on = inner(Rotation::R270).canvas(space);
            let aimed = aim_at(&list, inner(Rotation::R270), Some(&on)).unwrap();
            assert_eq!(aimed, TouchTarget::Display { service: 258, display: inner(Rotation::R270) }, "{space:?}");
        }
        let on = cover().canvas(Space::Screen);
        let aimed = aim_at(&list, cover(), Some(&on)).unwrap();
        assert_eq!(aimed, TouchTarget::Display { service: 257, display: cover() });
    }

    #[test]
    fn a_display_with_no_touchscreen_or_no_list_is_not_guessed() {
        let list = touchscreens(&duo());
        let stranger = ActiveDisplay { unique_id: "555".into(), ..inner(Rotation::R0) };
        assert!(matches!(aim_at(&list, stranger, None), Err(Unaimed::Unknown(_))));
        assert!(matches!(aim_at(&[], inner(Rotation::R0), None), Err(Unaimed::Unknown(_))));
    }

    #[test]
    fn points_read_before_a_fold_or_a_turn_are_not_sent() {
        let list = touchscreens(&duo());
        // Read folded, on the cover display.
        let on = cover().canvas(Space::Screen);
        assert!(matches!(aim_at(&list, inner(Rotation::R270), Some(&on)), Err(Unaimed::Changed { .. })));
        // Read upright, then turned: the screen's width and height swap.
        let on = inner(Rotation::R0).canvas(Space::Screen);
        assert!(matches!(aim_at(&list, inner(Rotation::R270), Some(&on)), Err(Unaimed::Changed { .. })));
        // The panel's own points do not swap, but the home screen lays its icons
        // out again when the display turns.
        let on = inner(Rotation::R0).canvas(Space::Panel);
        assert_eq!(on.size, inner(Rotation::R90).size(Space::Panel));
        assert!(matches!(aim_at(&list, inner(Rotation::R90), Some(&on)), Err(Unaimed::Changed { .. })));
    }

    #[test]
    fn a_turn_between_the_two_landscapes_keeps_the_size_and_is_still_a_change() {
        let list = touchscreens(&duo());
        for space in [Space::Screen, Space::Panel] {
            let on = inner(Rotation::R90).canvas(space);
            assert_eq!(on.size, inner(Rotation::R270).size(space), "{space:?}");
            match aim_at(&list, inner(Rotation::R270), Some(&on)) {
                Err(Unaimed::Changed { read, now }) => assert_ne!(read, now, "{space:?}"),
                other => panic!("{space:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn another_display_of_the_same_size_is_a_change() {
        let list = touchscreens(&duo());
        let read = ActiveDisplay { unique_id: "987-654-321".into(), ..inner(Rotation::R270) };
        let on = read.canvas(Space::Screen);
        assert_eq!(on.size, inner(Rotation::R270).size(Space::Screen));
        assert!(matches!(aim_at(&list, inner(Rotation::R270), Some(&on)), Err(Unaimed::Changed { .. })));
    }

    #[test]
    fn a_reading_that_did_not_know_the_display_is_not_sent_on_a_phone_with_several() {
        let list = touchscreens(&duo());
        for space in [Space::Screen, Space::Panel] {
            let on = unaware(space, inner(Rotation::R270).size(space));
            assert!(matches!(aim_at(&list, inner(Rotation::R270), Some(&on)), Err(Unaimed::Changed { .. })), "{space:?}");
        }
    }

    fn not_asked() -> Result<ActiveDisplay, String> {
        Err("not asked".into())
    }

    #[tokio::test]
    async fn a_phone_with_one_touchscreen_is_never_asked_which_display_is_in_use() {
        let one = touchscreens(&[surface(257, "CoreDevice touchscreen(nil)")]);
        let on = unaware(Space::Screen, (402.0, 874.0));
        let on = Some(&on);
        let asked = std::cell::Cell::new(0);
        let ask = async || {
            asked.set(asked.get() + 1);
            not_asked()
        };
        assert_eq!(aim_with(Some(&one), false, on, ask).await.unwrap(), TouchTarget::Single);
        assert_eq!(aim_with(None, false, on, ask).await.unwrap(), TouchTarget::Single, "not listed, not seen with several");
        assert_eq!(asked.get(), 0);
        // A Duo is asked, once per aim.
        let duo = touchscreens(&duo());
        let answer = async || {
            asked.set(asked.get() + 1);
            Ok(inner(Rotation::R0))
        };
        let on = inner(Rotation::R0).canvas(Space::Screen);
        let aimed = aim_with(Some(&duo), false, Some(&on), answer).await;
        assert_eq!(aimed.unwrap(), TouchTarget::Display { service: 258, display: inner(Rotation::R0) });
        assert_eq!(asked.get(), 1);
    }

    #[test]
    fn with_one_touchscreen_reports_go_to_the_main_touchscreen_with_their_coordinates_untouched() {
        // What the library's `send_touchscreen` and `send_multitouch` send.
        let (service, bytes) = route(&TouchTarget::Single, Report::One(TOUCHSCREEN_STATE_CONTACT, 17615, 40569), Some(5)).unwrap();
        assert_eq!(service, 257);
        assert_eq!(bytes, build_touchscreen_report(TOUCHSCREEN_STATE_CONTACT, 17615, 40569, Some(5)));
        assert_eq!(bytes[4..8], [17615u16.to_le_bytes(), 40569u16.to_le_bytes()].concat()[..]);
        let pair = [
            TouchscreenContact { identity: 0, touching: true, x: 100, y: 200 },
            TouchscreenContact { identity: 1, touching: true, x: 300, y: 400 },
        ];
        let (service, bytes) = route(&TouchTarget::Single, Report::Pair(&pair), Some(5)).unwrap();
        assert_eq!((service, bytes), (257, build_multitouch_report(&pair, Some(5)).unwrap()));
        // The inner display of a Duo has its own touchscreen.
        let inner = TouchTarget::Display { service: 258, display: inner(Rotation::R270) };
        let (service, _) = route(&inner, Report::One(TOUCHSCREEN_STATE_RELEASE, 1, 2), Some(5)).unwrap();
        assert_eq!(service, 258);
    }

    #[test]
    fn with_one_touchscreen_points_scale_by_the_reading_as_before() {
        // iph 0.1.9 sent 17615,40569 for a tap at this point of a 402x874
        // screen (IPH_TRACE, iPhone 16 Pro).
        let on = unaware(Space::Screen, (402.0, 874.0));
        assert_eq!(TouchTarget::Single.place(&on, (68.7 + 78.7 / 2.0, 495.7 + 90.7 / 2.0)), (17615, 40569));
        for at in [(0.0, 0.0), (201.0, 437.0), (37.3, 811.9), (402.0, 874.0), (-3.0, 900.0)] {
            assert_eq!(TouchTarget::Single.place(&on, at), (normalize(at.0, 402.0), normalize(at.1, 874.0)), "{at:?}");
        }
        let plan = pinch::plan(&pinch::Spec {
            screen: on.size,
            center: (201.0, 437.0),
            scale: 2.0,
            rotate: 0.0,
            duration: Duration::from_millis(100),
        })
        .unwrap();
        assert!(TouchTarget::Single.place_plan(&on, &plan).is_none());
    }

    #[test]
    fn a_pinch_on_a_turned_display_has_its_contacts_turned() {
        let on = inner(Rotation::R270).canvas(Space::Screen);
        let plan = pinch::plan(&pinch::Spec {
            screen: on.size,
            center: (951.0 * 0.4, 669.0 * 0.6),
            scale: 2.0,
            rotate: 0.0,
            duration: Duration::from_millis(100),
        })
        .unwrap();
        let target = TouchTarget::Display { service: 258, display: inner(Rotation::R270) };
        let placed = target.place_plan(&on, &plan).unwrap();
        assert_eq!(placed.frames.len(), plan.frames.len());
        for (a, b) in plan.frames.iter().zip(&placed.frames) {
            for (c, d) in a.contacts.iter().zip(&b.contacts) {
                // rot270: (u, v) lands on (1 - v, u).
                assert!((d.x as i32 - (65535 - c.y as i32)).abs() <= 1, "{c:?} -> {d:?}");
                assert!((d.y as i32 - c.x as i32).abs() <= 1, "{c:?} -> {d:?}");
                assert_eq!((d.identity, d.touching), (c.identity, c.touching));
            }
        }
    }
}
