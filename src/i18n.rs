//! Guidance text follows the Mac's language: Korean on a Korean Mac, English
//! otherwise. JSON keys and error codes stay English.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ko,
    En,
}

fn detect() -> Lang {
    if let Ok(v) = std::env::var("IPH_LANG") {
        return if v.to_lowercase().starts_with("ko") { Lang::Ko } else { Lang::En };
    }
    // The system language list is the truth on macOS; the shell's LANG often
    // stays en_US on a Korean Mac.
    if let Ok(out) = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(first) = text
            .lines()
            .map(|l| l.trim().trim_matches(|c| c == '"' || c == ','))
            .find(|l| !l.is_empty() && *l != "(" && *l != ")")
        {
            return if first.to_lowercase().starts_with("ko") { Lang::Ko } else { Lang::En };
        }
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(key)
            && !v.is_empty() {
                return if v.to_lowercase().starts_with("ko") { Lang::Ko } else { Lang::En };
            }
    }
    Lang::En
}

pub fn lang() -> Lang {
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(detect)
}

/// Pick the text for the current language.
pub fn t<'a>(ko: &'a str, en: &'a str) -> &'a str {
    match lang() {
        Lang::Ko => ko,
        Lang::En => en,
    }
}
