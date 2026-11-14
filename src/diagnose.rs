//! Collect an issue attachment without needing a selected or reachable phone.

use std::{
    collections::{BTreeSet, VecDeque},
    future::Future,
    io::{Read, Seek, SeekFrom, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use idevice::{
    IdeviceService, lockdown::LockdownClient, mobile_image_mounter::ImageMounter,
    provider::IdeviceProvider,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

use crate::{
    config::Config,
    device::conn,
    events, fields,
    i18n::t,
    paths,
    reply::{Fail, Res},
};

const CAPTURE_BYTES: usize = 256 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Save the attachment here (default: iph-diagnostics-<time>-<pid>.txt).
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Start with USB connected; observe discovery while you unplug it.
    #[arg(long)]
    pub wifi: bool,
    /// Seconds to observe after the USB baseline with --wifi.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=300))]
    pub duration: u64,
    /// Maximum run time of the iph command supplied after --.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=900))]
    pub command_timeout: u64,
    /// Optional iph command to run once and capture, e.g. -- read --full.
    #[arg(last = true, value_name = "IPH_COMMAND")]
    pub command: Vec<String>,
}

struct Report {
    path: PathBuf,
    data: Value,
}

impl Report {
    fn new(path: PathBuf, args: &Args) -> Result<Self> {
        // Refuse accidental overwrites; reports can be compared across runs.
        paths::private_file()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        let report = Self {
            path,
            data: json!({
                "schema_version": 1, "status": "collecting", "started_at": crate::clock::now_utc(),
                "started_unix_ms": events::now_ms(), "wifi_observation": args.wifi,
                "observation_seconds": if args.wifi { args.duration } else { 5 },
                "environment": { "iph_version": env!("CARGO_PKG_VERSION"), "build": crate::daemon::build_stamp(),
                    "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
                "snapshots": [], "discovery_timeline": [], "findings": [],
            }),
        };
        report.save()?;
        Ok(report)
    }

    fn save(&self) -> Result<()> {
        let mut file = paths::private_file()
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        serde_json::to_writer_pretty(&mut file, &self.data)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }

    fn push(&mut self, key: &str, value: Value) {
        self.data[key].as_array_mut().unwrap().push(value);
    }
}

pub async fn run(args: Args, flag: Option<String>) -> Res {
    let path = args.output.clone().unwrap_or_else(|| {
        PathBuf::from(format!(
            "iph-diagnostics-{}-{}.txt",
            crate::clock::now_utc().replace([':', '-'], ""),
            std::process::id(),
        ))
    });
    let path =
        std::path::absolute(path).map_err(|e| Fail::not_sent("diagnostic_file", e.to_string()))?;
    let mut report = Report::new(path, &args)
        .map_err(|e| Fail::not_sent("diagnostic_file", format!("{e:#}")))?;
    eprintln!(
        "{} {}",
        t("진단 파일을 수집합니다:", "Collecting diagnostics:"),
        report.path.display()
    );
    let interrupted = tokio::select! {
        result = collect(&mut report, &args, flag) => {
            if let Err(error) = result { report.data["collection_error"] = json!(format!("{error:#}")); }
            false
        }
        result = tokio::signal::ctrl_c() => {
            report.data["interrupt"] = json!(result.err().map(|e| e.to_string()));
            true
        }
    };
    report.data["status"] = json!(if interrupted {
        "interrupted"
    } else if report.data["collection_error"].is_string() {
        "partial"
    } else {
        "complete"
    });
    report.data["finished_at"] = json!(crate::clock::now_utc());
    report.data["recent_events"] = recent_events();
    report.data["daemon_logs"] = daemon_logs(report.data["selected_udid"].as_str());
    report.data["findings"] = json!(findings(&report.data));
    report
        .save()
        .map_err(|e| Fail::not_sent("diagnostic_file", format!("{e:#}")))?;
    Ok(
        fields! { "report": report.path, "interrupted": interrupted, "findings": report.data["findings"],
        "hint": t("GitHub 이슈에 이 파일을 첨부해 주세요.", "Attach this file to your GitHub issue.") },
    )
}

async fn collect(report: &mut Report, args: &Args, flag: Option<String>) -> Result<()> {
    let config = Config::load();
    let mut chosen = flag
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("IPH_UDID").ok().filter(|s| !s.is_empty()))
        .or_else(|| config.as_ref().ok().and_then(|c| c.udid.clone()));
    report.data["config"] = match &config {
        Ok(c) => json!({ "path": paths::config_file(), "values": c }),
        Err(e) => json!({ "error": format!("{e:#}") }),
    };
    let mut env = serde_json::Map::new();
    for key in [
        "IPH_UDID",
        "IPH_PREFER",
        "IPH_LOG",
        "IPH_TRACE",
        "IPH_STATE_DIR",
        "IPH_CONFIG_DIR",
        "IPH_INPUT_WARMUP_MS",
        "IPH_INPUT_PRIME",
        "IPH_REUSE_MS",
        "IPH_REOPEN_GAP_MS",
        "IPH_PCAP",
        "IPH_DUMP_DIR",
        "USBMUXD_SOCKET_ADDRESS",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), json!(value));
        }
    }
    report.data["environment"]["variables"] = json!(env);
    report.data["environment"]["usbmuxd_address"] = json!(format!("{:?}", conn::mux_addr()));
    report.save()?;

    if args.wifi {
        eprintln!(
            "{}",
            t(
                "USB 연결 상태를 먼저 기록합니다. 아직 케이블을 빼지 마세요.",
                "Recording the USB baseline. Keep the cable connected for now."
            )
        );
    }
    // Preserve the original failure before opening diagnostic sessions. The
    // Wi-Fi mode needs USB facts first, while that route is still available.
    let before = snapshot("before", &mut chosen, args.wifi || args.command.is_empty()).await;
    report.data["selected_udid"] = json!(chosen);
    report.push("snapshots", before);
    report.save()?;
    if !args.wifi {
        reproduce(report, args, &chosen).await?;
    }
    if args.wifi {
        eprintln!(
            "{} {} {}",
            t("이제 USB 케이블을 빼세요.", "Unplug the USB cable now."),
            args.duration,
            t(
                "초 동안 기기 목록과 Bonjour 변화를 기록합니다.",
                "seconds of discovery and Bonjour changes will be recorded."
            )
        );
    }

    let window = Duration::from_secs(if args.wifi { args.duration } else { 5 });
    let browse_args = ["-B".into(), "_apple-mobdev2._tcp".into(), "local.".into()];
    let browse = capture("dns-sd", &browse_args, window);
    let observe = async {
        if args.wifi {
            let start = Instant::now();
            while start.elapsed() < window {
                let listing = list_snapshot().await;
                report.push("discovery_timeline", json!({ "at": crate::clock::now_utc(), "unix_ms": events::now_ms(), "listing": listing }));
                report.save()?;
                tokio::time::sleep(
                    Duration::from_secs(1).min(window.saturating_sub(start.elapsed())),
                )
                .await;
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let (mut bonjour, observation) = tokio::join!(browse, observe);
    observation?;
    bonjour["observation_complete"] = bonjour["timed_out"].clone();
    bonjour["note"] = json!(
        "Advertisements may belong to other devices. Absence alone does not identify whether the phone, network, or host blocked discovery."
    );
    report.data["bonjour"] = bonjour;
    report.save()?;
    report.data["bonjour"]["resolved_services"] =
        resolve_services(report.data["bonjour"]["stdout"].as_str().unwrap_or("")).await;
    report.save()?;

    if args.wifi {
        reproduce(report, args, &chosen).await?;
    }
    report.push("snapshots", snapshot("after", &mut chosen, true).await);
    report.data["selected_udid"] = json!(chosen);
    report.save()?;

    let commands: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[
            ("sw_vers", &[]),
            ("uname", &["-a"]),
            ("ifconfig", &[]),
            ("scutil", &["--nwi"]),
            ("scutil", &["--nc", "list"]),
            ("route", &["-n", "get", "default"]),
        ]
    } else if cfg!(windows) {
        &[
            ("cmd", &["/C", "ver"]),
            ("ipconfig", &["/all"]),
            ("route", &["print"]),
        ]
    } else {
        &[("uname", &["-a"]), ("ip", &["address"]), ("ip", &["route"])]
    };
    let host = futures_util::future::join_all(commands.iter().map(|(program, argv)| async move {
        capture(
            program,
            &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            PROBE_TIMEOUT,
        )
        .await
    }))
    .await;
    report.data["environment"]["host_commands"] = json!(host);
    report.save()?;
    if cfg!(target_os = "macos") {
        report.data["system_logs"] = capture("log", &[
            "show", "--last", "10m", "--style", "compact", "--info", "--predicate",
            "process == 'usbmuxd' OR process == 'mDNSResponder' OR process == 'AMPDevicesAgent'",
        ].map(String::from), Duration::from_secs(10)).await;
    } else {
        report.data["system_logs"] = json!({ "status": "unavailable", "reason": "macOS unified logs are not available on this platform" });
    }
    Ok(())
}

async fn list_snapshot() -> Value {
    match conn::list_raw().await {
        Ok(devices) => json!({ "ok": true, "devices": conn::describe_raw(&devices) }),
        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
    }
}

async fn reproduce(report: &mut Report, args: &Args, chosen: &Option<String>) -> Result<()> {
    if args.command.is_empty() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let mut argv = Vec::new();
    if let Some(udid) = chosen
        && !args
            .command
            .iter()
            .any(|s| s == "--udid" || s.starts_with("--udid="))
    {
        argv.extend(["--udid".into(), udid.clone()]);
    }
    argv.extend(args.command.iter().cloned());
    report.data["reproduction"] = json!({ "command": exe, "args": argv, "status": "started", "started_at": crate::clock::now_utc() });
    report.save()?;
    eprintln!(
        "{} {:?}",
        t(
            "문제 명령을 한 번 실행합니다:",
            "Running the reproduction command once:"
        ),
        args.command
    );
    report.data["reproduction"] = capture(
        exe.as_os_str(),
        &argv,
        Duration::from_secs(args.command_timeout),
    )
    .await;
    report.save()
}

async fn snapshot(phase: &str, chosen: &mut Option<String>, probe: bool) -> Value {
    let mut out = json!({ "phase": phase, "at": crate::clock::now_utc(), "unix_ms": events::now_ms(), "probes": [] });
    out["raw_usbmuxd"] = match raw_mux_listing().await {
        Ok(value) => json!({ "ok": true, "response": value }),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    };
    let devices = match conn::list_raw().await {
        Ok(devices) => devices,
        Err(error) => {
            out["listing"] = json!({ "ok": false, "error": format!("{error:#}") });
            return out;
        }
    };
    out["listing"] = json!({ "ok": true, "devices": conn::describe_raw(&devices) });
    if chosen.is_none() {
        let ids: BTreeSet<_> = devices.iter().map(|d| d.udid.clone()).collect();
        if ids.len() == 1 {
            *chosen = ids.into_iter().next();
        }
    }
    out["selected_udid"] = json!(chosen);
    let Some(udid) = chosen.as_ref() else {
        out["selection"] = json!(if devices.is_empty() {
            "no_device"
        } else {
            "multiple_devices; choose --udid to probe one"
        });
        return out;
    };
    if !probe {
        out["probe_note"] = json!("Active probes deferred until after the reproduction command");
        return out;
    }
    for device in devices.iter().filter(|d| &d.udid == udid) {
        let addr = match conn::mux_addr() {
            Ok(addr) => addr,
            Err(e) => {
                out["error"] = json!(format!("{e:#}"));
                return out;
            }
        };
        let provider = device.to_provider(addr, conn::LABEL);
        let mut probe =
            probe_provider(udid, conn::conn_name(&device.connection_type), &provider).await;
        if let idevice::usbmuxd::Connection::Network(ip) = device.connection_type {
            if matches!(ip, std::net::IpAddr::V6(v6) if v6.is_unicast_link_local()) {
                probe["direct_tcp"] = json!({ "status": "skipped", "reason": "usbmuxd's parsed address omits the IPv6 scope; see the raw NetworkAddress and Bonjour interface probes" });
            } else {
                probe["direct_tcp"] = tcp_probe(SocketAddr::new(ip, 62078)).await;
            }
        }
        out["probes"].as_array_mut().unwrap().push(probe);
    }
    // A known phone can still be reachable at its cached address when the
    // system temporarily omits it. Record that route separately from discovery.
    if out["probes"].as_array().unwrap().is_empty() {
        match conn::route(udid).await {
            Ok(route) => out["probes"]
                .as_array_mut()
                .unwrap()
                .push(probe_provider(udid, "cached_wifi", &*route.provider).await),
            Err(e) => out["route_error"] = json!(format!("{e:#}")),
        }
    }
    out
}

/// Retain the actual ListDevices response as well as the library's parsed
/// view: malformed NetworkAddress entries can otherwise be silently dropped.
async fn raw_mux_listing() -> Result<Value> {
    events::step("usbmuxd.raw_list", json!({}), PROBE_TIMEOUT, async {
        let mut socket = conn::mux_addr()?.to_socket().await?;
        let mut dict = plist::Dictionary::new();
        dict.insert("MessageType".into(), "ListDevices".into());
        dict.insert("ClientVersionString".into(), conn::LABEL.into());
        dict.insert("kLibUSBMuxVersion".into(), 3u64.into());
        let mut payload = Vec::new();
        plist::Value::Dictionary(dict).to_writer_xml(&mut payload)?;
        for word in [(payload.len() + 16) as u32, 1, 8, 1] {
            socket.write_all(&word.to_le_bytes()).await?;
        }
        socket.write_all(&payload).await?;
        let mut header = [0; 16];
        socket.read_exact(&mut header).await?;
        let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        anyhow::ensure!(
            (16..=1024 * 1024).contains(&length),
            "invalid usbmuxd response length {length}"
        );
        let mut payload = vec![0; length - 16];
        socket.read_exact(&mut payload).await?;
        let value = plist::Value::from_reader(std::io::Cursor::new(payload))?;
        Ok::<_, anyhow::Error>(serde_json::to_value(value)?)
    })
    .await
}

async fn check<T, E: std::fmt::Debug>(
    checks: &mut Vec<Value>,
    name: &str,
    future: impl Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    let started = Instant::now();
    let result = events::step(name, json!({ "source": "diagnose" }), PROBE_TIMEOUT, future).await;
    checks.push(
        json!({ "stage": name, "ok": result.is_ok(), "elapsed_ms": started.elapsed().as_millis(),
        "error": result.as_ref().err().map(|e| format!("{e:#}")) }),
    );
    result
}

async fn probe_provider(udid: &str, connection: &str, provider: &dyn IdeviceProvider) -> Value {
    let mut checks = Vec::new();
    let mut values = serde_json::Map::new();
    let pairing = check(&mut checks, "pairing_record", provider.get_pairing_file()).await;
    let paired = pairing.is_ok();
    let lockdown = check(
        &mut checks,
        "lockdown.connect",
        LockdownClient::connect(provider),
    )
    .await;
    if let Ok(mut lockdown) = lockdown {
        let session_ok = match &pairing {
            Ok(pairing) => check(
                &mut checks,
                "lockdown.start_session",
                lockdown.start_session(pairing),
            )
            .await
            .is_ok(),
            Err(_) => true,
        };
        if session_ok {
            for (key, domain) in [
                ("ProductType", None),
                ("ProductVersion", None),
                ("BuildVersion", None),
                ("DeviceName", None),
                ("WiFiAddress", None),
                ("PasswordProtected", None),
                (
                    "EnableWifiConnections",
                    Some("com.apple.mobile.wireless_lockdown"),
                ),
                (
                    "EnableWifiDebugging",
                    Some("com.apple.mobile.wireless_lockdown"),
                ),
            ] {
                match check(
                    &mut checks,
                    &format!("lockdown.GetValue.{key}"),
                    lockdown.get_value(Some(key), domain),
                )
                .await
                {
                    Ok(value) => {
                        values.insert(
                            key.into(),
                            serde_json::to_value(value).unwrap_or(Value::Null),
                        );
                    }
                    Err(_) => continue,
                }
            }
        }
    }
    if paired {
        if let Ok(mut mounter) = check(
            &mut checks,
            "image_mounter.connect",
            ImageMounter::connect(provider),
        )
        .await
            && let Ok(enabled) = check(
                &mut checks,
                "developer_mode",
                mounter.query_developer_mode_status(),
            )
            .await
        {
            values.insert("DeveloperModeStatus".into(), json!(enabled));
        }
        let started = Instant::now();
        let tunnel = conn::open_tunnel_with_provider(provider, connection).await;
        checks.push(json!({ "stage": "tunnel", "ok": tunnel.is_ok(), "elapsed_ms": started.elapsed().as_millis(),
            "error": tunnel.as_ref().err().map(|e| format!("{e:#}")) }));
        if let Ok(tunnel) = tunnel {
            values.insert(
                "rsd_services".into(),
                json!(tunnel.rsd.services.keys().collect::<Vec<_>>()),
            );
        }
    }
    json!({ "udid": udid, "connection": connection, "paired": paired, "values": values, "checks": checks })
}

#[derive(Default)]
struct Capture {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl Capture {
    fn push(&mut self, bytes: &[u8]) {
        let excess = (self.bytes.len() + bytes.len()).saturating_sub(CAPTURE_BYTES);
        self.truncated |= excess > 0;
        self.bytes.drain(..excess.min(self.bytes.len()));
        self.bytes
            .extend(&bytes[bytes.len().saturating_sub(CAPTURE_BYTES)..]);
    }
}

async fn drain(mut reader: impl tokio::io::AsyncRead + Unpin, output: Arc<Mutex<Capture>>) {
    let mut bytes = [0; 8192];
    while let Ok(n) = reader.read(&mut bytes).await {
        if n == 0 {
            break;
        }
        output.lock().unwrap().push(&bytes[..n]);
    }
}

/// Keep partial stdout/stderr on timeout, drain both concurrently, and reap
/// the child. This also bounds tools that leave their output pipes open.
async fn capture(
    program: impl AsRef<std::ffi::OsStr>,
    argv: &[String],
    timeout: Duration,
) -> Value {
    let started = Instant::now();
    let mut out = json!({ "command": program.as_ref().to_string_lossy(), "args": argv,
        "started_at": crate::clock::now_utc(), "started_unix_ms": events::now_ms(),
        "timed_out": false, "exit_code": null, "stdout": "", "stderr": "" });
    let mut child = match Command::new(&program)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            out["error"] = json!(e.to_string());
            out["elapsed_ms"] = json!(started.elapsed().as_millis());
            return out;
        }
    };
    let stdout = Arc::new(Mutex::new(Capture::default()));
    let stderr = Arc::new(Mutex::new(Capture::default()));
    let mut stdout_task = tokio::spawn(drain(child.stdout.take().unwrap(), stdout.clone()));
    let mut stderr_task = tokio::spawn(drain(child.stderr.take().unwrap(), stderr.clone()));
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => out["exit_code"] = json!(status.code()),
        Ok(Err(e)) => out["error"] = json!(e.to_string()),
        Err(_) => {
            out["timed_out"] = json!(true);
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
    let drained = tokio::time::timeout(Duration::from_secs(1), async {
        let _ = tokio::join!(&mut stdout_task, &mut stderr_task);
    })
    .await
    .is_ok();
    if !drained {
        stdout_task.abort();
        stderr_task.abort();
    }
    out["output_pipes_closed"] = json!(drained);
    for (key, captured) in [("stdout", stdout), ("stderr", stderr)] {
        let captured = captured.lock().unwrap();
        out[key] = json!(String::from_utf8_lossy(
            &captured.bytes.iter().copied().collect::<Vec<_>>()
        ));
        out[format!("{key}_truncated")] = json!(captured.truncated);
    }
    out["finished_at"] = json!(crate::clock::now_utc());
    out["elapsed_ms"] = json!(started.elapsed().as_millis());
    out
}

fn browse_instances(text: &str) -> Vec<String> {
    let mut instances = BTreeSet::new();
    for line in text.lines() {
        if !line.split_whitespace().any(|word| word == "Add") {
            continue;
        }
        if let Some((_, name)) = line.split_once("_apple-mobdev2._tcp.") {
            let name = name.trim();
            if !name.is_empty() {
                instances.insert(name.to_string());
            }
        }
    }
    instances.into_iter().collect()
}

async fn resolve_services(text: &str) -> Value {
    let instances = browse_instances(text);
    // Preserve the full browse even on a busy LAN; bound active resolution.
    let services =
        futures_util::future::join_all(instances.iter().take(8).map(|instance| async move {
            let resolved = capture(
                "dns-sd",
                &[
                    "-L".into(),
                    instance.clone(),
                    "_apple-mobdev2._tcp".into(),
                    "local.".into(),
                ],
                Duration::from_secs(2),
            )
            .await;
            let mut service =
                json!({ "instance": instance, "resolve": resolved, "tcp_probes": [] });
            if let Some((host, port, interface)) =
                service_endpoint(service["resolve"]["stdout"].as_str().unwrap_or(""))
            {
                service["host"] = json!(host);
                service["port"] = json!(port);
                service["interface"] = json!(interface);
                let lookup = tokio::time::timeout(
                    PROBE_TIMEOUT,
                    tokio::net::lookup_host((host.as_str(), port)),
                )
                .await;
                match lookup {
                    Ok(Ok(addresses)) => {
                        let probes = addresses.take(4).map(|mut addr| async move {
                            if let SocketAddr::V6(ref mut v6) = addr
                                && v6.ip().is_unicast_link_local()
                                && v6.scope_id() == 0
                            {
                                v6.set_scope_id(interface.unwrap_or(0));
                            }
                            tcp_probe(addr).await
                        });
                        service["tcp_probes"] = json!(futures_util::future::join_all(probes).await);
                    }
                    Ok(Err(error)) => service["lookup_error"] = json!(error.to_string()),
                    Err(_) => service["lookup_error"] = json!("address lookup timed out"),
                }
            }
            service
        }))
        .await;
    json!({ "instances_seen": instances.len(), "resolution_limit": 8, "services": services })
}

fn service_endpoint(text: &str) -> Option<(String, u16, Option<u32>)> {
    for line in text.lines() {
        let Some((_, rest)) = line.split_once("can be reached at ") else {
            continue;
        };
        let endpoint = rest.split_whitespace().next()?;
        let (host, port) = endpoint.rsplit_once(':')?;
        let interface = line
            .split_once("(interface ")
            .and_then(|(_, value)| value.trim_end_matches(')').parse().ok());
        return Some((
            host.trim_end_matches('.').into(),
            port.parse().ok()?,
            interface,
        ));
    }
    None
}

async fn tcp_probe(addr: SocketAddr) -> Value {
    let started = Instant::now();
    let result = events::step(
        "direct_tcp.connect",
        json!({ "address": addr.to_string() }),
        PROBE_TIMEOUT,
        tokio::net::TcpStream::connect(addr),
    )
    .await;
    json!({ "address": addr.to_string(), "ok": result.is_ok(), "elapsed_ms": started.elapsed().as_millis(),
        "error": result.err().map(|e| format!("{e:#}")) })
}

fn tail_file(path: &Path) -> Value {
    let result = (|| -> std::io::Result<Value> {
        let mut file = std::fs::File::open(path)?;
        let bytes = file.metadata()?.len();
        file.seek(SeekFrom::Start(bytes.saturating_sub(CAPTURE_BYTES as u64)))?;
        let mut tail = Vec::new();
        file.take(CAPTURE_BYTES as u64).read_to_end(&mut tail)?;
        Ok(
            json!({ "path": path, "bytes": bytes, "truncated": bytes > CAPTURE_BYTES as u64, "text": String::from_utf8_lossy(&tail) }),
        )
    })();
    result.unwrap_or_else(|e| json!({ "path": path, "error": e.to_string() }))
}

fn recent_events() -> Value {
    let dir = paths::state_dir().join("events");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => return json!({ "path": dir, "error": e.to_string() }),
    };
    let mut files: Vec<_> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|s| s == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    json!(
        files
            .into_iter()
            .take(8)
            .map(|(_, path)| tail_file(&path))
            .collect::<Vec<_>>()
    )
}

