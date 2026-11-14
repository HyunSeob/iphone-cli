//! `iph doctor`: check, in order, everything the phone needs for iph to work,
//! and say in one line what the user must do for each thing that is missing.
//!
//! Which checks are truly required was established on an iPhone 15 Pro Max
//! with iOS 27.0.1; see docs/onboarding.md.

use std::time::Duration;

use idevice::{
    IdeviceService, amfi::AmfiClient, lockdown::LockdownClient,
    mobile_image_mounter::ImageMounter, provider::IdeviceProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    actions::{LaunchArgs, ScrollArgs},
    ax::tree::BuildOptions,
    device::{
        ax, conn, wifi,
        session::{Fresh, Session},
    },
    fields,
    i18n::t,
    reply::Res,
};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DoctorArgs {
    /// Skip the check that sends a real touch (it brings Settings to the front
    /// and scrolls its list down and back).
    pub no_touch_test: bool,
    /// Enable disabled wireless lockdown flags through USB, then look for
    /// the device's usbmuxd Network entry. Ordinary doctor only reads them.
    pub fix_wifi: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub check: &'static str,
    /// "ok" | "fail" | "warn" | "skipped" | "info"
    pub status: &'static str,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_action: Option<String>,
    pub auto_fixed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Check {
    fn new(check: &'static str, required: bool) -> Self {
        Self {
            check,
            status: "ok",
            required,
            user_action: None,
            auto_fixed: false,
            detail: None,
        }
    }

    fn status(mut self, status: &'static str) -> Self {
        self.status = status;
        self
    }

    fn action(mut self, ko: &str, en: &str) -> Self {
        self.user_action = Some(t(ko, en).to_string());
        self
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

const SETTINGS: &str = "com.apple.Preferences";
const NEEDED_SERVICES: &[&str] = &[
    ax::SERVICE,
    "com.apple.coredevice.displayservice",
    "com.apple.coredevice.hid.universalhidservice",
    "com.apple.coredevice.appservice",
];

async fn with_timeout<T>(fut: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::time::timeout(Duration::from_secs(15), fut).await.ok()
}

struct LockdownFacts {
    developer_mode: Option<bool>,
    /// `PasswordProtected`: true while the phone is locked with a passcode.
    locked: Option<bool>,
}

/// What lockdown says about Developer Mode and the lock. Read-only.
async fn lockdown_facts(provider: &dyn IdeviceProvider) -> LockdownFacts {
    let mut facts = LockdownFacts { developer_mode: None, locked: None };
    // The image mounter answers the Developer Mode question most reliably;
    // the AMFI service closed the connection on iOS 27.0.1 over Wi-Fi.
    if let Some(Ok(mut mounter)) = with_timeout(ImageMounter::connect(provider)).await {
        facts.developer_mode = with_timeout(mounter.query_developer_mode_status())
            .await
            .and_then(|r| r.ok());
    }
    if let Some(Ok(mut lockdown)) = with_timeout(LockdownClient::connect(provider)).await {
        if let Some(Ok(pairing)) = with_timeout(provider.get_pairing_file()).await {
            let _ = with_timeout(lockdown.start_session(&pairing)).await;
        }
        if facts.developer_mode.is_none() {
            facts.developer_mode = with_timeout(
                lockdown.get_value(Some("DeveloperModeStatus"), Some("com.apple.security.mac.amfi")),
            )
            .await
            .and_then(|r| r.ok())
            .and_then(|v| v.as_boolean());
        }
    }
    facts.locked = conn::passcode_locked(provider).await;
    facts
}

/// Developer services the tunnel does not offer. `Err` when the tunnel itself
/// cannot be opened.
async fn missing_services(s: &mut Session) -> Result<Vec<&'static str>, String> {
    match s.ensure_tunnel().await {
        Ok(tunnel) => Ok(NEEDED_SERVICES
            .iter()
            .filter(|name| !tunnel.rsd.services.contains_key(**name))
            .copied()
            .collect()),
        Err(f) => Err(f.message),
    }
}

fn hex_field(v: Option<&plist::Value>) -> Option<u64> {
    match v? {
        plist::Value::String(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).ok(),
        plist::Value::Integer(i) => i.as_unsigned(),
        _ => None,
    }
}

/// Mount the personalized developer disk image that Xcode keeps on this Mac.
///
/// Written from the documented flow (match the build identity to the phone's
/// board and chip, have Apple sign it, upload, mount). It could not be tried
/// on the test phone, whose image was already mounted.
async fn mount_ddi(provider: &dyn IdeviceProvider) -> anyhow::Result<()> {
    use anyhow::{Context, anyhow};
    let restore = crate::ddi::xcode_restore()
        .ok_or_else(|| anyhow!("no Xcode developer disk image on this computer"))?;

    let manifest_bytes = std::fs::read(restore.join("BuildManifest.plist"))?;
    let manifest: plist::Value = plist::from_bytes(&manifest_bytes).context("BuildManifest.plist")?;

    let mut lockdown = LockdownClient::connect(provider)
        .await
        .map_err(|e| anyhow!("lockdown: {e:?}"))?;
    if let Ok(pairing) = provider.get_pairing_file().await {
        let _ = lockdown.start_session(&pairing).await;
    }
    let number = |v: plist::Value| v.as_unsigned_integer();
    let board = lockdown.get_value(Some("BoardId"), None).await.ok().and_then(number);
    let chip = lockdown.get_value(Some("ChipID"), None).await.ok().and_then(number);
    let ecid = lockdown
        .get_value(Some("UniqueChipID"), None)
        .await
        .ok()
        .and_then(number)
        .ok_or_else(|| anyhow!("the phone did not report its chip id"))?;

    let identities = manifest
        .as_dictionary()
        .and_then(|d| d.get("BuildIdentities"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("BuildManifest has no BuildIdentities"))?;
    let identity = identities
        .iter()
        .filter_map(|i| i.as_dictionary())
        .find(|i| hex_field(i.get("ApBoardID")) == board && hex_field(i.get("ApChipID")) == chip)
        .ok_or_else(|| anyhow!("no Xcode developer disk image on this computer matches this phone model"))?;
    let path_of = |component: &str| -> anyhow::Result<std::path::PathBuf> {
        identity
            .get("Manifest")
            .and_then(|m| m.as_dictionary())
            .and_then(|m| m.get(component))
            .and_then(|c| c.as_dictionary())
            .and_then(|c| c.get("Info"))
            .and_then(|i| i.as_dictionary())
            .and_then(|i| i.get("Path"))
            .and_then(|p| p.as_string())
            .map(|p| restore.join(p))
            .ok_or_else(|| anyhow!("BuildManifest has no path for {component}"))
    };
    let image = std::fs::read(path_of("PersonalizedDMG")?)?;
    let trust_cache = std::fs::read(path_of("LoadableTrustCache")?)?;

    let mut mounter = ImageMounter::connect(provider)
        .await
        .map_err(|e| anyhow!("image mounter: {e:?}"))?;
    mounter
        .mount_personalized(provider, image, trust_cache, &manifest_bytes, None, ecid)
        .await
        .map_err(|e| anyhow!("mount: {e:?}"))
}

/// The checks for "the phone is unlocked and Settings is in front", and
/// whether Settings is in front.
///
/// `locked`: lockdown's word. `launched`: whether the launch of Settings was
/// seen in front, or why it failed; `None` when not tried. `read`: when it was
/// not seen, a reading taken after: whether Settings is in front on it, or why
/// it failed.
fn front_checks(
    locked: Option<bool>,
    launched: Option<Result<bool, String>>,
    read: Option<Result<bool, String>>,
) -> (Vec<Check>, bool) {
    let mut out = Vec::new();
    if locked == Some(true) {
        out.push(
            Check::new("screen_unlocked", true)
                .status("fail")
                .detail("the phone reports that it is locked")
                .action("폰 잠금을 풀어 주세요.", "Unlock the phone."),
        );
        return (out, false);
    }
    let unlocked = match locked {
        Some(false) => Check::new("screen_unlocked", true).detail("lockdown: not locked"),
        _ => Check::new("screen_unlocked", true).detail("lockdown did not say; Settings came to the front"),
    };
    if launched == Some(Ok(true)) || read == Some(Ok(true)) {
        out.push(unlocked);
        return (out, true);
    }
    match (&launched, &read) {
        (_, Some(Err(why))) => {
            // The screen cannot be read: that is what to report, lock or not.
            out.push(match locked {
                Some(false) => Check::new("screen_unlocked", true).detail("lockdown: not locked"),
                _ => Check::new("screen_unlocked", true)
                    .status("skipped")
                    .detail("lockdown did not say, and the screen cannot be read"),
            });
            out.push(
                Check::new("accessibility_tree", true)
                    .status("fail")
                    .detail(why.clone())
                    .action(
                        "화면 읽기 응답이 없습니다(폰의 화면 읽기 서비스 axauditd). iph가 그 서비스를 다시 띄웠거나 2분 안에 다시 띄웁니다. 잠시 뒤 `iph doctor`를 다시 실행하고, 계속되면 폰을 재시동하세요.",
                        "No answer from the phone's screen reading (its axauditd service). iph has restarted it or will within 2 minutes. Run `iph doctor` again shortly; if it keeps failing, restart the phone.",
                    ),
            );
        }
        (Some(Err(why)), _) => {
            out.push(unlocked);
            out.push(
                Check::new("settings_in_front", true)
                    .status("fail")
                    .detail(why.clone())
                    .action("설정 앱을 열지 못했습니다. 폰 화면을 확인하고 다시 시도하세요.", "Settings could not be opened. Check the phone's screen and try again."),
            );
        }
        _ => {
            // The screen reads, and shows something else.
            out.push(unlocked);
            out.push(
                Check::new("settings_in_front", true)
                    .status("fail")
                    .detail("Settings was launched but another screen is in front")
                    .action(
                        "설정 앱이 앞에 나오지 않았습니다. 다른 창이나 알림이 가리고 있는지 폰 화면을 확인하세요.",
                        "Settings did not come to the front. Check the phone's screen for a window or alert in the way.",
                    ),
            );
        }
    }
    (out, false)
}

fn wifi_check(
    connection: Option<&str>,
    state: Option<&wifi::Wireless>,
    network: Result<bool, String>,
) -> Check {
    let mut c = Check::new("wifi", false);
    let mut detail = Vec::new();
    match &network {
        Ok(true) if connection == Some("usb") => detail.push("reachable over Wi-Fi as well as USB".into()),
        Ok(true) => detail.push("usbmuxd Network entry present".into()),
        Ok(false) => detail.push("no usbmuxd Network entry".into()),
        Err(e) => detail.push(format!("could not check usbmuxd Network entry: {e}")),
    }
    if let Some(state) = state {
        detail.push(state.detail());
        c.auto_fixed = !state.changed.is_empty();
        if state.supported() == Some(false) && network != Ok(true) {
            return c.status("warn").detail(detail.join("; ")).action(
                "기기가 SupportsWifi와 SupportsWifiSyncing을 모두 false로 보고했습니다. 자동으로 무선 설정을 바꾸지 않았습니다. USB 연결을 사용하세요.",
                "The device reports SupportsWifi=false and SupportsWifiSyncing=false. Wireless settings were not changed automatically. Use USB.",
            );
        }
        let disabled = state.disabled();
        if !disabled.is_empty() {
            c = c.status("warn");
            let keys = disabled.join(" / ");
            return c.detail(detail.join("; ")).action(
                &format!("기기의 무선 디버깅/Wi-Fi 연결 플래그({keys})가 꺼져 있습니다. USB 연결 상태에서 `iph setup --fix-wifi` 또는 `iph doctor --fix-wifi`를 실행하세요. 직접 설정하려면 Finder의 'Wi-Fi에 있을 때 이 iPhone 보기'(윈도: Apple 기기 앱)와 Xcode의 'Connect via network'를 확인하세요. 자동 설정 실패 원인은 detail에 표시됩니다."),
                &format!("The device's wireless debugging/Wi-Fi connection flags ({keys}) are off. With USB connected, run `iph setup --fix-wifi` or `iph doctor --fix-wifi`. For manual setup, check Finder's \"Show this iPhone when on Wi-Fi\" (Apple Devices on Windows) and Xcode's \"Connect via network\". Any automatic setup failure is in detail."),
            );
        }
    }
    if network == Ok(true) || (network.is_ok() && state.is_none() && connection == Some("wifi")) {
        return c.detail(detail.join("; "));
    }
    c = c.status("warn").detail(detail.join("; "));
    if network.is_err() {
        return c.action(
            "usbmuxd에서 무선 연결 목록을 확인하지 못했습니다. 컴퓨터의 Apple 기기 연결 서비스와 USB 연결을 확인하고 `iph doctor`를 다시 실행하세요.",
            "Could not query wireless connections from usbmuxd. Check the computer's Apple device service and USB connection, then run `iph doctor` again.",
        );
    }
    match state {
        Some(state) if state.enabled() => c.action(
            "EnableWifiConnections와 EnableWifiDebugging은 모두 켜져 있지만 usbmuxd에 Network 연결이 없습니다. 폰과 컴퓨터가 같은 Wi-Fi 대역에 있는지, 공유기의 AP/클라이언트 격리가 꺼져 있는지, mDNS/Bonjour(_apple-mobdev2._tcp.local.)가 차단되지 않는지 확인하세요. 설정 직후라면 잠시 뒤 `iph doctor`를 다시 실행하세요.",
            "EnableWifiConnections and EnableWifiDebugging are both on, but usbmuxd has no Network connection. Check the same Wi-Fi subnet, AP/client isolation, and whether mDNS/Bonjour (_apple-mobdev2._tcp.local.) is blocked. If just enabled, run `iph doctor` again shortly.",
        ),
        _ => c.action(
            "기기의 무선 플래그를 확인하지 못했습니다(원인은 detail 참고). USB로 연결하고 잠금을 풀어 '이 컴퓨터를 신뢰'를 확인한 뒤 `iph doctor`를 다시 실행하세요. 무선 설정은 Finder/Apple 기기 앱과 Xcode의 'Connect via network'에서도 확인할 수 있습니다.",
            "Could not confirm the device's wireless flags (see detail). Connect by USB, unlock and trust this computer, then run `iph doctor` again. Wireless settings can also be checked in Finder/Apple Devices and Xcode's \"Connect via network\".",
        ),
    }
}

async fn check_wifi(udid: &str, connection: Option<&str>, fix: bool) -> Check {
    let all = match tokio::time::timeout(Duration::from_secs(3), conn::list_raw()).await {
        Ok(Ok(all)) => all,
        Ok(Err(e)) => return wifi_check(connection, None, Err(format!("{e:#}"))),
        Err(_) => return wifi_check(connection, None, Err("ListDevices timed out".into())),
    };
    let mut network = Ok(wifi::has_network(&all, udid));
    let mut state = None;
    if let Some(usb) = wifi::usb_device(&all, udid) {
        let addr = match conn::mux_addr() {
            Ok(addr) => addr,
            Err(e) => return wifi_check(connection, None, Err(format!("{e:#}"))),
        };
        let provider = usb.to_provider(addr, conn::LABEL);
        state = Some(wifi::inspect(&provider, fix).await);
        if fix && network == Ok(false)
            && state.as_ref().is_some_and(|s| s.enabled() || !s.changed.is_empty()) {
            network = wifi::discover(udid).await;
        }
    }
    if connection.is_none() && state.is_none() && network == Ok(false) {
        return Check::new("wifi", false).status("skipped").detail("phone not reachable");
    }
    wifi_check(connection, state.as_ref(), network)
}

pub async fn run(s: &mut Session, args: DoctorArgs) -> Res {
    let mut checks: Vec<Check> = Vec::new();
    let skip = |name: &'static str, required: bool, why: &str| {
        Check::new(name, required).status("skipped").detail(why.to_string())
    };

    // 1. Pairing: the Mac holds a pairing record for this phone.
    let paired = match conn::mux_connect().await {
        Ok(mut mux) => with_timeout(mux.get_pair_record(&s.udid)).await.is_some_and(|r| r.is_ok()),
        Err(_) => false,
    };
    checks.push(if paired {
        Check::new("pairing", true)
    } else {
        Check::new("pairing", true).status("fail").action(
            "USB로 연결하고 폰에서 '이 컴퓨터를 신뢰'를 누르세요.",
            "Connect the phone by USB and tap \"Trust This Computer\" on it.",
        )
    });

    // 2. Reachable at all, and how.
    let route = conn::route(&s.udid).await;
    let connection = route.as_ref().ok().map(|r| r.connection.clone());
    checks.push(match &route {
        Ok(r) => Check::new("connection", true).detail(r.connection.clone()),
        Err(e) => Check::new("connection", true)
            .status("fail")
            .detail(format!("{e:#}"))
            .action(
                "폰을 USB로 연결하거나, 컴퓨터와 같은 Wi-Fi에 두고 화면을 켜 주세요.",
                "Plug the phone in, or put it on the computer's Wi-Fi and wake its screen.",
            ),
    });

    // Inspect (and, with --fix-wifi, configure) the USB transport before
    // developer services: a missing image must not prevent wireless setup.
    let wifi = check_wifi(&s.udid, connection.as_deref(), args.fix_wifi).await;

    // 3. Developer Mode. If it is off, make the switch appear in Settings.
    let facts = match &route {
        Ok(r) => Some(lockdown_facts(&*r.provider).await),
        Err(_) => None,
    };
    let dev_mode = facts.as_ref().and_then(|f| f.developer_mode);
    checks.push(match dev_mode {
        Some(true) => Check::new("developer_mode", true),
        Some(false) => {
            let mut revealed = false;
            if let Ok(r) = &route
                && let Some(Ok(mut amfi)) = with_timeout(AmfiClient::connect(&*r.provider)).await {
                    revealed = with_timeout(amfi.reveal_developer_mode_option_in_ui())
                        .await
                        .is_some_and(|r| r.is_ok());
                }
            let mut c = Check::new("developer_mode", true).status("fail").action(
                "설정 → 개인정보 보호 및 보안 → 개발자 모드 → 켜기 → 재시동 후 비밀번호를 입력하세요.",
                "Settings → Privacy & Security → Developer Mode → turn on → restart, then enter the passcode.",
            );
            c.auto_fixed = revealed;
            if revealed {
                c = c.detail("the Developer Mode switch was made visible in Settings");
            }
            c
        }
        None if route.is_ok() => Check::new("developer_mode", true)
            .status("warn")
            .detail("could not ask the phone; judged by the services below"),
        None => skip("developer_mode", true, "phone not reachable"),
    });

    // 4. Developer disk image: its services must be on offer over the tunnel.
    let mut services_ok = false;
    if route.is_ok() && dev_mode != Some(false) {
        let mut missing = missing_services(s).await;
        let mut mounted_from = None;
        let mut mount_note = None;
        if matches!(&missing, Ok(m) if !m.is_empty()) {
            // Install the image through cryptexd (Xcode's copy, else one
            // fetched once), then look again. Phones before iOS 17.4 take it
            // through the image mounter instead, from Xcode's copy only.
            let cryptex = match s.ensure_tunnel().await {
                Ok(tunnel) => match tokio::time::timeout(Duration::from_secs(300), crate::ddi::install(tunnel)).await {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!("timed out")),
                },
                Err(f) => Err(anyhow::anyhow!(f.message)),
            };
            match cryptex {
                Ok(source) => mounted_from = Some(source),
                Err(e) => {
                    let mut note = format!("cryptex install failed: {e:#}");
                    if let Ok(r) = &route {
                        match tokio::time::timeout(Duration::from_secs(180), mount_ddi(&*r.provider)).await {
                            Ok(Ok(())) => mounted_from = Some("Xcode".to_string()),
                            Ok(Err(e)) => note.push_str(&format!("; image mounter: {e:#}")),
                            Err(_) => note.push_str("; image mounter timed out"),
                        }
                    }
                    if mounted_from.is_none() {
                        mount_note = Some(note);
                    }
                }
            }
            if mounted_from.is_some() {
                s.reset();
                tokio::time::sleep(Duration::from_secs(2)).await;
                missing = missing_services(s).await;
            }
        }
        let mounted_now = mounted_from.is_some();
        match missing {
            Ok(m) if m.is_empty() => {
                services_ok = true;
                let mut c = Check::new("developer_disk_image", true);
                c.auto_fixed = mounted_now;
                if let Some(from) = &mounted_from {
                    c = c.detail(format!("installed the developer disk image ({from})"));
                }
                checks.push(c);
            }
            Ok(m) => {
                let mut detail = format!("missing services: {}", m.join(", "));
                if let Some(note) = mount_note {
                    detail.push_str("; ");
                    detail.push_str(&note);
                }
                checks.push(Check::new("developer_disk_image", true).status("fail").detail(detail).action(
                    "개발자 디스크 이미지를 연결하지 못했습니다. 인터넷 연결(애플 서명 서버, GitHub)을 확인하고 다시 `iph doctor`를 실행하세요. 맥이라면 Xcode의 창 → 기기 및 시뮬레이터에서 이 폰을 한 번 골라도 됩니다.",
                    "Could not install the developer disk image. Check the internet connection (Apple's signing server, GitHub) and run `iph doctor` again. On a Mac, selecting this phone once in Xcode → Window → Devices and Simulators also works.",
                ));
            }
            Err(message) => checks.push(
                Check::new("developer_disk_image", true)
                    .status("fail")
                    .detail(message)
                    .action(
                        "폰 잠금을 풀고 다시 시도하세요. 계속 실패하면 USB로 연결하세요.",
                        "Unlock the phone and try again. If it keeps failing, connect by USB.",
                    ),
            ),
        }
    } else {
        checks.push(skip(
            "developer_disk_image",
            true,
            if route.is_ok() { "Developer Mode is off" } else { "phone not reachable" },
        ));
    }

    // 5. Wi-Fi (recommended): works without the cable.
    checks.push(wifi);

    // 6. Unlocked, and Settings comes to the front. Only lockdown's word
    // makes this a lock: a launch that cannot be seen in front can as well
    // be a screen that cannot be read (2026-11-11: 25 minutes of "unlock the
    // phone" on an unlocked phone whose accessibility daemon answered only
    // with empty readings).
    let locked = facts.as_ref().and_then(|f| f.locked);
    let mut front_ok = false;
    let mut tree_reported = false;
    if locked == Some(true) || services_ok {
        let launched = if locked == Some(true) {
            None
        } else {
            Some(match s.cmd_launch(LaunchArgs { app: SETTINGS.into(), restart: false }).await {
                Ok(r) => Ok((
                    r.get("foreground").and_then(|v| v.as_bool()) == Some(true),
                    r.get("pid").and_then(|v| v.as_i64()),
                )),
                Err(f) => Err(f.message),
            })
        };
        // Not seen in front: is the screen readable at all, and what is on it?
        let read = match &launched {
            None | Some(Ok((true, _))) => None,
            Some(other) => {
                let pid = other.as_ref().ok().and_then(|(_, pid)| *pid);
                Some(match s.screen(Fresh::Force, false, BuildOptions::default()).await {
                    Ok((_, screen)) => Ok(pid.is_some_and(|p| screen.apps.iter().any(|a| a.pid == p))),
                    Err(f) => Err(f.message),
                })
            }
        };
        let launched = launched.map(|l| l.map(|(front, _)| front));
        let (found, front) = front_checks(locked, launched, read);
        tree_reported = found.iter().any(|c| c.check == "accessibility_tree");
        front_ok = front;
        checks.extend(found);
    } else {
        checks.push(skip("screen_unlocked", true, "developer services unavailable"));
    }

    // 7. The accessibility tree arrives.
    let mut tree_ok = false;
    if front_ok {
        match s.screen(Fresh::Force, false, BuildOptions::default()).await {
            Ok((_, screen)) if screen.elements.len() > 1 => {
                tree_ok = true;
                checks.push(
                    Check::new("accessibility_tree", true)
                        .detail(format!("{} elements", screen.elements.len())),
                );
            }
            Ok(_) => checks.push(Check::new("accessibility_tree", true).status("fail").action(
                "폰 화면이 켜져 있는지 확인하고 다시 시도하세요.",
                "Make sure the phone's screen is on and try again.",
            )),
            Err(f) => checks.push(
                Check::new("accessibility_tree", true)
                    .status("fail")
                    .detail(f.message)
                    .action(
                        "폰 화면이 켜져 있는지 확인하고 다시 시도하세요.",
                        "Make sure the phone's screen is on and try again.",
                    ),
            ),
        }
    } else if !tree_reported {
        checks.push(skip("accessibility_tree", true, "Settings is not in front"));
    }

    // 8. Touch input really lands: scroll Settings down one step and back.
    if args.no_touch_test {
        checks.push(skip("touch_input", true, "--no-touch-test"));
    } else if tree_ok {
        // Down and back; if down does nothing (the list may be at its end, or
        // the first drag of a new session was lost), up and back.
        let mut verdict: Option<Check> = None;
        let mut last_detail = String::new();
        for (there, back) in [("down", "up"), ("up", "down")] {
            let moved = s
                .cmd_scroll(ScrollArgs { direction: there.into(), until: None, max: None })
                .await;
            match &moved {
                Ok(r) if r.get("screen_changed").and_then(|v| v.as_bool()) == Some(true) => {
                    // Put the list back where it was.
                    let _ = s
                        .cmd_scroll(ScrollArgs { direction: back.into(), until: None, max: None })
                        .await;
                    let mut check = Check::new("touch_input", true);
                    if there == "up" {
                        check = check.detail("the first drag did not move the screen; the second did");
                    }
                    verdict = Some(check);
                    break;
                }
                Ok(_) => last_detail = "the drags were sent but the screen did not move".to_string(),
                Err(f) => last_detail = f.message.clone(),
            }
        }
        checks.push(verdict.unwrap_or_else(|| {
            Check::new("touch_input", true).status("fail").detail(last_detail).action(
                "설정 → 개발자 → 'UI 자동화 활성화'를 켜고 다시 시도하세요. 그래도 안 되면 폰을 재시동하세요.",
                "Turn on Settings → Developer → Enable UI Automation and try again. If it still fails, restart the phone.",
            )
        }));
    } else {
        checks.push(skip("touch_input", true, "no accessibility tree"));
    }

    // 9. Auto-lock (recommended). Not read from the phone: just advice.
    checks.push(Check::new("auto_lock", false).status("info").action(
        "오래 쓸 때는 설정 → 디스플레이 및 밝기 → 자동 잠금 → '안 함'을 권합니다. 폰이 잠기면 읽기와 입력이 멈춥니다.",
        "For long sessions set Settings → Display & Brightness → Auto-Lock → Never. Reading and input stop when the phone locks.",
    ));

    // 10. Do Not Disturb that `iph dnd on` left on. The phone's Focus is not
    // touched here.
    if let Some(left) = &s.dnd.record {
        checks.push(
            Check::new("do_not_disturb", false)
                .status("info")
                .detail(format!("on since {}, set by iph", left.at))
                .action(
                    "`iph dnd on`으로 켠 방해금지 모드가 켜져 있습니다. `iph dnd off`로 원래대로 되돌릴 수 있습니다.",
                    "Do Not Disturb from `iph dnd on` is on. `iph dnd off` puts the earlier Focus back.",
                ),
        );
    }

    let ok = checks
        .iter()
        .all(|c| !c.required || c.status == "ok");
    let failing: Vec<Value> = checks
        .iter()
        .filter(|c| c.required && c.status != "ok")
        .map(|c| json!(c.check))
        .collect();
    Ok(fields! {
        "ready": ok,
        "udid": s.udid.clone(),
        "connection": connection,
        "failing": failing,
        "checks": checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wireless(values: [Option<bool>; 4]) -> wifi::Wireless {
        wifi::Wireless { values, ..Default::default() }
    }

    #[test]
    fn disabled_wireless_flags_are_named_instead_of_blame_on_the_wifi() {
        for values in [[true, false], [false, true], [false, false]] {
            let state = wireless([Some(true), Some(true), Some(values[0]), Some(values[1])]);
            let c = wifi_check(Some("usb"), Some(&state), Ok(false));
            assert_eq!((c.status, c.required, c.auto_fixed), ("warn", false, false));
            let action = c.user_action.unwrap();
            for (i, key) in ["EnableWifiConnections", "EnableWifiDebugging"].iter().enumerate() {
                if !values[i] { assert!(action.contains(key), "{action}"); }
            }
            assert!(action.contains("iph setup --fix-wifi") && action.contains("iph doctor --fix-wifi"));
            assert!(action.contains("Finder") && action.contains("Connect via network"));
            assert!(!action.contains("AP/") && !action.contains("mDNS"), "{action}");
        }
    }

    #[test]
    fn enabled_flags_without_network_point_to_subnet_isolation_and_mdns() {
        let state = wireless([Some(true); 4]);
        let c = wifi_check(Some("usb"), Some(&state), Ok(false));
        assert_eq!(c.status, "warn");
        let action = c.user_action.unwrap();
        assert!(action.contains("EnableWifiConnections") && action.contains("EnableWifiDebugging"));
        assert!(action.contains("AP/") && action.contains("_apple-mobdev2._tcp.local."));
        let c = wifi_check(Some("usb"), Some(&state), Err("ListDevices timed out".into()));
        assert!(c.detail.unwrap().contains("ListDevices timed out"));
        assert!(!c.user_action.unwrap().contains("AP/"));
    }

    #[test]
    fn unknown_or_unsupported_flags_are_not_called_disabled() {
        let c = wifi_check(Some("usb"), Some(&wireless([None; 4])), Ok(false));
        assert_eq!(c.status, "warn");
        assert!(c.detail.unwrap().contains("EnableWifiDebugging=unknown"));
        let c = wifi_check(Some("usb"), Some(&wireless([Some(false); 4])), Ok(false));
        assert!(c.user_action.unwrap().contains("SupportsWifi"));
        assert!(!c.auto_fixed);
    }

    #[test]
    fn fixing_flags_does_not_pass_wifi_until_network_is_observed() {
        let mut state = wireless([Some(true); 4]);
        state.changed = vec!["EnableWifiDebugging"];
        let c = wifi_check(Some("usb"), Some(&state), Ok(false));
        assert_eq!(c.status, "warn");
        assert!(c.auto_fixed);
        let c = wifi_check(Some("usb"), Some(&state), Ok(true));
        assert_eq!(c.status, "ok");
        assert!(c.auto_fixed);
        assert!(c.detail.as_ref().unwrap().contains("reachable over Wi-Fi as well as USB"));
        let output = json!({ "ok": true, "ready": true, "checks": [c] });
        assert_eq!(output["checks"][0]["auto_fixed"], true);
        let human = crate::human::render("doctor", &output);
        assert!(human.contains('✓') && human.contains("EnableWifiDebugging=true"));
        assert!(human.contains("enabled and read back: EnableWifiDebugging"));
    }

    fn says(checks: &[Check]) -> String {
        checks
            .iter()
            .map(|c| format!("{} {} {}", c.check, c.status, c.user_action.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_unreadable_screen_on_an_unlocked_phone_is_not_called_a_lock() {
        // 2026-11-11: lockdown said unlocked, the launch could not be seen in
        // front because no reading came, and doctor said "unlock the phone".
        let (checks, front) = front_checks(
            Some(false),
            Some(Ok(false)),
            Some(Err("no accessibility tree within 14000 ms (3 empty, 0 not understood)".into())),
        );
        let text = says(&checks);
        assert!(!front);
        assert!(!text.contains("잠금을 풀") && !text.contains("Unlock"), "{text}");
        assert!(text.contains("screen_unlocked ok"), "{text}");
        assert!(text.contains("accessibility_tree fail"), "{text}");
        // Lockdown not answering is no reason to call it a lock either.
        let (checks, _) = front_checks(None, Some(Ok(false)), Some(Err("ax_timeout".into())));
        let text = says(&checks);
        assert!(!text.contains("잠금을 풀") && !text.contains("Unlock"), "{text}");
        assert!(text.contains("accessibility_tree fail"), "{text}");
    }

    #[test]
    fn only_lockdown_makes_it_a_lock() {
        let (checks, front) = front_checks(Some(true), None, None);
        assert!(!front);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].check, "screen_unlocked");
        assert_eq!(checks[0].status, "fail");
        assert!(checks[0].user_action.is_some());
    }

    #[test]
    fn another_screen_in_front_is_said_as_such() {
        let (checks, front) = front_checks(Some(false), Some(Ok(false)), Some(Ok(false)));
        let text = says(&checks);
        assert!(!front);
        assert!(text.contains("settings_in_front fail"), "{text}");
        assert!(!text.contains("잠금을 풀") && !text.contains("Unlock"), "{text}");
    }

    #[test]
    fn settings_in_front_passes() {
        assert!(front_checks(Some(false), Some(Ok(true)), None).1);
        // Seen only on the reading after the launch.
        assert!(front_checks(None, Some(Ok(false)), Some(Ok(true))).1);
    }
}
