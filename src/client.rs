//! Talk to the daemon over its Unix socket, starting it when needed.

use std::{
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    process::Stdio,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use crate::{
    config::Config,
    daemon::{Request, build_stamp},
    device::conn,
    i18n::t,
    paths,
    reply::{Fail, envelope},
};

/// Pick the phone: `--udid`, then `IPH_UDID`, then `udid` in config.toml,
/// then the only connected phone.
pub async fn resolve_udid(flag: Option<String>) -> Result<String, Fail> {
    if let Some(u) = flag.filter(|u| !u.is_empty()) {
        return Ok(u);
    }
    if let Ok(u) = std::env::var("IPH_UDID")
        && !u.is_empty() {
            return Ok(u);
        }
    let config = Config::load().map_err(|e| Fail::not_sent("config_invalid", format!("{e:#}")))?;
    if let Some(u) = config.udid.filter(|u| !u.is_empty()) {
        return Ok(u);
    }
    let devices = conn::list()
        .await
        .map_err(|e| Fail::not_sent("usbmuxd_unreachable", format!("{e:#}")))?;
    match devices.len() {
        1 => Ok(devices[0].udid.clone()),
        0 => Err(Fail::not_sent("no_device", "no iPhone is connected").hint(t(
            "폰을 USB로 연결하거나 맥과 같은 Wi-Fi에 두고 화면을 켜 주세요. 처음이라면 `iph setup`을 실행하세요.",
            "Plug the phone in, or put it on the Mac's Wi-Fi and wake it. First time? Run `iph setup`.",
        ))),
        n => Err(Fail::not_sent(
            "multiple_devices",
            format!("{n} iPhones are connected"),
        )
        .hint(t(
            "`--udid <UDID>`로 하나를 고르거나 ~/.config/iph/config.toml에 `udid = \"…\"`를 적으세요.",
            "Pick one with `--udid <UDID>` or put `udid = \"…\"` in ~/.config/iph/config.toml.",
        ))
        .with("devices", json!(devices))),
    }
}

fn spawn_daemon(udid: &str) -> std::io::Result<()> {
    paths::ensure_private_dir(&paths::state_dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(paths::daemon_log(udid))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("daemon")
        .arg("--udid")
        .arg(udid)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Its own session, so closing the terminal does not take the daemon along.
    // SAFETY: setsid is async-signal-safe and touches nothing shared.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map(|_| ())
}

async fn connect(udid: &str) -> Result<UnixStream, String> {
    let sock = paths::socket(udid);
    if let Ok(s) = UnixStream::connect(&sock).await {
        return Ok(s);
    }
    spawn_daemon(udid).map_err(|e| format!("cannot start the daemon: {e}"))?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        if let Ok(s) = UnixStream::connect(&sock).await {
            return Ok(s);
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    Err(format!(
        "the daemon did not come up; see {}",
        paths::daemon_log(udid).display()
    ))
}

/// Ask the daemon to step down and wait until it has. `None` when none was
/// running, otherwise its last words. Unless `keep_dnd`, it first puts back
/// the Focus it replaced with Do Not Disturb, which takes some seconds.
async fn stop(udid: &str, keep_dnd: bool) -> Option<Value> {
    let sock = paths::socket(udid);
    let mut s = UnixStream::connect(&sock).await.ok()?;
    let req = Request { build: build_stamp(), cmd: "shutdown".into(), args: json!({ "keep_dnd": keep_dnd }) };
    let _ = s.write_all(format!("{}\n", serde_json::to_string(&req).unwrap()).as_bytes()).await;
    let mut line = String::new();
    // It answers after the command in flight and the Focus change.
    let patience = crate::daemon::budget("shutdown") + Duration::from_secs(70);
    if tokio::time::timeout(patience, BufReader::new(&mut s).read_line(&mut line)).await.is_err() {
        kill_daemon(udid);
        return Some(json!({ "ok": true, "killed": true }));
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if UnixStream::connect(&sock).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    Some(serde_json::from_str(&line).unwrap_or_else(|_| json!({ "ok": true })))
}

/// Kill a daemon that no longer answers, by the pid it wrote at startup.
fn kill_daemon(udid: &str) {
    if let Some(pid) = std::fs::read_to_string(paths::daemon_pid(udid))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 1)
    {
        // SAFETY: plain kill(2) on a pid this tool's own daemon recorded.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    let _ = std::fs::remove_file(paths::socket(udid));
    let _ = std::fs::remove_file(paths::daemon_pid(udid));
}

/// Stop the daemon for `udid`. `None` when none was running.
pub async fn stop_daemon(udid: &str, keep_dnd: bool) -> Option<Value> {
    stop(udid, keep_dnd).await
}

/// Send one command and return the result envelope.
pub async fn request(udid: &str, cmd: &str, args: Value) -> Value {
    let started = Instant::now();
    let req = Request { build: build_stamp(), cmd: cmd.to_string(), args };
    let line = format!("{}\n", serde_json::to_string(&req).unwrap());
    for _ in 0..3 {
        let mut stream = match connect(udid).await {
            Ok(s) => s,
            Err(e) => return envelope(Err(Fail::not_sent("daemon_unreachable", e)), started),
        };
        if let Err(e) = stream.write_all(line.as_bytes()).await {
            return envelope(
                Err(Fail::not_sent("daemon_unreachable", format!("write: {e}"))),
                started,
            );
        }
        let mut reply = String::new();
        // The daemon bounds every command itself (and, ahead of a command,
        // the switch to Do Not Disturb); this is for a daemon that stopped
        // answering altogether.
        let patience = crate::daemon::budget(cmd) + Duration::from_secs(100);
        let read = tokio::time::timeout(patience, BufReader::new(&mut stream).read_line(&mut reply)).await;
        let Ok(read) = read else {
            kill_daemon(udid);
            return envelope(
                Err(Fail::unknown(
                    "daemon_timeout",
                    format!("the daemon did not answer within {} s and was stopped", patience.as_secs()),
                )
                .hint(t(
                    "입력이 나갔을 수 있습니다. 다시 보내지 말고 `iph read`로 화면을 확인하세요.",
                    "The input may have gone out. Do not resend; check the screen with `iph read`.",
                ))),
                started,
            );
        };
        let parsed: Option<Value> = match read {
            Ok(n) if n > 0 => serde_json::from_str(&reply).ok(),
            _ => None,
        };
        let Some(value) = parsed else {
            // The daemon took the request and went away: the input may be out.
            return envelope(
                Err(Fail::unknown(
                    "daemon_connection_lost",
                    "the daemon closed the connection before answering",
                )
                .hint(t(
                    "입력이 나갔을 수 있습니다. 다시 보내지 말고 `iph read`로 화면을 확인하세요.",
                    "The input may have gone out. Do not resend; check the screen with `iph read`.",
                ))),
                started,
            );
        };
        if value["error"]["code"] == "daemon_outdated" {
            // It refused before doing anything; replace it and ask again.
            stop(udid, true).await;
            continue;
        }
        return value;
    }
    envelope(
        Err(Fail::not_sent("daemon_unreachable", "the daemon keeps reporting an older build")),
        started,
    )
}
