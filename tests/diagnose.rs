//! Exercise the real CLI against a tiny usbmuxd and host-tool fixtures.
//! No physical phone, host daemon, or actual network discovery is used.
#![cfg(unix)]

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

const PHONE: &str = "00000000-DIAG-TEST-PHONE";

struct Fixture {
    dir: PathBuf,
    address: String,
    done: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(disappear: bool, malformed: bool, bonjour: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("iph-diag-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::create_dir_all(dir.join("config")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let shutdown = done.clone();
        let lists = Arc::new(AtomicUsize::new(0));
        let server = thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let lists = lists.clone();
                        thread::spawn(move || {
                            let _ = serve(stream, lists, disappear, malformed);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        let fixture = Self {
            dir,
            address,
            done,
            server: Some(server),
        };
        for tool in ["sw_vers", "uname", "ifconfig", "scutil", "route", "log"] {
            fixture.tool(tool, "printf 'host fixture\\n'\n");
        }
        if bonjour {
            let port = fixture.address.rsplit_once(':').unwrap().1;
            fixture.tool("dns-sd", &format!(
                "case \"$1\" in\n-B) printf '12:34:56.000 Add 2 6 local. _apple-mobdev2._tcp. advertised phone with spaces\\n' ;;\n-L) printf 'service can be reached at 127.0.0.1:{port} (interface 6)\\n' ;;\nesac\n"));
        }
        fixture
    }

    fn tool(&self, name: &str, script: &str) {
        let path = self.dir.join("tools").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_iph"));
        cmd.current_dir(&self.dir)
            .env("PATH", self.dir.join("tools"))
            .env("IPH_STATE_DIR", self.dir.join("state"))
            .env("IPH_CONFIG_DIR", self.dir.join("config"))
            .env("USBMUXD_SOCKET_ADDRESS", &self.address)
            .env_remove("IPH_UDID")
            .env_remove("IPH_TRACE")
            .env_remove("IPH_LOG")
            .env_remove("IPH_PREFER")
            .env_remove("IPH_PCAP");
        cmd
    }

    fn run(&self, args: &[&str]) -> (Value, Value) {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        let text = std::fs::read(response["report"].as_str().unwrap()).unwrap();
        let report = serde_json::from_slice(&text).unwrap();
        (response, report)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        self.server.take().unwrap().join().unwrap();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn serve(
    mut stream: TcpStream,
    lists: Arc<AtomicUsize>,
    disappear: bool,
    malformed: bool,
) -> std::io::Result<()> {
    // macOS may inherit O_NONBLOCK from the listening socket.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    loop {
        let mut header = [0; 16];
        stream.read_exact(&mut header)?;
        let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        if !(16..1024 * 1024).contains(&length) {
            panic!("bad frame {header:?}");
        }
        let mut data = vec![0; length - 16];
        stream.read_exact(&mut data)?;
        let request = plist::Value::from_reader(std::io::Cursor::new(data)).unwrap();
        let request = request.as_dictionary().unwrap();
        let kind = request
            .get("MessageType")
            .and_then(|v| v.as_string())
            .unwrap_or("");
        let response = match kind {
            "ListDevices" if malformed => {
                json!({ "DeviceList": "broken-list-for-original-error-test" })
            }
            "ListDevices" => {
                let baseline = lists.fetch_add(1, Ordering::Relaxed) < 2;
                if disappear && baseline {
                    json!({ "DeviceList": [{ "DeviceID": 7, "Properties": { "SerialNumber": PHONE, "ConnectionType": "USB" } }] })
                } else {
                    json!({ "DeviceList": [] })
                }
            }
            "Connect" => json!({ "MessageType": "Result", "Number": 0 }),
            _ => json!({ "MessageType": "Result", "Number": 2 }),
        };
        let value = plist::to_value(&response).unwrap();
        let mut payload = Vec::new();
        value.to_writer_xml(&mut payload).unwrap();
        let total = (payload.len() + 16) as u32;
        stream.write_all(&total.to_le_bytes())?;
        stream.write_all(&header[4..])?;
        stream.write_all(&payload)?;
        if kind == "Connect" {
            // From here on, the stream is a lockdown service with 4-byte BE
            // plist lengths. Return facts without pairing or modifying it.
            loop {
                let mut length = [0; 4];
                stream.read_exact(&mut length)?;
                let mut payload = vec![0; u32::from_be_bytes(length) as usize];
                stream.read_exact(&mut payload)?;
                let request = plist::Value::from_reader(std::io::Cursor::new(payload)).unwrap();
                let key = request
                    .as_dictionary()
                    .unwrap()
                    .get("Key")
                    .and_then(|v| v.as_string())
                    .unwrap_or("");
                let value = match key {
                    "EnableWifiConnections" | "EnableWifiDebugging" | "PasswordProtected" => {
                        json!(false)
                    }
                    "ProductVersion" => json!("27.0.1"),
                    "ProductType" => json!("iPhoneTest,1"),
                    _ => json!("fixture"),
                };
                let mut payload = Vec::new();
                plist::to_value(&json!({ "Value": value }))
                    .unwrap()
                    .to_writer_xml(&mut payload)
                    .unwrap();
                stream.write_all(&(payload.len() as u32).to_be_bytes())?;
                stream.write_all(&payload)?;
            }
        }
    }
}

#[test]
fn no_device_report_keeps_original_failed_command_and_diagnostics_without_daemon() {
    let fixture = Fixture::new(false, false, false);
    let (response, report) = fixture.run(&["diagnose", "--", "read"]);
    assert_eq!(response["ok"], true);
    assert_eq!(report["status"], "complete");
    assert!(report["selected_udid"].is_null());
    assert_eq!(report["reproduction"]["exit_code"], 1);
    let original: Value =
        serde_json::from_str(report["reproduction"]["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(original["error"]["code"], "no_device", "{original:#}");
    assert!(report["bonjour"]["error"].is_string());
    let text = report.to_string();
    assert!(text.contains("device_selection"));
    assert!(text.contains("could not be tested"));
    assert!(
        !fixture.dir.join("state").read_dir().unwrap().any(|e| e
            .unwrap()
            .path()
            .extension()
            .is_some_and(|s| s == "pid"))
    );
}

#[test]
fn wifi_observation_preserves_usb_facts_after_device_disappears() {
    let fixture = Fixture::new(true, false, true);
    let (_, report) = fixture.run(&[
        "diagnose",
        "--wifi",
        "--duration",
        "2",
        "-o",
        "wifi.txt",
        "--",
        "devices",
    ]);
    assert_eq!(report["selected_udid"], PHONE, "{}", report["snapshots"]);
    assert_eq!(
        report["snapshots"][0]["probes"][0]["values"]["EnableWifiConnections"],
        false
    );
    assert_eq!(report["snapshots"][1]["listing"]["devices"], json!([]));
    assert_eq!(
        report["discovery_timeline"][0]["listing"]["devices"],
        json!([])
    );
    assert_eq!(
        report["bonjour"]["resolved_services"]["services"][0]["tcp_probes"][0]["ok"],
        true
    );
    let text = report.to_string();
    assert!(text.contains("EnableWifiConnections=false"));
    assert!(text.contains("Verify their identity"));
    assert!(text.contains("no cached Wi-Fi address"));
    assert!(
        report["reproduction"]["args"]
            .as_array()
            .unwrap()
            .contains(&json!(PHONE))
    );
}

#[test]
fn malformed_listing_and_invalid_config_still_produce_an_attachment() {
    let fixture = Fixture::new(false, true, false);
    std::fs::write(fixture.dir.join("config/config.toml"), "not valid = [").unwrap();
    let (_, report) = fixture.run(&["diagnose"]);
    assert_eq!(report["status"], "complete");
    assert!(report["config"]["error"].is_string());
    assert_eq!(
        report["snapshots"][0]["raw_usbmuxd"]["response"]["DeviceList"],
        "broken-list-for-original-error-test"
    );
    assert_eq!(report["snapshots"][0]["listing"]["ok"], false);
    assert!(
        report["snapshots"][0]["listing"]["error"]
            .as_str()
            .unwrap()
            .contains("usbmuxd ListDevices")
            || report["snapshots"][0]["listing"]["error"]
                .as_str()
                .unwrap()
                .contains("usbmuxd.ListDevices")
    );
}

#[test]
fn unreachable_usbmuxd_still_saves_the_original_connection_error() {
    let fixture = Fixture::new(false, false, false);
    let output = fixture
        .command()
        .env("USBMUXD_SOCKET_ADDRESS", "127.0.0.1:0")
        .args(["diagnose", "-o", "unreachable.txt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value =
        serde_json::from_slice(&std::fs::read(fixture.dir.join("unreachable.txt")).unwrap())
            .unwrap();
    assert_eq!(report["snapshots"][0]["listing"]["ok"], false);
    assert!(
        report["snapshots"][0]["listing"]["error"]
            .as_str()
            .unwrap()
            .contains("usbmuxd.connect")
    );
    assert!(
        report["findings"]
            .to_string()
            .contains("usbmuxd could not be reached")
    );
}

#[test]
fn interrupted_observation_saves_a_readable_partial_report() {
    let fixture = Fixture::new(false, false, true);
    let child = fixture
        .command()
        .args([
            "diagnose",
            "--wifi",
            "--duration",
            "30",
            "-o",
            "interrupted.txt",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let path = fixture.dir.join("interrupted.txt");
    while Instant::now() < deadline {
        let value = std::fs::read(&path)
            .ok()
            .and_then(|text| serde_json::from_slice::<Value>(&text).ok());
        if value.is_some_and(|v| v["snapshots"].as_array().is_some_and(|s| !s.is_empty())) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    // Only the diagnostic subprocess receives SIGINT.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(report["status"], "interrupted");
    assert!(report["recent_events"].is_array());
}

#[test]
fn existing_report_is_preserved_and_nested_collectors_are_refused() {
    let fixture = Fixture::new(false, false, false);
    let path = fixture.dir.join("existing.txt");
    std::fs::write(&path, "previous evidence").unwrap();
    let output = fixture
        .command()
        .args(["diagnose", "-o", "existing.txt"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "previous evidence");
    let output = fixture
        .command()
        .args(["diagnose", "--", "--udid", PHONE, "diagnose"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("cannot be nested"));
}
