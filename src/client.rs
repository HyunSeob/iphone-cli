//! Talk to the daemon over its socket (named pipe on Windows), starting it
//! when needed.

use std::{
    process::Stdio,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{
    config::Config,
    daemon::{Request, build_stamp},
    device::conn,
    i18n::t,
    events, ipc, paths,
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
    events::record("device_selection", if devices.len() == 1 { "ok" } else { "error" }, 0,
        json!({ "devices": devices, "count": devices.len() }));
    match devices.len() {
        1 => Ok(devices[0].udid.clone()),
        0 => Err(Fail::not_sent("no_device", "no iPhone is connected").hint(t(
            "폰을 USB로 연결하거나 컴퓨터와 같은 Wi-Fi에 두고 화면을 켜 주세요. 처음이라면 `iph setup`을 실행하세요.",
            "Plug the phone in, or put it on the computer's Wi-Fi and wake it. First time? Run `iph setup`.",
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
    let log = paths::private_file()
        .create(true)
        .append(true)
        .open(paths::daemon_log(udid))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("daemon")
        .arg("--udid")
        .arg(udid)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    detach(&mut cmd);
    cmd.spawn().map(|_| ())
}

/// Its own session, so closing the terminal does not take the daemon along.
#[cfg(unix)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe and touches nothing shared.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

/// No console window, and out of the terminal's process group. The daemon
/// must not inherit this process's own stdout/stderr: when an agent reads
/// `iph` through a pipe, an inherited copy would keep the pipe open until the
/// daemon exits, and the caller would wait that long.
#[cfg(windows)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::{
        Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation},
        System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
    };
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: only clears the inherit flag on this process's own handles.
        unsafe {
            let handle = GetStdHandle(which);
            if !handle.is_null() {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

async fn connect(udid: &str) -> Result<ipc::Stream, String> {
    if let Ok(s) = ipc::connect(udid).await {
        return Ok(s);
    }
    spawn_daemon(udid).map_err(|e| format!("cannot start the daemon: {e}"))?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        if let Ok(s) = ipc::connect(udid).await {
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
/// running, otherwise its last words.
async fn stop(udid: &str) -> Option<Value> {
    let mut s = ipc::connect(udid).await.ok()?;
    let req = Request { build: build_stamp(), cmd: "shutdown".into(), args: Value::Null };
    let _ = s.write_all(format!("{}\n", serde_json::to_string(&req).unwrap()).as_bytes()).await;
    let mut line = String::new();
    // It answers after the command in flight.
    let patience = crate::daemon::budget("shutdown");
    if tokio::time::timeout(patience, BufReader::new(&mut s).read_line(&mut line)).await.is_err() {
        kill_daemon(udid);
        return Some(json!({ "ok": true, "killed": true }));
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if ipc::connect(udid).await.is_err() {
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
        kill_pid(pid);
    }
    ipc::cleanup(udid);
    let _ = std::fs::remove_file(paths::daemon_pid(udid));
}

/// Stop the daemon for `udid`. `None` when none was running.
pub async fn stop_daemon(udid: &str) -> Option<Value> {
    stop(udid).await
}

/// Send one command and return the result envelope.
pub async fn request(udid: &str, cmd: &str, args: Value) -> Value {
    let started = Instant::now();
    events::record("request", "started", 0, json!({ "udid": udid, "command": cmd }));
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
            stop(udid).await;
            continue;
        }
        events::record("request", if value["ok"] == true { "ok" } else { "error" }, started.elapsed().as_millis(),
            json!({ "udid": udid, "command": cmd, "outcome": value["outcome"], "error": value["error"] }));
        return value;
    }
    envelope(
        Err(Fail::not_sent("daemon_unreachable", "the daemon keeps reporting an older build")),
        started,
    )
}

#[cfg(unix)]
fn kill_pid(pid: i32) {
    // SAFETY: plain kill(2) on a pid this tool's own daemon recorded.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_pid(pid: i32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
