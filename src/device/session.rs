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

const APP_SERVICE: &str = "com.apple.coredevice.appservice";

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
/// How long to wait for the answer to a request before asking again. Every
/// request restarts the phone's wait, so this must be generous: answers took
/// up to 5.8 s while a list was still settling.
const ASK_AGAIN: Duration = Duration::from_millis(6500);
/// The phone never answers a request sooner than this (about 1.1 s typical).
const ANSWER_AT_LEAST: Duration = Duration::from_millis(700);
const READ_TIMEOUT: Duration = Duration::from_secs(14);
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
    /// Logical screen size from the last complete reading.
    screen_size: Option<(f64, f64)>,
    /// pid → bundle id of processes seen in front (None = not an installed app).
    bundles: std::collections::HashMap<i64, Option<String>>,
    /// Counts tunnel (re)connections, for `iph doctor` and logs.
    pub connects: u64,
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

#[cfg(test)]
mod tests {
    use super::plain_path;

    #[test]
    fn paths_compare_across_spellings() {
        assert_eq!(
            plain_path("file:///private/var/containers/Bundle/Application/AB/My%20App.app/"),
            "/var/containers/Bundle/Application/AB/My App.app"
        );
        assert_eq!(plain_path("/Applications/Preferences.app"), "/Applications/Preferences.app");
        assert_eq!(plain_path("file:///Applications/%EB%A9%94%EB%AA%A8.app/x"), "/Applications/메모.app/x");
    }
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
            screen_size: None,
            bundles: std::collections::HashMap::new(),
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

    /// One reading of the screen as the raw dump plus its sequence number.
    /// `full` waits for the second pass (identifiers).
    pub async fn dump(&mut self, fresh: Fresh, full: bool) -> Res<(u64, Arc<Dump>)> {
        let started = Instant::now();
        let mut reconnects = 0;
        let mut fresh = fresh;
        loop {
            self.ensure_ax().await?;
            let ax = self.ax.as_mut().unwrap();
            let cached = match fresh {
                Fresh::MaxAge(max) => ax
                    .latest()
                    .filter(|(_, _, age)| *age <= max)
                    .map(|(seq, dump, _)| (seq, dump)),
                Fresh::Force => None,
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
                let (empty, unparsed) = self
                    .ax
                    .as_ref()
                    .map(|a| (a.empty_count(), a.unparsed_count()))
                    .unwrap_or((0, 0));
                return Err(Fail::not_sent(
                    "ax_timeout",
                    format!(
                        "no accessibility tree within {} ms ({empty} empty, {unparsed} not understood)",
                        started.elapsed().as_millis(),
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
                    // A newer reading replaced this one: take that instead.
                    if ax.seq() > seq && started.elapsed() < READ_TIMEOUT {
                        fresh = Fresh::MaxAge(Duration::from_secs(5));
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
        let mut fresh = fresh;
        let mut last = None;
        // A reading taken while an app is coming to the front can lack the
        // application's frame; look again rather than hand out a screen
        // without a size.
        for _ in 0..3 {
            let (seq, dump) = self.dump(fresh, full).await?;
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
                match bounded("input session", 20, InputSession::open(&mut adapter, &mut tunnel.rsd)).await {
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

    /// Bundle id of the app running as `pid`, if it is an installed app.
    pub async fn bundle_for_pid(&mut self, pid: i64) -> Option<String> {
        if let Some(known) = self.bundles.get(&pid) {
            return known.clone();
        }
        let mut svc = self.app_service().await.ok()?;
        let processes = tokio::time::timeout(Duration::from_secs(10), svc.list_processes())
            .await
            .ok()?
            .ok()?;
        let exe = processes
            .iter()
            .find(|p| p.pid as i64 == pid)
            .and_then(|p| p.executable_url.as_ref())
            .map(|u| plain_path(&u.relative))?;
        let apps = self.list_apps(false).await.ok()?;
        let found = apps
            .iter()
            .filter(|a| exe.starts_with(&format!("{}/", plain_path(&a.path))))
            .max_by_key(|a| a.path.len())
            .map(|a| a.bundle_identifier.clone());
        self.bundles.insert(pid, found.clone());
        found
    }

    /// Refuse input when `only_apps` is set and something else is in front.
    pub async fn check_front_allowed(&mut self, screen: &Screen) -> Res<()> {
        if self.config.safety.only_apps.is_empty() {
            return Ok(());
        }
        let allowed = self.config.safety.only_apps.clone();
        for app in &screen.apps {
            let bundle = self.bundle_for_pid(app.pid).await;
            if !bundle.as_ref().is_some_and(|b| allowed.contains(b)) {
                return Err(Fail::blocked(
                    "app_not_allowed",
                    format!(
                        "the app in front ({}) is not in safety.only_apps",
                        bundle.clone().or_else(|| app.name.clone()).unwrap_or_else(|| "unknown".into())
                    ),
                )
                .hint(t(
                    "config.toml의 [safety] only_apps에 있는 앱만 조작합니다. `iph launch <허용된 앱>`으로 돌아가거나 목록을 고치세요.",
                    "Only apps listed in [safety] only_apps in config.toml are operated. Go back with `iph launch <allowed app>` or change the list.",
                ))
                .with("front_app", serde_json::json!({ "name": app.name, "bundle_id": bundle, "pid": app.pid })));
            }
        }
        if screen.apps.is_empty() {
            return Err(Fail::blocked("app_not_allowed", "cannot tell which app is in front"));
        }
        Ok(())
    }

    /// Refuse to launch an app outside `only_apps`.
    pub fn check_launch_allowed(&self, bundle: &str) -> Res<()> {
        let allowed = &self.config.safety.only_apps;
        if allowed.is_empty() || allowed.iter().any(|b| b == bundle) {
            return Ok(());
        }
        Err(Fail::blocked(
            "app_not_allowed",
            format!("{bundle} is not in safety.only_apps"),
        ))
    }

    pub async fn screenshot_png(&mut self) -> Res<Vec<u8>> {
        let mut svc: ScreenCaptureServiceClient<Box<dyn ReadWrite>> = self.rsd_client().await?;
        bounded("screenshot", 20, svc.take_screenshot(None, ImageFormat::Png))
            .await
            .map_err(|e| Fail::not_sent("screenshot_failed", e))
    }
}
