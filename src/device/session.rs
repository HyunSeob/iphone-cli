//! Everything the daemon holds open for one phone: the tunnel, the
//! accessibility subscription and the touch-input session.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use idevice::{
    ReadWrite, RsdService,
    core_device::{
        AppListEntry, AppServiceClient, ImageFormat, PasteboardServiceClient,
        ScreenCaptureServiceClient,
    },
};

use crate::{
    ax::{
        dump::Dump,
        tree::{BuildOptions, Screen},
    },
    config::Config,
    device::{
        ax::{ANSWER_AT_LEAST, AxSub},
        conn::{self, Tunnel},
        input::InputSession,
    },
    i18n::t,
    reply::{Fail, Res},
};

const APP_SERVICE: &str = "com.apple.coredevice.appservice";

/// One touch or button gesture, in normalized HID coordinates.
#[derive(Debug, Clone, Copy)]
pub enum Gesture {
    Tap { x: u16, y: u16, hold: Duration },
    DoubleTap { x: u16, y: u16 },
    Drag { from: (u16, u16), to: (u16, u16), duration: Duration, motion: crate::device::input::Motion },
    Button { page: u64, usage: u64, hold: Duration },
}

/// How fresh a reading must be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fresh {
    /// Ask the phone for a new dump and wait for it.
    Force,
    /// Accept the latest dump if it is at most this old.
    MaxAge(Duration),
}

/// A cached reading younger than this is reused when a command follows
/// another one closely (scripts, batches).
pub const REUSE: Duration = Duration::from_millis(1500);

/// `REUSE`, or `IPH_REUSE_MS` when set (for experiments).
pub fn reuse() -> Duration {
    std::env::var("IPH_REUSE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(REUSE)
}
/// How long to wait for one dump before asking again. Dumps requested during
/// an animation sometimes never come.
/// How long to wait for the answer to a request before asking again. Every
/// request restarts the phone's wait, so this must be generous: answers took
/// up to 5.8 s while a list was still settling.
const ASK_AGAIN: Duration = Duration::from_millis(6500);
const READ_TIMEOUT: Duration = Duration::from_secs(14);
/// The phone's accessibility daemon is not ended more often than this.
const AX_RESTART_GAP: Duration = Duration::from_secs(120);
/// Shortest time between the end of one input session and the start of the
/// next.
const REOPEN_GAP: Duration = Duration::from_millis(2000);
const SECOND_PASS_WAIT: Duration = Duration::from_millis(2500);

/// How the phone's accessibility daemon failed a reading, when that calls
/// for ending it so the phone starts a new one.
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) enum AxStall {
    /// Not a word on a connection that is up.
    Silent,
    /// Only answers that carry nothing (no application, no element). That is
    /// how it stayed for 25 minutes on 2026-11-11 (every reading
    /// `ax_timeout`, three empty answers each) while the phone was unlocked
    /// and showing Settings; a new daemon answered at once.
    EmptyOnly,
}

/// `empty` and `unparsed`: answers of each kind during the failed reading.
/// Answers that were not understood point at the parser, not the daemon.
pub(crate) fn ax_stall(empty: u64, unparsed: u64) -> Option<AxStall> {
    match (empty, unparsed) {
        (0, 0) => Some(AxStall::Silent),
        (_, 0) => Some(AxStall::EmptyOnly),
        _ => None,
    }
}

