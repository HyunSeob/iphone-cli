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
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use idevice::{
    ReadWrite, RsdService,
    core_device::{
        CallInfoBlob, DisplayServiceClient, build_screen_audio_offer, build_screen_video_offer,
        build_start_audio_parameters, build_start_video_parameters,
        hid::{
            ButtonState, IndigoHidClient, KeyboardUsage, MainKeyboardService,
            TOUCHSCREEN_STATE_CONTACT, TOUCHSCREEN_STATE_RELEASE, UniversalHidServiceClient,
        },
    },
    rsd::RsdHandshake,
};

use crate::tcp::handle::{AdapterHandle, UdpSocketHandle};

/// What the controller says it supports. Xcode's device window sends 140.
const CLIENT_SUPPORTED_FEATURES: u64 = 140;

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
/// answers the list of them. For the trace: when, or why not.
async fn devices_ready<R: ReadWrite>(client: &mut UniversalHidServiceClient<R>) -> String {
    let began = now_ms();
    match tokio::time::timeout(DEVICES_READY, client.list_connected_services()).await {
        Ok(Ok(_)) => format!("{} ms", now_ms().saturating_sub(began)),
        Ok(Err(e)) => format!("error {e:?}"),
        Err(_) => "timed out".to_string(),
    }
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

pub struct InputSession {
    display: DisplayServiceClient<Box<dyn ReadWrite>>,
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
    pub async fn open(adapter: &mut AdapterHandle, rsd: &mut RsdHandshake, video: bool) -> Result<Self> {
        let began = Instant::now();
        let began_ms = now_ms();
        let mut display = DisplayServiceClient::connect_rsd(adapter, rsd)
            .await
            .map_err(|e| anyhow!("display service: {e:?}"))?;

        let audio_udp = adapter.bind_udp(0).await?;
        let video_udp = adapter.bind_udp(0).await?;
        let receiver_ip = adapter.host_ip().to_string();
        let sender_ip = adapter.peer_ip().to_string();

        let call_info = CallInfoBlob {
            call_id: 0,
            client_version: 1,
            device_type: "Mac17,7".into(),
            framework_version: "2205.3.1".into(),
            os_version: "25F71".into(),
            device_name: None,
            audio_device_uid: None,
        };
        let session_id = uuid::Uuid::new_v4();

        let audio_call = uuid::Uuid::new_v4().to_string().to_uppercase();
        let audio_offer = build_screen_audio_offer(&audio_call, &call_info)
            .map_err(|e| anyhow!("audio offer: {e:?}"))?;
        let audio = display
            .start_media_stream(build_start_audio_parameters(
                &receiver_ip,
                audio_udp.local_port(),
                &sender_ip,
                50000,
                audio_offer,
                CLIENT_SUPPORTED_FEATURES,
                session_id,
            ))
            .await;
        let mut streams = Vec::new();
        let mut video = video;
        match audio {
            Ok(_) => streams.push(audio_udp),
            // CoreDevice 9022 "The device microphone or camera is currently
            // in use" (a call, a recording): the phone refuses the audio
            // stream, so the video stream alone sets up the session.
            Err(e) if format!("{e:?}").contains("Integer(9022)") => video = true,
            Err(e) => return Err(anyhow!("start audio stream: {e:?}")),
        }
        if video {
            let video_call = uuid::Uuid::new_v4().to_string().to_uppercase();
            let ssrc = uuid::Uuid::new_v4().as_u128() as u32;
            let video_offer = build_screen_video_offer(&video_call, &call_info, ssrc)
                .map_err(|e| anyhow!("video offer: {e:?}"))?;
            display
                .start_media_stream(build_start_video_parameters(
                    &receiver_ip,
                    video_udp.local_port(),
                    &sender_ip,
                    50001,
                    video_offer,
                    CLIENT_SUPPORTED_FEATURES,
                    1,
                    session_id,
                ))
                .await
                .map_err(|e| anyhow!("start video stream: {e:?}"))?;
            streams.push(video_udp);
        }

        let drain = Drain::start(streams);

        // Only the touch connection now: every HID connection makes its own
        // five devices on the phone (a touchscreen, a keyboard, buttons...),
        // one after another, and a touch waits until the phone has opened
        // them. The keyboard and the buttons connect when first used.
        let mut hid = UniversalHidServiceClient::connect_rsd(adapter, rsd)
            .await
            .map_err(|e| anyhow!("hid service: {e:?}"))?;
        let hid_ready = devices_ready(&mut hid).await;
        // A "finger up" with no finger down does nothing on the phone, but it
        // is a first report, and the first report on a new connection is
        // sometimes lost, taking a whole gesture with it. Measured with a new
        // session for every scroll: 7 of 40 first drags lost without this,
        // 0 of 60 with it (iOS 27.0.1). `IPH_INPUT_PRIME=0` turns it off.
        let prime = std::env::var("IPH_INPUT_PRIME").map(|v| v != "0").unwrap_or(true);
        if prime {
            let _ = hid.send_touchscreen(TOUCHSCREEN_STATE_RELEASE, 65535, 65535, None).await;
        }
        tokio::time::sleep(warmup()).await;
        // Touches only count while the phone is sending the stream. It has
        // usually begun by now; when it has not, wait for it a little.
        let waiting = Instant::now();
        while !drain.flowing() && waiting.elapsed() < FIRST_PACKET {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        crate::device::ax::trace(|| {
            let firsts: Vec<String> = drain
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
        if prime {
            let _ = hid.send_touchscreen(TOUCHSCREEN_STATE_RELEASE, 65535, 65535, None).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
        }

        Ok(Self {
            display,
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
        })
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

    async fn contact(&mut self, x: u16, y: u16) -> Result<()> {
        crate::device::ax::trace(|| format!("contact {x},{y}"));
        self.hid
            .send_touchscreen(TOUCHSCREEN_STATE_CONTACT, x, y, None)
            .await
            .map_err(|e| anyhow!("touch contact: {e:?}"))?;
        self.reports += 1;
        Ok(())
    }

    async fn release(&mut self, x: u16, y: u16) -> Result<()> {
        let r = self
            .hid
            .send_touchscreen(TOUCHSCREEN_STATE_RELEASE, x, y, None)
            .await
            .map_err(|e| anyhow!("touch release: {e:?}"));
        crate::device::ax::trace(|| "touch released".into());
        self.last_release = Some(Instant::now());
        if r.is_ok() {
            self.reports += 1;
        }
        r
    }

    /// One finger down at (x, y), held for `hold`, then lifted. Coordinates are
    /// normalized 0..=65535.
    pub async fn tap(&mut self, x: u16, y: u16, hold: Duration) -> Result<()> {
        self.gap().await;
        crate::device::ax::trace(|| format!("tap down {x},{y}"));
        self.contact(x, y).await?;
        tokio::time::sleep(hold).await;
        self.release(x, y).await
    }

    pub async fn double_tap(&mut self, x: u16, y: u16) -> Result<()> {
        self.gap().await;
        for i in 0..2 {
            self.contact(x, y).await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.release(x, y).await?;
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
        from: (u16, u16),
        to: (u16, u16),
        duration: Duration,
        motion: Motion,
    ) -> Result<()> {
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
                self.contact(x, y).await?;
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
                    self.contact(to.0, to.1).await?;
                }
            }
            Ok(())
        }
        .await;
        if sent_any {
            // Always lift, also after a failed sample.
            let lifted = self.release(last.0, last.1).await;
            res?;
            lifted?;
        } else {
            res?;
        }
        Ok(())
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
            let ready = devices_ready(&mut self.hid).await;
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
            let ready = devices_ready(&mut keyboard).await;
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
        let _ = self.keyboard_detach().await;
        self.settle_previous().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.display.stop_media_stream()).await;
    }

    /// Stop the stream of a session given up after an error, without the
    /// orderly wait. A stream nobody stops keeps the phone encoding video.
    pub async fn abandon(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.display.stop_media_stream()).await;
    }
}
