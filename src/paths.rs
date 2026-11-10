//! Where iph keeps its files.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Sockets, logs and the operation record. `IPH_STATE_DIR` overrides it (tests).
pub fn state_dir() -> PathBuf {
    std::env::var_os("IPH_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("Library/Application Support/iph"))
}

/// `config.toml` and personal recipes. `IPH_CONFIG_DIR` overrides it (tests).
pub fn config_dir() -> PathBuf {
    std::env::var_os("IPH_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config/iph"))
}

pub fn socket(udid: &str) -> PathBuf {
    state_dir().join(format!("{udid}.sock"))
}

pub fn daemon_log(udid: &str) -> PathBuf {
    state_dir().join(format!("{udid}.log"))
}

pub fn ops_log() -> PathBuf {
    state_dir().join("ops.jsonl")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn personal_recipes() -> PathBuf {
    config_dir().join("recipes")
}

/// Create a directory only the user can enter.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}
