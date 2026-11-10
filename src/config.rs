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
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SafetyConfig {
    /// Extra words that need `--confirm`.
    pub add: Vec<String>,
    /// Built-in words to stop guarding.
    pub remove: Vec<String>,
    /// When not empty: bundle ids of the only apps iph may launch or send
    /// input to. Anything else in front blocks the input.
    pub only_apps: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Close the touch-input media session after this many idle seconds.
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
    }
}