fn daemon_logs(udid: Option<&str>) -> Value {
    match udid {
        Some(udid) => json!([tail_file(&paths::daemon_log(udid))]),
        None => {
            json!({ "status": "unavailable", "reason": "no phone selected; connection events still include discovery failures" })
        }
    }
}

fn findings(report: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let snapshots: Vec<_> = report["snapshots"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    if snapshots.iter().any(|s| s["listing"]["ok"] == false) {
        out.push(
            "usbmuxd could not be reached or listed devices; inspect the original listing error."
                .into(),
        );
    }
    for snapshot in &snapshots {
        if let (Some(raw), Some(parsed)) = (
            snapshot["raw_usbmuxd"]["response"]["DeviceList"].as_array(),
            snapshot["listing"]["devices"].as_array(),
        ) && raw.len() != parsed.len()
        {
            out.push("Raw and parsed usbmuxd device counts differ. Inspect NetworkAddress parsing and the discovery timeline for a device transition.".into());
        }
        for probe in snapshot["probes"].as_array().into_iter().flatten() {
            if probe["values"]["EnableWifiConnections"] == false {
                out.push("The phone reported EnableWifiConnections=false. The Wi-Fi setting is not enabled on the phone.".into());
            }
            if probe["checks"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| c["stage"] == "lockdown.start_session" && c["ok"] == false)
            {
                out.push("A pairing/session handshake failed; inspect lockdown.start_session's original error.".into());
            }
        }
    }
    if let Some(last) = snapshots.last() {
        if report["wifi_observation"] == true {
            let selected_usb = |s: &&Value| {
                s["listing"]["devices"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|d| {
                        d["connection"] == "usb"
                            && (report["selected_udid"].is_null()
                                || d["udid"] == report["selected_udid"])
                    })
            };
            if !snapshots.first().is_some_and(selected_usb) {
                out.push("No USB baseline was available for the selected phone. Run --wifi with USB connected to capture its wireless settings before unplugging.".into());
            }
            if selected_usb(last) {
                out.push("USB was still listed after observation. The reproduction may have used USB; the wireless path was not isolated.".into());
            }
        }
        let has_wifi = last["listing"]["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|d| {
                d["connection"] == "wifi"
                    && (report["selected_udid"].is_null() || d["udid"] == report["selected_udid"])
            });
        if !has_wifi && last["listing"]["ok"] == true {
            let bonjour = &report["bonjour"];
            if bonjour["error"].is_string()
                || (bonjour["timed_out"] != true && bonjour["exit_code"] != 0)
            {
                out.push("Bonjour could not be tested (dns-sd unavailable or failed). This is not evidence that the phone did not advertise.".into());
            } else if !browse_instances(bonjour["stdout"].as_str().unwrap_or("")).is_empty() {
                out.push("usbmuxd has no Wi-Fi entry for the selected phone, while mobdev2 advertisements were seen. Verify their identity, then investigate host registration/pairing.".into());
            } else {
                out.push("No Wi-Fi usbmuxd entry or mobdev2 advertisement was observed. Compare the phone's wireless setting, network multicast, and host discovery on another Wi-Fi network.".into());
            }
        }
    }
    if report["reproduction"]["timed_out"] == true {
        out.push("The reproduction command timed out. It was run only once; its action may still have reached the phone. Inspect the output before retrying.".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bonjour_names_with_spaces_and_interface_survive_parsing() {
        let browse = "12:34:56.000 Add 2 6 local. _apple-mobdev2._tcp. aa:bb@Phone with spaces\n12:34:57.000 Rmv 0 6 local. _apple-mobdev2._tcp. another\n";
        assert_eq!(browse_instances(browse), ["aa:bb@Phone with spaces"]);
        assert_eq!(
            service_endpoint("aa:bb can be reached at iPhone.local.:62078 (interface 6)"),
            Some(("iPhone.local".into(), 62078, Some(6)))
        );
    }

    #[test]
    fn missing_bonjour_tool_is_not_reported_as_missing_phone_advertisement() {
        let report = json!({ "snapshots": [{ "listing": { "ok": true, "devices": [] } }],
            "bonjour": { "error": "No such file or directory" } });
        let result = findings(&report).join("\n");
        assert!(result.contains("could not be tested"));
        assert!(!result.contains("No Wi-Fi usbmuxd entry or mobdev2 advertisement was observed"));
        let mut report = report;
        report["bonjour"] =
            json!({ "exit_code": 1, "stderr": "DNSServiceBrowse failed: NotPermitted" });
        assert!(findings(&report).join("\n").contains("could not be tested"));
    }

    #[test]
    fn plist_values_keep_wireless_booleans_and_raw_address_bytes() {
        assert_eq!(
            serde_json::to_value(plist::Value::Boolean(false)).unwrap(),
            false
        );
        assert_eq!(
            serde_json::to_value(plist::Value::Data(vec![16, 2, 0, 0, 192, 168, 1, 4])).unwrap(),
            json!([16, 2, 0, 0, 192, 168, 1, 4])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_process_keeps_both_outputs_and_does_not_wait_for_inherited_pipes() {
        let start = Instant::now();
        let result = capture(
            "/bin/sh",
            &[
                "-c".into(),
                "printf partial-output; printf original-error >&2; sleep 5".into(),
            ],
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["stdout"], "partial-output");
        assert_eq!(result["stderr"], "original-error");
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn capture_drains_large_output_and_retains_the_final_failure() {
        let result = capture(
            "/bin/sh",
            &[
                "-c".into(),
                "head -c 300000 /dev/zero; printf final-error; exit 7".into(),
            ],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(result["exit_code"], 7);
        assert_eq!(result["stdout_truncated"], true);
        assert!(result["stdout"].as_str().unwrap().ends_with("final-error"));
        assert_eq!(result["timed_out"], false);
    }
}
