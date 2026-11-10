//! Guard against pressing something that pays, sends or orders.
//!
//! A tap whose target label contains a danger word is refused until the caller
//! passes `--confirm <op_id>`. An `op_id` works once: it is written to
//! `ops.jsonl` before the tap goes out, so a retry with the same id is refused
//! even if the first attempt crashed halfway.
//!
//! Limit: this reads labels. A button with no label, or labelled with other
//! words, is not caught.

use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{config::SafetyConfig, paths};

pub const DEFAULT_WORDS: &[&str] = &[
    "결제",
    "결제하기",
    "송금",
    "보내기",
    "이체",
    "주문",
    "주문하기",
    "구매",
    "구매하기",
    "결제 완료",
    "지불",
    "충전",
    "pay",
    "purchase",
    "buy",
    "send money",
    "transfer",
    "place order",
];

#[derive(Debug, Clone)]
pub struct Guard {
    words: Vec<String>,
}

impl Guard {
    pub fn new(cfg: &SafetyConfig) -> Self {
        let removed: Vec<String> = cfg.remove.iter().map(|w| w.trim().to_lowercase()).collect();
        let mut words: Vec<String> = DEFAULT_WORDS
            .iter()
            .map(|w| w.to_string())
            .chain(cfg.add.iter().map(|w| w.trim().to_lowercase()))
            .filter(|w| !w.is_empty() && !removed.contains(w))
            .collect();
        // Longest first, so the report names the most specific word.
        words.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));
        words.dedup();
        Self { words }
    }

    pub fn words(&self) -> &[String] {
        &self.words
    }

    /// The danger word found in `label`, if any.
    pub fn check(&self, label: &str) -> Option<&str> {
        let text = label.to_lowercase();
        self.words
            .iter()
            .find(|w| contains_word(&text, w))
            .map(|w| w.as_str())
    }
}

/// Latin words must stand alone ("pay" is not in "display"); Korean words
/// match anywhere, because Korean attaches endings ("결제하기", "송금하세요").
fn contains_word(text: &str, word: &str) -> bool {
    if !word.is_ascii() {
        return text.contains(word);
    }
    let mut from = 0;
    while let Some(pos) = text[from..].find(word) {
        let start = from + pos;
        let end = start + word.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
        if boundary(before) && boundary(after) {
            return true;
        }
        from = end;
    }
    false
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpRecord {
    pub op_id: String,
    /// UTC, ISO 8601.
    pub at: String,
    pub udid: String,
    /// "started", then one of "done", "unknown", "not_sent".
    pub state: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_word: Option<String>,
}

pub fn valid_op_id(op_id: &str) -> bool {
    !op_id.is_empty()
        && op_id.len() <= 128
        && op_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:#".contains(c))
}

/// UTC timestamp like `2026-11-10T13:05:09Z`.
pub fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_utc(secs)
}

fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

struct Locked(File);

impl Locked {
    fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            paths::ensure_private_dir(dir)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        // SAFETY: flock on a file descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("cannot lock {}", path.display());
        }
        Ok(Self(file))
    }

    fn records(&mut self) -> Result<Vec<OpRecord>> {
        self.0.seek(SeekFrom::Start(0))?;
        Ok(BufReader::new(&self.0)
            .lines()
            .map_while(|l| l.ok())
            .filter_map(|l| serde_json::from_str::<OpRecord>(&l).ok())
            .collect())
    }

    fn append(&mut self, rec: &OpRecord) -> Result<()> {
        let mut line = serde_json::to_string(rec)?;
        line.push('\n');
        self.0.write_all(line.as_bytes())?;
        self.0.sync_data()?;
        Ok(())
    }
}

/// Every record written for `op_id`, oldest first.
pub fn lookup(op_id: &str) -> Result<Vec<OpRecord>> {
    lookup_in(&paths::ops_log(), op_id)
}

pub fn lookup_in(path: &Path, op_id: &str) -> Result<Vec<OpRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut log = Locked::open(path)?;
    Ok(log
        .records()?
        .into_iter()
        .filter(|r| r.op_id == op_id)
        .collect())
}

