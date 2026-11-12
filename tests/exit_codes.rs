//! The exit code of `iph batch` and `iph script` follows the outcome, as the
//! README's table says: a caller that only looks at the exit code must not
//! take an unsent run for a success. No phone is involved: the runs are
//! refused before anything goes to one (a step or script that cannot run), on
//! a made-up udid with its own daemon and folders.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

const NO_SUCH_PHONE: &str = "00000000-TESTONLY-NOPHONE";

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        // Short: the daemon's socket lives here, and socket paths are limited
        // to about 100 bytes (the usual temporary folder on a Mac is too deep).
        let dir = PathBuf::from(format!("/tmp/iph-exit-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(
            dir.join("config/config.toml"),
            format!("udid = \"{NO_SUCH_PHONE}\"\n"),
        )
        .unwrap();
        Sandbox { dir }
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    /// Run `iph <args>`: the exit code and the JSON it printed.
    fn iph(&self, args: &[&str]) -> (i32, Value) {
        let out = Command::new(env!("CARGO_BIN_EXE_iph"))
            .args(args)
            .env("IPH_STATE_DIR", self.dir.join("state"))
            .env("IPH_CONFIG_DIR", self.dir.join("config"))
            .env("IPH_UDID", NO_SUCH_PHONE)
            .env_remove("IPH_TRACE")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let value = serde_json::from_str(text.trim()).unwrap_or_else(|_| panic!("not JSON: {text}"));
        (out.status.code().unwrap_or(-1), value)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.iph(&["stop"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn batch_and_script_exit_codes_follow_the_outcome() {
    let s = Sandbox::new();

    let unsent = s.file("unsent.json", r#"[{"fly": "away"}]"#);
    let (code, v) = s.iph(&["batch", path(&unsent)]);
    assert_eq!((code, v["outcome"].as_str()), (1, Some("not_sent")), "{v}");

    let unsent = s.file("unsent.js", r#"throw new Error("stop here")"#);
    let (code, v) = s.iph(&["script", path(&unsent)]);
    assert_eq!((code, v["outcome"].as_str()), (1, Some("not_sent")), "{v}");

    // A script that sends nothing and returns: done, 0.
    let fine = s.file("fine.js", "return { n: 1 + 1 }");
    let (code, v) = s.iph(&["script", path(&fine)]);
    assert_eq!((code, v["ok"].as_bool()), (0, Some(true)), "{v}");
    assert_eq!(v["outputs"]["n"], 2);
}
