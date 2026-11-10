//! `iph doctor`: check, in order, everything the phone needs for iph to work,
//! and say in one line what the user must do for each thing that is missing.
//!
//! Which checks are truly required was established on an iPhone 15 Pro Max
//! with iOS 27.0.1; see docs/onboarding.md.

use std::time::Duration;

use idevice::{IdeviceService, amfi::AmfiClient, usbmuxd::UsbmuxdConnection};
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
                "폰을 USB로 연결하거나, 맥과 같은 Wi-Fi에 두고 화면을 켜 주세요.",
                "Plug the phone in, or put it on the Mac's Wi-Fi and wake its screen.",
            ),
    });

    // 3. Developer Mode. If it is off, make the switch appear in Settings.
    let mut dev_mode = None;
    if let Ok(r) = &route {
        if let Some(Ok(mut amfi)) = with_timeout(AmfiClient::connect(&*r.provider)).await {
            dev_mode = with_timeout(amfi.get_developer_mode_status()).await.and_then(|r| r.ok());
            if dev_mode == Some(false) {
                let revealed = with_timeout(amfi.reveal_developer_mode_option_in_ui())
                    .await
                    .is_some_and(|r| r.is_ok());
                let mut c = Check::new("developer_mode", true).status("fail").action(
                    "설정 → 개인정보 보호 및 보안 → 개발자 모드 → 켜기 → 재시동 후 비밀번호를 입력하세요.",
                    "Settings → Privacy & Security → Developer Mode → turn on → restart, then enter the passcode.",
                );
                c.auto_fixed = revealed;
                if revealed {
                    c = c.detail("the Developer Mode switch was made visible in Settings");
                }
                checks.push(c);
            }
        }
    }
    match dev_mode {
        Some(true) => checks.push(Check::new("developer_mode", true)),
        Some(false) => {}
        None => checks.push(if route.is_ok() {
            Check::new("developer_mode", true)
                .status("warn")
                .detail("could not ask the phone; judged by the services below")
        } else {
            skip("developer_mode", true, "phone not reachable")
        }),
    }

    // 4. Developer disk image: its services must be on offer over the tunnel.
    let mut services_ok = false;
    if route.is_ok() {
        match s.ensure_tunnel().await {
            Ok(tunnel) => {
                let missing: Vec<&str> = NEEDED_SERVICES
                    .iter()
                    .filter(|name| !tunnel.rsd.services.contains_key(**name))
                    .copied()
                    .collect();
                services_ok = missing.is_empty();
                checks.push(if services_ok {
                    Check::new("developer_disk_image", true)
                } else {
                    Check::new("developer_disk_image", true)
                        .status("fail")
                        .detail(format!("missing services: {}", missing.join(", ")))
                        .action(
                            "Xcode를 열고 창 → 기기 및 시뮬레이터에서 이 폰을 한 번 선택하면 개발자 디스크 이미지가 연결됩니다. 그 뒤 다시 `iph doctor`를 실행하세요.",
                            "Open Xcode → Window → Devices and Simulators and select this phone once; that mounts the developer disk image. Then run `iph doctor` again.",
                        )
                });
            }
            Err(f) => checks.push(
                Check::new("developer_disk_image", true)
                    .status("fail")
                    .detail(f.message)
                    .action(
                        "폰 잠금을 풀고 다시 시도하세요. 계속 실패하면 USB로 연결하세요.",
                        "Unlock the phone and try again. If it keeps failing, connect by USB.",
                    ),
            ),
        }
    } else {
        checks.push(skip("developer_disk_image", true, "phone not reachable"));
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
                    "케이블 없이 쓰려면 폰과 맥을 같은 Wi-Fi에 두세요. (Finder에서 폰을 고르고 'Wi-Fi에 있을 때 이 iPhone 보기'를 켜야 할 수 있습니다.)",
                    "To work without the cable, put the phone and the Mac on the same Wi-Fi. (You may need \"Show this iPhone when on Wi-Fi\" in Finder.)",
                )
            }
        }
        None => skip("wifi", false, "phone not reachable"),
    });

    // 6. Unlocked: Settings can be brought to the front and read.
    let mut front_ok = false;
    if services_ok {
        let launched = s.cmd_launch(LaunchArgs { app: SETTINGS.into() }).await;
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
        let down = s
            .cmd_scroll(ScrollArgs { direction: "down".into(), until: None, max: None })
            .await;
        let moved = matches!(&down, Ok(r) if r.get("screen_changed").and_then(|v| v.as_bool()) == Some(true));
        if moved {
            // Put the list back where it was.
            let _ = s
                .cmd_scroll(ScrollArgs { direction: "up".into(), until: None, max: None })
                .await;
            checks.push(Check::new("touch_input", true));
        } else {
            let detail = match down {
                Ok(_) => "the drag was sent but the screen did not move".to_string(),
                Err(f) => f.message,
            };
            checks.push(Check::new("touch_input", true).status("fail").detail(detail).action(
                "설정 → 개발자 → 'UI 자동화 활성화'를 켜고 다시 시도하세요. 그래도 안 되면 폰을 재시동하세요.",
                "Turn on Settings → Developer → Enable UI Automation and try again. If it still fails, restart the phone.",
            ));
        }
    } else {
        checks.push(skip("touch_input", true, "no accessibility tree"));
    }

    // 9. Auto-lock (recommended). Not read from the phone: just advice.
    checks.push(Check::new("auto_lock", false).status("info").action(
        "오래 쓸 때는 설정 → 디스플레이 및 밝기 → 자동 잠금 → '안 함'을 권합니다. 폰이 잠기면 읽기와 입력이 멈춥니다.",
        "For long sessions set Settings → Display & Brightness → Auto-Lock → Never. Reading and input stop when the phone locks.",
    ));

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
