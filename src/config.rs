//! `~/.config/iph/config.toml`.

use serde::Deserialize;

use crate::paths;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Device to use when `--udid` is not given.
    pub udid: Option<String>,
    pub dnd: DndConfig,
    pub daemon: DaemonConfig,
    pub script: ScriptConfig,
}

/// Limits for `iph script` and `iph run`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ScriptConfig {
    /// A script is stopped after this many seconds (at most 840).
    pub timeout_secs: u64,
    /// A script is stopped when it holds more than this many megabytes.
    pub memory_mb: u64,
}

impl Default for ScriptConfig {
    fn default() -> Self {
        Self { timeout_secs: 300, memory_mb: 64 }
    }
}

/// `iph dnd`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DndConfig {
    /// The Focus to switch to instead of Do Not Disturb, by the name the
    /// phone shows for it.
    pub focus: Option<String>,
    /// Label of the Focus control in Control Center, for a phone whose
    /// language is neither Korean nor English.
    pub control: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Close the touch-input media session after this many idle seconds.
    /// 0 closes it at the end of every command.
    pub input_idle_secs: u64,
    /// Exit the daemon after this many idle seconds.
    pub idle_exit_secs: u64,
    /// Also take the screen video while an input session is open. Touches
    /// count without it, so it is off: the video is megabits a second through
    /// the same tunnel as everything else.
    pub screen_video: bool,
    /// When the phone's accessibility daemon stops answering, end it so the
    /// phone starts a new one, and read again.
    pub restart_accessibility: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            input_idle_secs: 120,
            idle_exit_secs: 1800,
            screen_video: false,
            restart_accessibility: true,
        }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Config> {
        let path = paths::config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_config() {
        let c: Config = toml::from_str("udid = \"X\"\n[daemon]\ninput_idle_secs = 0\n").unwrap();
        assert_eq!(c.udid.as_deref(), Some("X"));
        assert_eq!(c.daemon.input_idle_secs, 0);
        assert_eq!(c.daemon.idle_exit_secs, 1800);
        assert_eq!((c.script.timeout_secs, c.script.memory_mb), (300, 64));
    }

    #[test]
    fn dnd_can_point_at_another_focus() {
        let c: Config = toml::from_str("[dnd]\nfocus = \"업무\"\n").unwrap();
        assert_eq!(c.dnd.focus.as_deref(), Some("업무"));
    }

    #[test]
    fn an_old_safety_section_is_ignored() {
        let c: Config = toml::from_str("[safety]\nonly_apps = [\"com.apple.Preferences\"]\ndnd = false\n").unwrap();
        assert!(c.dnd.focus.is_none());
    }
}
