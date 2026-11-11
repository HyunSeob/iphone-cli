//! `~/.config/iph/config.toml`.

use serde::Deserialize;

use crate::paths;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Device to use when `--udid` is not given.
    pub udid: Option<String>,
    pub safety: SafetyConfig,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// Extra words that need `--confirm`.
    pub add: Vec<String>,
    /// Built-in words to stop guarding.
    pub remove: Vec<String>,
    /// When not empty: bundle ids of the only apps iph may launch or send
    /// input to. Anything else in front blocks the input.
    pub only_apps: Vec<String>,
    /// Switch the phone to Do Not Disturb before the first input, and put
    /// back the Focus that was on once the work is over. A notification
    /// banner would otherwise take a tap meant for the top of the screen.
    pub dnd: bool,
    /// Put the earlier Focus back after this many seconds without a command.
    /// 0 waits for `iph stop` or the daemon's exit.
    pub dnd_idle_secs: u64,
    /// The Focus to switch to instead of Do Not Disturb, by the name the
    /// phone shows for it.
    pub dnd_focus: Option<String>,
    /// Label of the Focus control in Control Center, for a phone whose
    /// language is neither Korean nor English.
    pub dnd_control: Option<String>,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            add: Vec::new(),
            remove: Vec::new(),
            only_apps: Vec::new(),
            dnd: true,
            dnd_idle_secs: 300,
            dnd_focus: None,
            dnd_control: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Close the touch-input media session after this many idle seconds.
    /// 0 closes it at the end of every command.
    pub input_idle_secs: u64,
    /// Exit the daemon after this many idle seconds.
    pub idle_exit_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            input_idle_secs: 120,
            idle_exit_secs: 1800,
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
        let c: Config = toml::from_str(
            "udid = \"X\"\n[safety]\nadd = [\"기부\"]\nremove = [\"보내기\"]\nonly_apps = [\"com.apple.Preferences\"]\n",
        )
        .unwrap();
        assert_eq!(c.udid.as_deref(), Some("X"));
        assert_eq!(c.safety.add, vec!["기부"]);
        assert_eq!(c.safety.only_apps, vec!["com.apple.Preferences"]);
        assert_eq!(c.daemon.input_idle_secs, 120);
        assert!(c.safety.dnd);
        assert_eq!(c.safety.dnd_idle_secs, 300);
        assert_eq!((c.script.timeout_secs, c.script.memory_mb), (300, 64));
    }

    #[test]
    fn dnd_can_be_turned_off_or_pointed_at_another_focus() {
        let c: Config = toml::from_str("[safety]\ndnd = false\ndnd_focus = \"업무\"\n").unwrap();
        assert!(!c.safety.dnd);
        assert_eq!(c.safety.dnd_focus.as_deref(), Some("업무"));
    }
}
