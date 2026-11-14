//! Verify the real CLI's setup/doctor requests without contacting a phone.
#![cfg(unix)]

use std::{path::PathBuf, process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
};

struct Sandbox(PathBuf);

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn wireless_setup_requires_an_explicit_flag_in_both_commands() {
    let dir = Sandbox(PathBuf::from(format!(
        "/tmp/iph-setup-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8],
    )));
    std::fs::create_dir_all(&dir.0).unwrap();
    let udid = "NO-PHONE-SETUP-TEST";
    let socket = dir.0.join(format!("{udid}.sock"));
    let listener = UnixListener::bind(&socket).unwrap();

    for command in ["setup", "doctor"] {
        for fix_wifi in [false, true] {
            let mut cli = Command::new(env!("CARGO_BIN_EXE_iph"));
            cli.args(["--udid", udid, command]);
            if fix_wifi {
                cli.arg("--fix-wifi");
            }
            cli.env("IPH_STATE_DIR", &dir.0)
                .env("IPH_CONFIG_DIR", dir.0.join("config"))
                .env("IPH_LANG", "en")
                .env_remove("IPH_TRACE")
                .stdin(Stdio::null())
                .kill_on_drop(true);

            let server = async {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let response = json!({ "ok": true, "ready": true, "checks": [] });
                stream
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
                request
            };
            let (request, output) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(server, cli.output())
            })
            .await
            .expect("CLI did not complete its doctor request");
            let output = output.unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(request["cmd"], "doctor");
            assert_eq!(
                request["args"]["fix_wifi"], fix_wifi,
                "{command}: {request}"
            );
            if command == "setup" {
                let text = String::from_utf8(output.stdout).unwrap();
                assert_eq!(text.contains("--fix-wifi:"), fix_wifi, "{text}");
            }
        }
    }
}
