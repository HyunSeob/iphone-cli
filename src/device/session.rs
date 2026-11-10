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
        ax::AxSub,
        conn::{self, Tunnel},
        input::InputSession,
    },
    i18n::t,
    reply::{Fail, Res},
    safety::Guard,
};

/// How fresh a reading must be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fresh {
    /// Ask the phone for a new dump and wait for it.
    Force,
    /// Accept the latest dump if it is at most this old.
    MaxAge(Duration),
}

/// A cached reading younger than this is reused when a command follows
/// another one closely (recipes, batches).
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
const ASK_AGAIN: Duration = Duration::from_millis(2500);
const READ_TIMEOUT: Duration = Duration::from_secs(9);
const SECOND_PASS_WAIT: Duration = Duration::from_millis(1500);

pub struct Session {
    pub udid: String,
    pub config: Config,
    pub guard: Guard,
    pub tunnel: Option<Tunnel>,
    pub ax: Option<AxSub>,
    pub input: Option<InputSession>,
    pub(crate) input_used: Instant,
    apps: Option<Vec<AppListEntry>>,
    /// Counts tunnel (re)connections, for `iph doctor` and logs.
    pub connects: u64,
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

impl Session {
    pub fn new(udid: String, config: Config) -> Self {
        let guard = Guard::new(&config.safety);
        Self {
            udid,
            config,
            guard,
            tunnel: None,
            ax: None,
            input: None,
            input_used: Instant::now(),
            apps: None,
            connects: 0,
        }
    }

    /// Drop every connection. The next request builds them again.
    pub fn reset(&mut self) {
        self.input = None;
        self.ax = None;
        self.tunnel = None;
    }

    pub fn connection(&self) -> Option<&str> {
        self.tunnel.as_ref().map(|t| t.connection.as_str())
    }

    pub async fn ensure_tunnel(&mut self) -> Res<&mut Tunnel> {
        if self.tunnel.is_none() {
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
                match AxSub::open(&mut adapter, &tunnel.rsd).await {
                    Ok(ax) => {
                        self.ax = Some(ax);
                        return Ok(());
                    }
                    Err(e) => {
                        last = Some(format!("{e:#}"));
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

    /// One reading of the screen as the raw dump plus its sequence number.
    /// `full` waits for the second pass (identifiers).
    pub async fn dump(&mut self, fresh: Fresh, full: bool) -> Res<(u64, Arc<Dump>)> {
        let started = Instant::now();
        let mut reconnects = 0;
        let mut force = fresh == Fresh::Force;
        loop {
            self.ensure_ax().await?;
            let ax = self.ax.as_mut().unwrap();
            let cached = match fresh {
                Fresh::MaxAge(max) if !force => ax
                    .latest()
                    .filter(|(_, _, age)| *age <= max)
                    .map(|(seq, dump, _)| (seq, dump)),
                _ => None,
            };
            let got = match cached {
                Some(c) => Some(c),
                None => {
                    let after = ax.seq();
                    // A subscription that has not delivered yet already has a
                    // dump on the way; asking again would only delay it.
                    let mut asked = false;
                    if ax.latest().is_some() {
                        asked = ax.request().is_ok();
                    }
                    let mut got = None;
                    while started.elapsed() < READ_TIMEOUT {
                        if let Some(x) = ax.wait_newer(after, ASK_AGAIN).await {
                            got = Some(x);
                            break;
                        }
                        if ax.is_closed() {
                            break;
                        }
                        asked = ax.request().is_ok() || asked;
                    }
                    let _ = asked;
                    got
                }
            };
            let Some((seq, dump)) = got else {
                if self.ax.as_ref().is_some_and(|a| a.is_closed()) && reconnects < 2 {
                    reconnects += 1;
                    self.reset();
                    continue;
                }
                let empty = self.ax.as_ref().map(|a| a.empty_count()).unwrap_or(0);
                return Err(Fail::not_sent(
                    "ax_timeout",
                    format!(
                        "no accessibility tree within {} ms ({} empty dumps)",
                        started.elapsed().as_millis(),
                        empty
                    ),
                )
                .hint(t(
                    "폰 화면이 켜져 있고 잠금이 풀려 있는지 확인하세요.",
                    "Check that the phone's screen is on and unlocked.",
                )));
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
                    // A newer reading replaced this one: use that instead.
                    if ax.seq() > seq && started.elapsed() < READ_TIMEOUT {
                        force = false;
                        continue;
                    }
                    // No second pass (it does not always come): go on without
                    // identifiers rather than fail.
                    return Ok((seq, dump));
                }
            }
        }
    }

    pub async fn screen(&mut self, fresh: Fresh, full: bool, opts: BuildOptions) -> Res<(u64, Screen)> {
        let (seq, dump) = self.dump(fresh, full).await?;
        Ok((seq, Screen::build(&dump, opts)))
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
        if stale {
            if let Some(old) = self.input.take() {
                old.close().await;
            }
        }
        if self.input.is_none() {
            let mut last = String::new();
            for attempt in 0..2 {
                let tunnel = self.ensure_tunnel().await?;
                let mut adapter = tunnel.adapter.clone();
                match InputSession::open(&mut adapter, &mut tunnel.rsd).await {
                    Ok(s) => {
                        self.input = Some(s);
                        break;
                    }
                    Err(e) => {
                        last = format!("{e:#}");
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

    /// Close the input session when it has not been used for a while; the
    /// media stream keeps the phone encoding video.
    pub async fn close_idle_input(&mut self) {
        let idle = Duration::from_secs(self.config.daemon.input_idle_secs);
        if self.input.is_some() && self.input_used.elapsed() > idle {
            if let Some(s) = self.input.take() {
                s.close().await;
            }
        }
    }

    pub async fn shutdown(&mut self) {
        if let Some(s) = self.input.take() {
            s.close().await;
        }
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
            match T::connect_rsd(&mut adapter, &mut tunnel.rsd).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    last = format!("{e:?}");
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
            let mut svc = self.app_service().await?;
            let apps = svc
                .list_apps(true, true, false, false, true)
                .await
                .map_err(|e| Fail::not_sent("apps_failed", format!("{e:?}")))?;
            self.apps = Some(apps);
        }
        Ok(self.apps.clone().unwrap_or_default())
    }

    pub async fn screenshot_png(&mut self) -> Res<Vec<u8>> {
        let mut svc: ScreenCaptureServiceClient<Box<dyn ReadWrite>> = self.rsd_client().await?;
        svc.take_screenshot(None, ImageFormat::Png)
            .await
            .map_err(|e| Fail::not_sent("screenshot_failed", format!("{e:?}")))
    }
}