pub struct Session {
    pub udid: String,
    pub config: Config,
    pub tunnel: Option<Tunnel>,
    pub ax: Option<AxSub>,
    pub input: Option<InputSession>,
    pub(crate) input_used: Instant,
    /// When the last input session ended, to keep the next one apart from it.
    input_closed_at: Option<Instant>,
    /// A system panel came up over the app after the last tap (a Face ID
    /// prompt, for one). Scrolls sent on the same input session right after
    /// that moved nothing, with no error (seen once, 2026-11-11), so the next
    /// gesture starts a new session.
    pub(crate) input_suspect: bool,
    /// Input sessions given up after an error, each with the tunnel it ran on
    /// when that was given up too. Their streams are stopped at the next
    /// quiet moment (`sweep`).
    abandoned: Vec<(InputSession, Option<Tunnel>)>,
    apps: Option<Vec<AppListEntry>>,
    /// Logical screen size from the last complete reading.
    screen_size: Option<(f64, f64)>,
    /// Readings that arrived before this moment no longer describe the
    /// screen (something changed it after they were taken).
    stale_before: Option<Instant>,
    /// Counts tunnel (re)connections, for `iph doctor` and logs.
    pub connects: u64,
    /// How often, and when last, the phone's accessibility daemon was ended
    /// because it had stopped answering.
    pub ax_restarts: u64,
    ax_restarted_at: Option<Instant>,
    /// Do Not Disturb: whether it is on for this session and what to put back.
    pub dnd: crate::dnd::State,
}

