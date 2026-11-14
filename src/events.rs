//! Small, persistent connection events, including failures before a daemon exists.
//! No pairing records or request payloads are logged here.

use std::{
    future::Future,
    io::Write,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::paths;

const MAX_FILE_BYTES: u64 = 512 * 1024;
const KEEP_FILES: usize = 32;
static WRITER: Mutex<()> = Mutex::new(());

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn record(stage: &str, status: &str, elapsed_ms: u128, details: Value) {
    let Ok(_guard) = WRITER.lock() else { return };
    // Diagnostics must never turn a successful phone command into a failure.
    let _ = append(
        &paths::state_dir().join("events"),
        &json!({
            "at": crate::clock::now_utc(), "unix_ms": now_ms(), "pid": std::process::id(),
            "stage": stage, "status": status, "elapsed_ms": elapsed_ms, "details": details,
        }),
    );
}

fn append(dir: &Path, event: &Value) -> std::io::Result<()> {
    paths::ensure_private_dir(dir)?;
    let path = dir.join(format!("{}.jsonl", std::process::id()));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() >= MAX_FILE_BYTES) {
        let previous = path.with_extension("previous.jsonl");
        let _ = std::fs::remove_file(&previous);
        std::fs::rename(&path, previous)?;
    }
    let mut file = paths::private_file()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{event}")?;
    // Each process owns its files, avoiding interleaved writes and rotation
    // races between the CLI and daemon. Keep recent files across processes.
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|s| s == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by_key(|(modified, _)| *modified);
    let excess = files.len().saturating_sub(KEEP_FILES);
    for (_, old) in files.into_iter().take(excess) {
        if old != path {
            let _ = std::fs::remove_file(old);
        }
    }
    Ok(())
}

/// Time and retain the full error for one connection step.
pub async fn step<T, E: std::fmt::Debug>(
    name: &str,
    details: Value,
    timeout: Duration,
    future: impl Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    let started = Instant::now();
    record(name, "started", 0, details.clone());
    let result = match tokio::time::timeout(timeout, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(anyhow!("{name}: {error:?}")),
        Err(_) => Err(anyhow!(
            "{name}: timed out after {} ms",
            timeout.as_millis()
        )),
    };
    let mut details = json!({ "context": details });
    if let Err(error) = &result {
        details["error"] = json!(format!("{error:#}"));
    }
    record(
        name,
        if result.is_ok() { "ok" } else { "error" },
        started.elapsed().as_millis(),
        details,
    );
    result
}

/// Enable library warnings in CLI commands as well as the resident process.
pub fn init_tracing() {
    let filter = std::env::var("IPH_LOG").unwrap_or_else(|_| "warn".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
