//! A script request through the path a real phone takes (`Session::serve_script`).
//! No phone is involved: the script below makes no call to one.

#![allow(clippy::result_large_err)]

use iphone_cli::{config::Config, device::session::Session, script};
use serde_json::json;

const NO_SUCH_PHONE: &str = "00000000-TESTONLY-NOPHONE";

#[tokio::test]
async fn a_request_carries_its_arguments_and_the_saved_scripts_name() {
    let dir = std::env::temp_dir().join(format!("iph-script-{}", uuid::Uuid::new_v4()));
    unsafe {
        std::env::set_var("IPH_STATE_DIR", &dir);
        std::env::set_var("IPH_CONFIG_DIR", dir.join("config"));
    }
    let mut s = Session::new(NO_SUCH_PHONE.into(), Config::default());
    let req = script::request("return { n: Number(args.n) + 1 }", json!({"n": "41"}).as_object().unwrap(), Some(("answer", "builtin:answer")));
    let out = s.serve_script(&req).await.unwrap();
    assert_eq!(out["outputs"], json!({"n": 42}));
    assert_eq!((out["script"].as_str(), out["source"].as_str()), (Some("answer"), Some("builtin:answer")));
    assert_eq!(s.serve_script(&json!({})).await.unwrap_err().code, "invalid_argument");
}