/// A file URL or path as a plain path without a trailing slash.
fn plain_path(s: &str) -> String {
    let s = s.strip_prefix("file://").unwrap_or(s);
    let raw = s.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            let hex = std::str::from_utf8(&raw[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                bytes.push(v);
                i += 3;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let mut path = String::from_utf8_lossy(&bytes).into_owned();
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    // The same app is reported under /private/var and /var.
    path.strip_prefix("/private").map(String::from).unwrap_or(path)
}

fn is_transport_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "adapter closed",
        "broken pipe",
        "channel recv timeout",
        "connection reset",
        "not connected",
        "timed out",
        "closed",
        "unreachable",
        "eof",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

/// Bound a device call that has no time limit of its own.
async fn bounded<T, E: std::fmt::Debug>(
    what: &str,
    secs: u64,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{what}: {e:?}")),
        Err(_) => Err(format!("{what}: no answer within {secs} s")),
    }
}

impl Session {
    pub fn new(udid: String, config: Config) -> Self {
        let dnd = crate::dnd::State::load(&udid);
        Self {
            udid,
            config,
            tunnel: None,
            ax: None,
            input: None,
            input_used: Instant::now(),
            input_closed_at: None,
            input_suspect: false,
            abandoned: Vec::new(),
            apps: None,
            screen_size: None,
            stale_before: None,
            connects: 0,
            ax_restarts: 0,
            ax_restarted_at: None,
            dnd,
        }
    }

    /// Mark every reading taken so far as out of date.
    pub fn invalidate_readings(&mut self) {
        self.stale_before = Some(Instant::now());
    }

    /// Port of an RSD service, for connections made outside `&mut self`.
    pub async fn service_port(&mut self, name: &str) -> Res<(idevice::tcp::handle::AdapterHandle, u16)> {
        let tunnel = self.ensure_tunnel().await?;
        let port = tunnel
            .rsd
            .services
            .get(name)
            .map(|s| s.port)
            .ok_or_else(|| Fail::not_sent("service_unavailable", format!("{name} is not on offer")))?;
        Ok((tunnel.adapter.clone(), port))
    }

    /// Abandon the input session without the orderly close (after an error).
    pub fn drop_input(&mut self) {
        if let Some(input) = self.input.take() {
            self.input_closed_at = Some(Instant::now());
            self.abandoned.push((input, None));
        }
    }

    /// Drop every connection. The next request builds them again.
    pub fn reset(&mut self) {
        let tunnel = self.tunnel.take();
        if let Some(input) = self.input.take() {
            self.input_closed_at = Some(Instant::now());
            // The tunnel stays until the stream has been told to stop.
            self.abandoned.push((input, tunnel));
        }
        self.ax = None;
    }

    /// Tell the phone to stop the streams of abandoned input sessions. Left
    /// alone they keep the phone encoding, and after a row of timeouts
    /// (2026-11-11, with the Mac overloaded) the phone answered ever slower.
    pub async fn sweep(&mut self) {
        for (input, tunnel) in std::mem::take(&mut self.abandoned) {
            input.abandon().await;
            drop(tunnel);
        }
    }

    pub fn connection(&self) -> Option<&str> {
        self.tunnel.as_ref().map(|t| t.connection.as_str())
    }

    pub async fn ensure_tunnel(&mut self) -> Res<&mut Tunnel> {
        if self.tunnel.is_none() {
            // One tunnel at a time: let go of an abandoned one first.
            self.sweep().await;
            let tunnel = conn::open_tunnel(&self.udid).await.map_err(|e| {
                Fail::not_sent("tunnel_failed", format!("{e:#}")).hint(t(
                    "폰이 같은 Wi-Fi에 있거나 USB로 연결되어 있고 잠금이 풀려 있는지 확인하세요. `iph doctor`로 점검할 수 있습니다.",
                    "Check that the phone is on the same Wi-Fi or plugged in, and unlocked. Run `iph doctor`.",
                ))
            })?;
            self.connects += 1;
            self.tunnel = Some(tunnel);
        }
        Ok(self.tunnel.as_mut().unwrap())
    }

    async fn ensure_ax(&mut self) -> Res<()> {
        if self.ax.as_ref().is_some_and(|a| a.is_closed()) {
            // The accessibility connection died; the tunnel may be gone too.
            self.reset();
        }
        if self.ax.is_none() {
            let mut last = None;
            for attempt in 0..2 {
                let tunnel = self.ensure_tunnel().await?;
                let mut adapter = tunnel.adapter.clone();
                match bounded("accessibility service", 12, AxSub::open(&mut adapter, &tunnel.rsd)).await {
                    Ok(ax) => {
                        self.ax = Some(ax);
                        return Ok(());
                    }
                    Err(e) => {
                        last = Some(e);
                        if attempt == 0 {
                            self.reset();
                        }
                    }
                }
            }
            return Err(Fail::not_sent(
                "ax_unavailable",
                last.unwrap_or_else(|| "accessibility service unavailable".into()),
            )
            .hint(t(
                "개발자 모드가 켜져 있고 개발자 디스크 이미지가 연결되어 있어야 합니다. `iph doctor`로 확인하세요.",
                "Developer Mode must be on and the developer disk image mounted. Run `iph doctor`.",
            )));
        }
        Ok(())
    }

    /// End the phone's accessibility daemon (`axauditd`) so the phone starts
    /// a new one. Returns the pid that was ended.
    ///
    /// The daemon sometimes goes silent: it runs and takes new connections
    /// but answers no request, and neither a new tunnel nor another route
    /// changes that (2026-11-11, twice, both after minutes of work over
    /// Wi-Fi; the cause is not known). The new one answers at once.
    async fn restart_ax_daemon(&mut self) -> Result<u32, String> {
        let mut svc = self.app_service().await.map_err(|f| f.message)?;
        let processes = bounded("list processes", 10, svc.list_processes()).await?;
        let pid = processes
            .iter()
            .find(|p| {
                p.executable_url
                    .as_ref()
                    .is_some_and(|u| plain_path(&u.relative).ends_with("/axauditd"))
            })
            .map(|p| p.pid)
            .ok_or("axauditd is not among the phone's processes")?;
        // SIGABRT first: the phone then writes a crash report with every
        // thread's backtrace, which is the only record of where the daemon
        // was stuck (Settings > Privacy & Security > Analytics & Improvements
        // > Analytics Data, "axauditd-…"). SIGKILL after it in case the
        // daemon does not go.
        const SIGABRT: u32 = 6;
        const SIGKILL: u32 = 9;
        bounded("signal", 10, svc.send_signal(pid, SIGABRT)).await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let _ = bounded("signal", 5, svc.send_signal(pid, SIGKILL)).await;
        Ok(pid)
    }

    /// One reading of the screen as the raw dump plus its sequence number.
    /// `full` waits for the second pass (identifiers).
    pub async fn dump(&mut self, fresh: Fresh, full: bool) -> Res<(u64, Arc<Dump>)> {
        let mut started = Instant::now();
        let mut restarted = false;
        let mut reconnects = 0;
        let mut second_pass_retries = 0;
        let mut fresh = fresh;
        // The connection's counts of empty and not understood answers when
        // this reading began on it: only what came during the reading tells
        // how the phone is answering now.
        let mut base: Option<(Instant, u64, u64)> = None;
        loop {
            self.ensure_ax().await?;
            let ax = self.ax.as_mut().unwrap();
            if base.is_none_or(|(opened, _, _)| opened != ax.opened) {
                base = Some((ax.opened, ax.empty_count(), ax.unparsed_count()));
            }
            let usable = match (self.stale_before, ax.latest_arrival()) {
                (Some(limit), Some(arrived)) => arrived >= limit,
                _ => true,
            };
            let cached = match fresh {
                Fresh::MaxAge(max) if usable => ax
                    .latest()
                    .filter(|(_, _, age)| *age <= max)
                    .map(|(seq, dump, _)| (seq, dump)),
                _ => None,
            };
            let got = match cached {
                Some(c) => Some(c),
                None => {
                    // A connection that has not delivered yet already has its
                    // answer on the way; asking again would only delay it.
                    let mut asked_at = if ax.latest().is_none() && ax.opened.elapsed() < ASK_AGAIN {
                        Some(ax.opened)
                    } else {
                        ax.request().ok()
                    };
                    let mut got = None;
                    while let Some(at) = asked_at {
                        // Anything that arrives sooner than an answer can
                        // belongs to an earlier request and may predate what
                        // the caller wants to see.
                        if let Some(x) = ax.wait_arrived(at + ANSWER_AT_LEAST, ASK_AGAIN).await {
                            crate::device::ax::trace(|| {
                                let ms = ax
                                    .latest_arrival()
                                    .map(|t| t.saturating_duration_since(at).as_millis())
                                    .unwrap_or(0);
                                format!("answer accepted {ms} ms after the request")
                            });
                            got = Some(x);
                            break;
                        }
                        if ax.is_closed() || started.elapsed() >= READ_TIMEOUT {
                            break;
                        }
                        asked_at = ax.request().ok();
                    }
                    got
                }
            };
            let Some((seq, dump)) = got else {
                if self.ax.as_ref().is_some_and(|a| a.is_closed()) && reconnects < 2 {
                    reconnects += 1;
                    self.reset();
                    continue;
                }
                let (empty, unparsed) = match (self.ax.as_ref(), base) {
                    (Some(a), Some((_, e0, u0))) => {
                        (a.empty_count().saturating_sub(e0), a.unparsed_count().saturating_sub(u0))
                    }
                    _ => (0, 0),
                };
                let up = self.ax.as_ref().is_some_and(|a| !a.is_closed());
                let stall = if up { ax_stall(empty, unparsed) } else { None };
                // Empty answers are also what a locked phone might send; a
                // daemon that is fine is not ended for that.
                let locked = match stall {
                    Some(AxStall::EmptyOnly) => match conn::route(&self.udid).await {
                        Ok(r) => conn::passcode_locked(&*r.provider).await,
                        Err(_) => None,
                    },
                    _ => None,
                };
                let lately = self.ax_restarted_at.is_some_and(|at| at.elapsed() < AX_RESTART_GAP);
                let mut why_not = "";
                if stall.is_some() && locked != Some(true) {
                    if restarted {
                        why_not = "it was already ended once during this reading";
                    } else if lately {
                        why_not = "it was ended less than 2 minutes ago";
                    } else if !self.config.daemon.restart_accessibility {
                        why_not = "[daemon] restart_accessibility = false";
                    }
                }
                if stall.is_some() && locked != Some(true) && why_not.is_empty() {
                    restarted = true;
                    let waited = started.elapsed().as_secs();
                    let how = match stall {
                        Some(AxStall::EmptyOnly) => format!("sent only empty answers ({empty})"),
                        _ => "gave no answer".to_string(),
                    };
                    match self.restart_ax_daemon().await {
                        Ok(pid) => {
                            eprintln!(
                                "[{}] the phone's accessibility daemon {how} for {waited} s; ended it (pid {pid}) so the phone starts a new one",
                                crate::clock::now_utc()
                            );
                            self.ax_restarted_at = Some(Instant::now());
                            self.ax_restarts += 1;
                            self.ax = None;
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            started = Instant::now();
                            continue;
                        }
                        Err(e) => {
                            eprintln!(
                                "[{}] the phone's accessibility daemon {how} for {waited} s and could not be restarted: {e}",
                                crate::clock::now_utc()
                            );
                            why_not = "ending it failed";
                        }
                    }
                }
                let fail = Fail::not_sent(
                    "ax_timeout",
                    format!(
                        "no accessibility tree within {} ms ({empty} empty, {unparsed} not understood)",
                        started.elapsed().as_millis(),
                    ),
                )
                .with("phone_locked", locked.map(serde_json::Value::Bool).unwrap_or(serde_json::Value::Null));
                return Err(if locked == Some(true) {
                    fail.hint(t("폰이 잠겨 있습니다. 잠금을 풀어 주세요.", "The phone is locked. Unlock it."))
                } else if stall.is_some() {
                    fail.with("ax_restart_skipped", why_not).hint(t(
                        "폰의 화면 읽기 서비스(axauditd)가 응답하지 않습니다. 폰이 잠긴 것은 아닙니다. 잠시 뒤 다시 시도하세요. iph가 2분에 한 번까지 그 서비스를 다시 띄웁니다.",
                        "The phone's screen-reading service (axauditd) is not answering. The phone is not locked. Try again shortly; iph restarts that service at most every 2 minutes.",
                    ))
                } else {
                    fail.hint(t(
                        "화면 읽기 응답을 이해하지 못했습니다. `iph read --raw`로 확인하세요.",
                        "The screen-reading answers were not understood. Look with `iph read --raw`.",
                    ))
                });
            };
            if !full {
                return Ok((seq, dump));
            }
            let ax = self.ax.as_mut().unwrap();
            if let Some(merged) = ax.full(seq) {
                return Ok((seq, merged));
            }
            match ax.wait_full(seq, SECOND_PASS_WAIT).await {
                Some(merged) => return Ok((seq, merged)),
                None => {
                    // A newer reading replaced this one: take that instead.
                    if ax.seq() > seq && started.elapsed() < READ_TIMEOUT {
                        fresh = Fresh::MaxAge(Duration::from_secs(5));
                        continue;
                    }
                    // The second pass sometimes stays away (seen right after
                    // an input). Ask once more before going on without
                    // identifiers.
                    if second_pass_retries < 1 && started.elapsed() < READ_TIMEOUT {
                        second_pass_retries += 1;
                        fresh = Fresh::Force;
                        continue;
                    }
                    return Ok((seq, dump));
                }
            }
        }
    }

    pub async fn screen(&mut self, fresh: Fresh, full: bool, opts: BuildOptions) -> Res<(u64, Screen)> {
        let mut fresh = fresh;
        let mut last = None;
        // A reading taken while an app is coming to the front can lack the
        // application's frame; look again rather than hand out a screen
        // without a size.
        for _ in 0..3 {
            let (seq, dump) = Box::pin(self.dump(fresh, full)).await?;
            let screen = Screen::build(&dump, opts);
            if screen.width > 0.0 && screen.height > 0.0 {
                self.screen_size = Some((screen.width, screen.height));
                return Ok((seq, screen));
            }
            last = Some((seq, screen));
            fresh = Fresh::Force;
        }
        let (seq, mut screen) = last.unwrap();
        if let Some((w, h)) = self.screen_size {
            screen.width = w;
            screen.height = h;
        }
        Ok((seq, screen))
    }

    /// Open the input session if needed. Fails with `not_sent`: nothing has
    /// been written to the phone when this returns an error.
    pub async fn ensure_input(&mut self) -> Res<&mut InputSession> {
        // A media stream that stopped delivering packets no longer holds the
        // phone's HID gate open.
        let stale = self
            .input
            .as_ref()
            .is_some_and(|i| i.media_idle_ms().is_none_or(|ms| ms > 2500));
        if (stale || self.input_suspect)
            && let Some(old) = self.input.take() {
                old.close().await;
                self.input_closed_at = Some(Instant::now());
            }
        self.input_suspect = false;
        if self.input.is_none() {
            self.sweep().await;
            // Keep a new session a little apart from the one before it.
            // (Lost first gestures turned out to be touches sent before the
            // phone had opened the session's devices, which
            // `InputSession::open` now waits for; the gap stays as a margin.
            // `IPH_REOPEN_GAP_MS` overrides it.)
            if let Some(closed) = self.input_closed_at {
                let gap = std::env::var("IPH_REOPEN_GAP_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .map(Duration::from_millis)
                    .unwrap_or(REOPEN_GAP);
                let since = closed.elapsed();
                if since < gap {
                    tokio::time::sleep(gap - since).await;
                }
            }
            let mut last = String::new();
            for attempt in 0..2 {
                let video = self.config.daemon.screen_video;
                let tunnel = self.ensure_tunnel().await?;
                let mut adapter = tunnel.adapter.clone();
                match bounded("input session", 20, InputSession::open(&mut adapter, &mut tunnel.rsd, video)).await {
                    Ok(s) => {
                        self.input = Some(s);
                        break;
                    }
                    Err(e) => {
                        last = e;
                        if attempt == 0 && is_transport_error(&last) {
                            self.reset();
                        } else {
                            break;
                        }
                    }
                }
            }
            if self.input.is_none() {
                return Err(Fail::not_sent("input_unavailable", last).hint(t(
                    "터치 입력에는 개발자 디스크 이미지의 display service가 필요합니다. `iph doctor`로 확인하세요.",
                    "Touch input needs the display service from the developer disk image. Run `iph doctor`.",
                )));
            }
        }
        self.input_used = Instant::now();
        Ok(self.input.as_mut().unwrap())
    }

    /// Send one gesture. The phone sometimes closes the input connections on
    /// its own (it answers the next report with a reset, seen 2026-11-11);
    /// iph only finds out when it writes. When that write fails before a
    /// single report went out, nothing reached the phone, so a new session is
    /// opened and the gesture is sent once more. A gesture that failed
    /// halfway is never repeated.
    pub async fn send_gesture(&mut self, gesture: Gesture) -> Res<()> {
        for attempt in 0..2 {
            let input = self.ensure_input().await?;
            let before = input.reports_sent();
            let sent = match gesture {
                Gesture::Tap { x, y, hold } => input.tap(x, y, hold).await,
                Gesture::DoubleTap { x, y } => input.double_tap(x, y).await,
                Gesture::Drag { from, to, duration, motion } => input.drag(from, to, duration, motion).await,
                Gesture::Button { page, usage, hold } => input.button(page, usage, hold).await,
            };
            let Err(e) = sent else {
                return Ok(());
            };
            let untouched = input.reports_sent() == before;
            self.drop_input();
            if attempt == 0 && untouched {
                crate::device::ax::trace(|| format!("gesture not sent ({e:#}); trying once on a new session"));
                eprintln!("[{}] input connection was closed ({e:#}); sending again on a new session", crate::clock::now_utc());
                continue;
            }
            return Err(Fail::unknown("input_failed", format!("{e:#}")).hint(t(
                "입력이 나갔을 수 있습니다. 다시 보내지 말고 `iph read`로 화면을 확인하세요.",
                "The input may have gone out. Do not resend; check the screen with `iph read`.",
            )));
        }
        unreachable!("the loop returns on its second pass")
    }

    /// Close the input session when it has not been used for a while; the
    /// media stream keeps the phone encoding video.
    pub async fn close_idle_input(&mut self) {
        let idle = Duration::from_secs(self.config.daemon.input_idle_secs);
        if self.input.is_some() && self.input_used.elapsed() > idle
            && let Some(s) = self.input.take() {
                s.close().await;
                self.input_closed_at = Some(Instant::now());
            }
    }

    /// End the input session now.
    pub async fn close_input(&mut self) {
        if let Some(s) = self.input.take() {
            s.close().await;
            self.input_closed_at = Some(Instant::now());
        }
    }

    /// With `input_idle_secs = 0` the input session ends with each command,
    /// so the phone shows "screen sharing" only while input is being sent.
    /// For apps that hide their content or complain while the screen is
    /// shared. Every input then pays for a new session (3 to 4 s).
    pub async fn close_input_if_unheld(&mut self) {
        if self.config.daemon.input_idle_secs == 0
            && let Some(s) = self.input.take() {
                s.close().await;
                self.input_closed_at = Some(Instant::now());
            }
    }

    pub async fn shutdown(&mut self) {
        if let Some(s) = self.input.take() {
            s.close().await;
        }
        self.sweep().await;
        self.reset();
    }

    async fn rsd_client<T>(&mut self) -> Res<T>
    where
        T: RsdService,
    {
        let mut last = String::new();
        for attempt in 0..2 {
            let tunnel = self.ensure_tunnel().await?;
            let mut adapter = tunnel.adapter.clone();
            match bounded("connect", 12, T::connect_rsd(&mut adapter, &mut tunnel.rsd)).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    last = e;
                    if attempt == 0 && is_transport_error(&last) {
                        self.reset();
                    } else {
                        break;
                    }
                }
            }
        }
        Err(Fail::not_sent(
            "service_unavailable",
            format!("{}: {last}", T::rsd_service_name()),
        ))
    }

    pub async fn app_service(&mut self) -> Res<AppServiceClient<Box<dyn ReadWrite>>> {
        self.rsd_client().await
    }

    pub async fn pasteboard(&mut self) -> Res<PasteboardServiceClient<Box<dyn ReadWrite>>> {
        self.rsd_client().await
    }

    pub async fn list_apps(&mut self, refresh: bool) -> Res<Vec<AppListEntry>> {
        if refresh || self.apps.is_none() {
            // The plain `listapps` call gets no answer on iOS 27; the phone
            // advertises a streaming list instead. Use whichever is on offer.
            let streams = self.ensure_tunnel().await?.rsd.services.get(APP_SERVICE).is_some_and(|svc| {
                svc.features
                    .as_ref()
                    .is_some_and(|f| f.iter().any(|x| x == "com.apple.coredevice.feature.streamapplist"))
            });
            let mut svc = self.app_service().await?;
            let apps: Vec<AppListEntry> = if streams {
                use futures_util::TryStreamExt;
                bounded(
                    "stream app list",
                    30,
                    svc.stream_apps(true, true, false, false, true).try_collect(),
                )
                .await
            } else {
                bounded("list apps", 30, svc.list_apps(true, true, false, false, true)).await
            }
            .map_err(|e| Fail::not_sent("apps_failed", e))?;
            self.apps = Some(apps);
        }
        Ok(self.apps.clone().unwrap_or_default())
    }

    pub async fn screenshot_png(&mut self) -> Res<Vec<u8>> {
        let mut svc: ScreenCaptureServiceClient<Box<dyn ReadWrite>> = self.rsd_client().await?;
        bounded("screenshot", 20, svc.take_screenshot(None, ImageFormat::Png))
            .await
            .map_err(|e| Fail::not_sent("screenshot_failed", e))
    }
}

#[cfg(test)]
mod tests {
    use super::{AxStall, ax_stall, plain_path};

    #[test]
    fn paths_compare_across_spellings() {
        assert_eq!(
            plain_path("file:///private/var/containers/Bundle/Application/AB/My%20App.app/"),
            "/var/containers/Bundle/Application/AB/My App.app"
        );
        assert_eq!(plain_path("/Applications/Preferences.app"), "/Applications/Preferences.app");
        assert_eq!(plain_path("file:///Applications/%EB%A9%94%EB%AA%A8.app/x"), "/Applications/메모.app/x");
    }

    #[test]
    fn a_daemon_that_sends_only_empty_answers_is_restarted_too() {
        // Before: only a silent daemon was ended (`empty == 0 && unparsed == 0`),
        // and one that sent three empty answers a reading stayed stuck.
        assert_eq!(ax_stall(0, 0), Some(AxStall::Silent));
        assert_eq!(ax_stall(3, 0), Some(AxStall::EmptyOnly));
        // Answers the parser could not read are not the daemon's fault.
        assert_eq!(ax_stall(0, 1), None);
        assert_eq!(ax_stall(2, 1), None);
    }
}
