//! Where iph keeps its files.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Sockets, logs and the operation record. `IPH_STATE_DIR` overrides it (tests).
/// `~/Library/Application Support/iph` on macOS, `%LOCALAPPDATA%\iph` on Windows.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("IPH_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData/Local"))
            .join("iph");
    }
    home().join("Library/Application Support/iph")
}

/// `config.toml` and personal scripts. `IPH_CONFIG_DIR` overrides it (tests).
/// `~/.config/iph` on macOS, `%APPDATA%\iph` on Windows.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("IPH_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        return std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData/Roaming"))
            .join("iph");
    }
    home().join(".config/iph")
}

/// Developer disk images fetched when no Xcode copy is around.
pub fn ddi_cache() -> PathBuf {
    state_dir().join("ddi")
}

pub fn socket(udid: &str) -> PathBuf {
    state_dir().join(format!("{udid}.sock"))
}

pub fn daemon_pid(udid: &str) -> PathBuf {
    state_dir().join(format!("{udid}.pid"))
}

pub fn daemon_log(udid: &str) -> PathBuf {
    state_dir().join(format!("{udid}.log"))
}

/// The Focus `iph dnd on` replaced with Do Not Disturb on this phone, for `off`.
pub fn dnd_record(udid: &str) -> PathBuf {
    state_dir().join(format!("{udid}.dnd.json"))
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn personal_scripts() -> PathBuf {
    config_dir().join("scripts")
}

/// Create a directory only the user can enter. On Windows the folders under
/// the user's profile are private already.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Open options for a file only the user can read (0600 on macOS).
pub fn private_file() -> std::fs::OpenOptions {
    #[allow(unused_mut)]
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
