//! `iph doctor`: check, in order, everything the phone needs for iph to work,
//! and say in one line what the user must do for each thing that is missing.
//!
//! Which checks are truly required was established on an iPhone 15 Pro Max
//! with iOS 27.0.1; see docs/onboarding.md.

use std::time::Duration;

use idevice::{
    IdeviceService, amfi::AmfiClient, lockdown::LockdownClient,
    mobile_image_mounter::ImageMounter, provider::IdeviceProvider, usbmuxd::UsbmuxdConnection,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    actions::{LaunchArgs, ScrollArgs},
    ax::tree::BuildOptions,
    device::{
        ax, conn,
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
        facts.locked = with_timeout(lockdown.get_value(Some("PasswordProtected"), None))
            .await
            .and_then(|r| r.ok())
            .and_then(|v| v.as_boolean());
    }
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

pub async fn run(s: &mut Session, args: DoctorArgs) -> Res {
    let mut checks: Vec<Check> = Vec::new();
    let skip = |name: &'static str, required: bool, why: &str| {
        Check::new(name, required).status("skipped").detail(why.to_string())
    };

    // 1. Pairing: the Mac holds a pairing record for this phone.
    let paired = match UsbmuxdConnection::default().await {
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
    checks.push(match connection.as_deref() {
        Some("wifi") => Check::new("wifi", false),
        Some(_) => {
            let also_wifi = conn::list_raw().await.ok().is_some_and(|all| {
                all.iter().any(|d| {
                    d.udid == s.udid && matches!(d.connection_type, idevice::usbmuxd::Connection::Network(_))
                })
            });
            if also_wifi {
                Check::new("wifi", false).detail("reachable over Wi-Fi as well as USB")
            } else {
                Check::new("wifi", false).status("warn").action(
                    "케이블 없이 쓰려면 폰과 컴퓨터를 같은 Wi-Fi에 두세요. (맥은 Finder, 윈도는 Apple 기기 앱에서 폰을 고르고 'Wi-Fi에 있을 때 이 iPhone 보기'를 켜야 할 수 있습니다.)",
                    "To work without the cable, put the phone and the computer on the same Wi-Fi. (You may need \"Show this iPhone when on Wi-Fi\" in Finder on a Mac, or in the Apple Devices app on Windows.)",
                )
            }
        }
        None => skip("wifi", false, "phone not reachable"),
    });

    // 6. Unlocked: lockdown says so, and Settings can be brought to the front.
    let mut front_ok = false;
    if facts.as_ref().and_then(|f| f.locked) == Some(true) {
        checks.push(
            Check::new("screen_unlocked", true)
                .status("fail")
                .detail("the phone reports that it is locked")
                .action("폰 잠금을 풀어 주세요.", "Unlock the phone."),
        );
    } else if services_ok {
        let launched = s.cmd_launch(LaunchArgs { app: SETTINGS.into(), restart: false }).await;
        match launched {
            Ok(r) if r.get("foreground").and_then(|v| v.as_bool()) == Some(true) => {
                front_ok = true;
                checks.push(Check::new("screen_unlocked", true));
            }
            Ok(_) => checks.push(Check::new("screen_unlocked", true).status("fail").action(
                "폰 잠금을 풀어 주세요.",
                "Unlock the phone.",
            )),
            Err(f) => checks.push(
                Check::new("screen_unlocked", true)
                    .status("fail")
                    .detail(f.message)
                    .action("폰 잠금을 풀어 주세요.", "Unlock the phone."),
            ),
        }
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
    } else {
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

    // 10. Do Not Disturb (recommended). Only what iph is set to do and what it
    // left behind; the phone's Focus is not touched here.
    let dnd = if let Some(left) = &s.dnd.record {
        Check::new("do_not_disturb", false)
            .status("info")
            .detail(format!("on since {}, set by iph", left.at))
            .action(
                "iph가 켠 방해금지 모드가 켜져 있습니다. 명령이 없으면 잠시 뒤 원래대로 돌아가고, `iph dnd off`로 바로 되돌릴 수 있습니다.",
                "Do Not Disturb that iph turned on is on. It goes back by itself after a while without commands; `iph dnd off` puts it back now.",
            )
    } else if s.config.safety.dnd {
        Check::new("do_not_disturb", false).detail("turned on before the first input, put back afterwards")
    } else {
        Check::new("do_not_disturb", false).status("info").action(
            "[safety] dnd = false 입니다. 알림 배너가 화면 위쪽 탭을 가로챌 수 있으니, 쓰는 동안 집중 모드를 직접 켜 두세요.",
            "[safety] dnd = false. A notification banner can take a tap near the top of the screen, so turn a Focus on yourself while iph works.",
        )
    };
    checks.push(dnd);

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