/// Claim `op_id` for one operation. Fails with the earlier records if the id
/// was used before.
pub fn begin(rec: &OpRecord) -> Result<std::result::Result<(), Vec<OpRecord>>> {
    begin_in(&paths::ops_log(), rec)
}

pub fn begin_in(path: &Path, rec: &OpRecord) -> Result<std::result::Result<(), Vec<OpRecord>>> {
    let mut log = Locked::open(path)?;
    let earlier: Vec<OpRecord> = log
        .records()?
        .into_iter()
        .filter(|r| r.op_id == rec.op_id)
        .collect();
    if !earlier.is_empty() {
        return Ok(Err(earlier));
    }
    log.append(rec)?;
    Ok(Ok(()))
}

/// Record how a claimed operation ended.
pub fn finish(rec: &OpRecord) -> Result<()> {
    finish_in(&paths::ops_log(), rec)
}

pub fn finish_in(path: &Path, rec: &OpRecord) -> Result<()> {
    Locked::open(path)?.append(rec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> Guard {
        Guard::new(&SafetyConfig::default())
    }

    #[test]
    fn korean_words_match_inside_longer_labels() {
        let g = guard();
        assert_eq!(g.check("12,000원 결제하기"), Some("결제하기"));
        assert_eq!(g.check("송금"), Some("송금"));
        assert_eq!(g.check("친구에게 돈 보내기"), Some("보내기"));
        assert_eq!(g.check("주문 내역"), Some("주문"));
        assert_eq!(g.check("일반"), None);
        assert_eq!(g.check("소비 내역"), None);
    }

    #[test]
    fn latin_words_need_word_boundaries() {
        let g = guard();
        assert_eq!(g.check("Pay"), Some("pay"));
        assert_eq!(g.check("Apple Pay"), Some("pay"));
        assert_eq!(g.check("Buy now"), Some("buy"));
        assert_eq!(g.check("Send Money"), Some("send money"));
        assert_eq!(g.check("Display & Brightness"), None);
        assert_eq!(g.check("Payment history"), None);
        assert_eq!(g.check("Buyer protection"), None);
    }

    #[test]
    fn config_adds_and_removes_words() {
        let g = Guard::new(&SafetyConfig {
            add: vec!["기부".into(), "Donate".into()],
            remove: vec!["보내기".into()],
        });
        assert_eq!(g.check("기부하기"), Some("기부"));
        assert_eq!(g.check("donate"), Some("donate"));
        assert_eq!(g.check("메모 보내기"), None);
    }

    #[test]
    fn op_id_is_single_use() {
        let dir = std::env::temp_dir().join(format!("iph-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("ops.jsonl");
        let rec = |state: &str| OpRecord {
            op_id: "pay-001".into(),
            at: now_utc(),
            udid: "TEST".into(),
            state: state.into(),
            command: "tap".into(),
            target: Some("label=\"결제하기\"".into()),
            matched_word: Some("결제하기".into()),
        };
        assert!(lookup_in(&path, "pay-001").unwrap().is_empty());
        assert!(begin_in(&path, &rec("started")).unwrap().is_ok());
        finish_in(&path, &rec("done")).unwrap();
        // Second use is refused and shows what happened the first time.
        let earlier = begin_in(&path, &rec("started")).unwrap().unwrap_err();
        assert_eq!(earlier.len(), 2);
        assert_eq!(earlier[1].state, "done");
        // A different id is fine.
        let mut other = rec("started");
        other.op_id = "pay-002".into();
        assert!(begin_in(&path, &other).unwrap().is_ok());
        assert_eq!(lookup_in(&path, "pay-001").unwrap().len(), 2);
        // The record is private to the user.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn op_id_shape() {
        assert!(valid_op_id("toss-2026-11-11_01"));
        assert!(!valid_op_id(""));
        assert!(!valid_op_id("has space"));
        assert!(!valid_op_id(&"x".repeat(200)));
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_794_316_509), "2026-11-10T13:15:09Z");
    }
}
