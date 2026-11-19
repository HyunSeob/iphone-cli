//! `iph syslog`: the phone's log stream, kept in the daemon while a test
//! runs, so what an app logged after a tap can be read back.
//!
//! One capture per daemon. The stream is read by a task of its own that never
//! touches the session, so commands keep working while it runs. An entry that
//! passes the filters goes into a ring buffer bounded by entry count and by
//! bytes and gets a `seq` (1 for the first). `read` returns what is newer than
//! a `seq` the caller already has and says where to continue; the buffer goes
//! with the capture. With `output` the kept entries are also written to a
//! JSON Lines file: a `start` line, the entries, and an `end` line that says
//! why the capture ended. A file without an `end` line belongs to a daemon
//! that was killed.

use std::{
    collections::VecDeque,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use idevice::{
    IdeviceError,
    core_device::AppListEntry,
    os_trace_relay::{LogLevel, OsTraceLog, OsTraceRelayReceiver},
};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::{
    clock,
    device::session::{Session, plain_path},
    fields, paths,
    reply::{Fail, Res},
    tcp::handle::Reset,
};

/// The buffer's defaults. Unfiltered over Wi-Fi the phone sent about 4,800
/// entries/s, 1.4 MB/s, 290 B an entry as JSON; 100,000 entries hold about
/// 20 s of that worst case, longer than a sensible gap between a tap and the
/// `read` after it, and minutes of a filtered capture. In memory an entry
/// weighs more than its JSON (see `Entry::weight`): about 430 B for one of
/// 290 B of JSON, so 100,000 of them take about 41 MiB. The 64 MiB limit
/// binds once messages average more than about 540 B.
const DEFAULT_BUFFER_ENTRIES: u64 = 100_000;
const MAX_BUFFER_ENTRIES: u64 = 1_000_000;
const DEFAULT_BUFFER_MB: u64 = 64;
const MAX_BUFFER_MB: u64 = 1024;
const DEFAULT_MAX_MB: u64 = 64;
/// The most `max_mb` takes. A part is read back whole by whoever opens it.
const MAX_FILE_MB: u64 = 4096;
const DEFAULT_MATCHES: u64 = 50;
const MAX_MATCHES: u64 = 1000;
/// Longest message a reply returns; the file keeps the whole message.
const MATCH_CHARS: usize = 500;
/// How long a written line may wait in the buffer before it is flushed.
const FLUSH_EVERY: Duration = Duration::from_millis(250);
const FILE_BUFFER: usize = 32 * 1024;

/// Severity, lowest first. The device's own codes are not in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Notice,
    Error,
    Fault,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Notice => "notice",
            Level::Error => "error",
            Level::Fault => "fault",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "debug" => Level::Debug,
            "info" => Level::Info,
            // Apple's tools call this level "default".
            "notice" | "default" => Level::Notice,
            "error" => Level::Error,
            "fault" => Level::Fault,
            _ => return None,
        })
    }
}

impl From<LogLevel> for Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Debug => Level::Debug,
            LogLevel::Info => Level::Info,
            LogLevel::Notice => Level::Notice,
            LogLevel::Error => Level::Error,
            LogLevel::Fault => Level::Fault,
        }
    }
}

/// One log entry as it is kept. Its `seq` belongs to the buffer, not to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// UTC.
    pub time: String,
    pub pid: u32,
    /// The executable's file name, the same across restarts of the app.
    pub process: String,
    /// The library or executable that logged it.
    pub sender: String,
    pub level: Level,
    pub subsystem: Option<String>,
    pub category: Option<String>,
    pub message: String,
}

/// The library hands the device's microseconds to a constructor that takes
/// nanoseconds (idevice 0.1.68), so the sub-second part arrives as micros.
fn utc_micros(secs: i64, micros: u32) -> String {
    let whole = clock::format_utc(secs);
    format!("{}.{:06}Z", whole.trim_end_matches('Z'), micros.min(999_999))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl From<OsTraceLog> for Entry {
    fn from(log: OsTraceLog) -> Self {
        entry_and_executable(log).0
    }
}

/// The entry, and the path of the executable that logged it, which is only
/// for the filter: the buffer keeps the file name alone.
fn entry_and_executable(log: OsTraceLog) -> (Entry, String) {
    let (subsystem, category) = match log.label {
        Some(l) => (Some(l.subsystem), Some(l.category)),
        None => (None, None),
    };
    let entry = Entry {
        time: utc_micros(log.timestamp.and_utc().timestamp(), log.timestamp.and_utc().timestamp_subsec_nanos()),
        pid: log.pid,
        process: file_name(&log.filename).to_string(),
        sender: file_name(&log.image_name).to_string(),
        level: log.level.into(),
        subsystem,
        category,
        message: log.message,
    };
    (entry, log.filename)
}

impl Entry {
    /// The entry as a reply or the file shows it. A message longer than
    /// `max_chars` is cut and marked with `message_cut`.
    fn value(&self, seq: u64, max_chars: usize) -> Value {
        let mut v = json!({
            "seq": seq,
            "time": self.time,
            "pid": self.pid,
            "process": self.process,
            "sender": self.sender,
            "level": self.level.name(),
        });
        if let Some(s) = &self.subsystem {
            v["subsystem"] = json!(s);
        }
        if let Some(c) = &self.category {
            v["category"] = json!(c);
        }
        if self.message.len() > max_chars && self.message.chars().count() > max_chars {
            v["message"] = json!(self.message.chars().take(max_chars).collect::<String>());
            v["message_cut"] = json!(true);
        } else {
            v["message"] = json!(self.message);
        }
        v
    }

    /// The file's line: the whole message.
    pub fn line(&self, seq: u64) -> String {
        self.value(seq, usize::MAX).to_string()
    }

    /// What the buffer counts against its byte budget: the text, the struct
    /// and a flat allowance for the six allocations behind it. An estimate,
    /// not an allocator's figure; above the entry's JSON.
    fn weight(&self) -> usize {
        const ALLOCATOR_SLACK: usize = 96;
        std::mem::size_of::<Entry>()
            + ALLOCATOR_SLACK
            + self.time.len()
            + self.process.len()
            + self.sender.len()
            + self.subsystem.as_ref().map_or(0, String::len)
            + self.category.as_ref().map_or(0, String::len)
            + self.message.len()
    }
}

/// An installed app, by where it lives on the phone.
#[derive(Debug, Clone, PartialEq)]
pub struct AppBundle {
    pub bundle_id: String,
    /// The bundle directory (`.../Papago.app`), as `plain_path` spells it.
    pub path: String,
}

impl AppBundle {
    /// `None` when the phone gave no absolute path to match against.
    pub fn new(bundle_id: &str, path: &str) -> Option<Self> {
        let path = plain_path(path);
        path.starts_with('/').then(|| Self { bundle_id: bundle_id.to_string(), path })
    }

    /// Whether `executable` (already `plain_path`ed) lies in the bundle: the
    /// app's own executable and its extensions, not a path that only starts
    /// with the same letters.
    fn contains(&self, executable: &str) -> bool {
        executable.strip_prefix(self.path.as_str()).is_some_and(|rest| rest.starts_with('/'))
    }
}

/// The app in `apps` with this bundle id.
fn find_bundle(apps: &[AppListEntry], bundle_id: &str) -> Option<AppBundle> {
    apps.iter().find(|a| a.bundle_identifier == bundle_id).and_then(|a| AppBundle::new(bundle_id, &a.path))
}

/// What is kept on this side. A pid is filtered by the phone instead.
///
/// With neither `processes` nor `apps` every entry passes. With either, an
/// entry passes when it is from any listed process or any listed app;
/// `min_level` then applies on top of that.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub processes: Vec<String>,
    pub apps: Vec<AppBundle>,
    pub min_level: Option<Level>,
}

impl Filter {
    /// `executable` is the logging process's path as the phone gave it.
    pub fn keeps(&self, e: &Entry, executable: &str) -> bool {
        let selected = (self.processes.is_empty() && self.apps.is_empty())
            || self.processes.contains(&e.process)
            || (!self.apps.is_empty() && {
                let executable = plain_path(executable);
                self.apps.iter().any(|app| app.contains(&executable))
            });
        selected && self.min_level.is_none_or(|min| e.level >= min)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub entries: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileSpec {
    pub path: PathBuf,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub processes: Vec<String>,
    /// As given: a bundle id or a name, resolved when the capture starts.
    pub apps: Vec<String>,
    pub min_level: Option<Level>,
    pub pid: Option<u32>,
    pub limits: Limits,
    /// Only when `output` is given: nothing is written to disk otherwise.
    pub file: Option<FileSpec>,
}

#[derive(Debug)]
pub struct Query {
    pub grep: Option<Regex>,
    pub max: usize,
}

#[derive(Debug)]
pub enum Request {
    Start(Spec),
    /// Entries newer than `after` (0: from the start), without stopping.
    Read { after: u64, query: Query },
    /// `None`: report the counts only.
    Stop(Option<Query>),
    Status,
}

/// Parse the arguments every surface sends. A given `output` must be
/// absolute, since the daemon writes it.
pub fn parse(args: &Value) -> Res<Request> {
    let given: Map<String, Value> = match args {
        Value::Null => Map::new(),
        Value::Object(m) => m.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => return Err(Fail::invalid("syslog takes an object of arguments")),
    };
    let action = given
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Fail::invalid("syslog needs an action: start, read, stop or status"))?;
    let allowed: &[&str] = match action {
        "start" => &["action", "process", "app", "pid", "level", "output", "max_mb", "buffer_entries", "buffer_mb"],
        "read" => &["action", "after", "grep", "max"],
        "stop" => &["action", "grep", "max"],
        "status" => &["action"],
        other => return Err(Fail::invalid(format!("unknown syslog action `{other}` (start, read, stop, status)"))),
    };
    if let Some(key) = given.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(Fail::invalid(format!("`syslog {action}` takes no `{key}`")));
    }
    let number = |key: &str| -> Res<Option<u64>> {
        match given.get(key) {
            None => Ok(None),
            Some(v) => v.as_u64().map(Some).ok_or_else(|| Fail::invalid(format!("`{key}` takes a whole number"))),
        }
    };
    let sized = |key: &str, default: u64, most: u64| -> Res<u64> {
        let n = number(key)?.unwrap_or(default);
        if (1..=most).contains(&n) {
            Ok(n)
        } else {
            Err(Fail::invalid(format!("`{key}` takes a whole number from 1 to {most}")))
        }
    };
    let names = |key: &str| -> Res<Vec<String>> {
        match given.get(key) {
            None => Ok(Vec::new()),
            Some(Value::String(s)) => Ok(vec![s.clone()]),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| v.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| Fail::invalid(format!("`{key}` takes names"))),
            Some(_) => Err(Fail::invalid(format!("`{key}` takes a name or a list of names"))),
        }
    };
    let grep = || -> Res<Option<Regex>> {
        match given.get("grep") {
            None => Ok(None),
            Some(v) => {
                let pattern = v.as_str().ok_or_else(|| Fail::invalid("`grep` takes a regular expression"))?;
                Ok(Some(Regex::new(pattern).map_err(|e| Fail::invalid(format!("`grep`: {e}")))?))
            }
        }
    };
    match action {
        "start" => {
            let processes = names("process")?;
            if processes.iter().any(|p| p.is_empty() || p.contains('/')) {
                return Err(Fail::invalid("`process` takes executable names such as Preferences, not paths"));
            }
            let apps = names("app")?;
            if apps.iter().any(|a| a.trim().is_empty()) {
                return Err(Fail::invalid("`app` takes a bundle id or an app's name, as `iph launch` does"));
            }
            let min_level = match given.get("level") {
                None => None,
                Some(v) => Some(v.as_str().and_then(Level::parse).ok_or_else(|| {
                    Fail::invalid("`level` takes debug, info, notice, error or fault")
                })?),
            };
            let pid = match number("pid")? {
                None => None,
                Some(n) => Some(u32::try_from(n).map_err(|_| Fail::invalid("`pid` is out of range"))?),
            };
            let limits = Limits {
                entries: usize::try_from(sized("buffer_entries", DEFAULT_BUFFER_ENTRIES, MAX_BUFFER_ENTRIES)?)
                    .map_err(|_| Fail::invalid("`buffer_entries` is out of range"))?,
                bytes: sized("buffer_mb", DEFAULT_BUFFER_MB, MAX_BUFFER_MB)?
                    .checked_mul(1024 * 1024)
                    .and_then(|b| usize::try_from(b).ok())
                    .ok_or_else(|| Fail::invalid("`buffer_mb` is out of range"))?,
            };
            let file = match given.get("output") {
                None if given.contains_key("max_mb") => {
                    return Err(Fail::invalid("`max_mb` sizes the file that `output` names"));
                }
                None => None,
                Some(v) => {
                    let path = PathBuf::from(v.as_str().ok_or_else(|| Fail::invalid("`output` takes a path"))?);
                    if !path.is_absolute() {
                        return Err(Fail::invalid("`output` must be an absolute path"));
                    }
                    let max_bytes = sized("max_mb", DEFAULT_MAX_MB, MAX_FILE_MB)?
                        .checked_mul(1024 * 1024)
                        .ok_or_else(|| Fail::invalid("`max_mb` is out of range"))?;
                    Some(FileSpec { path, max_bytes })
                }
            };
            Ok(Request::Start(Spec { processes, apps, min_level, pid, limits, file }))
        }
        "read" => {
            let max = number("max")?.unwrap_or(DEFAULT_MATCHES);
            if max == 0 {
                return Err(Fail::invalid("`max` must be at least 1"));
            }
            Ok(Request::Read {
                after: number("after")?.unwrap_or(0),
                query: Query { grep: grep()?, max: max.min(MAX_MATCHES) as usize },
            })
        }
        "stop" => {
            let grep = grep()?;
            let max = number("max")?;
            if grep.is_none() && max.is_none() {
                return Ok(Request::Stop(None));
            }
            let max = max.unwrap_or(DEFAULT_MATCHES).min(MAX_MATCHES) as usize;
            Ok(Request::Stop(Some(Query { grep, max })))
        }
        _ => Ok(Request::Status),
    }
}

/// The kept entries, newest last. Their seqs run without a gap from
/// `first_seq` to `newest_seq`, since only the oldest are evicted, so a seq
/// finds its entry by position.
pub struct Ring {
    entries: VecDeque<Entry>,
    bytes: usize,
    /// The seq of the next entry kept. The first is 1.
    next_seq: u64,
    limits: Limits,
}

/// One `read`: the matching entries oldest first and where to go on.
#[derive(Debug, PartialEq)]
pub struct Page {
    pub entries: Vec<Value>,
    /// Pass it as `after` to the next read. Everything up to it has been
    /// returned or did not match; nothing after it has been looked at, except
    /// when it is the newest seq, which means everything has.
    pub cursor: u64,
    /// `max` cut the page short: another matching entry is already waiting.
    pub more: bool,
    /// Entries newer than `after` that were evicted before this read.
    pub dropped: u64,
}

impl Ring {
    pub fn new(limits: Limits) -> Self {
        Self { entries: VecDeque::new(), bytes: 0, next_seq: 1, limits }
    }

    fn first_seq(&self) -> u64 {
        self.next_seq - self.entries.len() as u64
    }

    fn newest_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// Keep an entry, evicting the oldest while over a limit. An entry larger
    /// than the byte budget evicts itself and everything before it.
    pub fn push(&mut self, entry: Entry) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.bytes += entry.weight();
        self.entries.push_back(entry);
        while self.entries.len() > self.limits.entries || self.bytes > self.limits.bytes {
            let Some(old) = self.entries.pop_front() else { break };
            self.bytes = self.bytes.saturating_sub(old.weight());
        }
        seq
    }

    pub fn usage(&self) -> Value {
        let mut v = json!({
            "entries": self.entries.len(),
            "bytes": self.bytes,
            "max_entries": self.limits.entries,
            "max_bytes": self.limits.bytes,
            "evicted": self.first_seq() - 1,
        });
        if !self.entries.is_empty() {
            v["oldest_seq"] = json!(self.first_seq());
        }
        if self.next_seq > 1 {
            v["newest_seq"] = json!(self.newest_seq());
        }
        v
    }

    /// The entries newer than `after` whose message matches, at most `max`,
    /// oldest first. A cursor is only meaningful in the capture that issued
    /// it, since seqs begin again at 1 with each capture. One that is newer
    /// than every entry returns nothing and gets the newest seq back, so it
    /// cannot run ahead of the buffer.
    pub fn read(&self, after: u64, grep: Option<&Regex>, max: usize) -> Page {
        let first = self.first_seq();
        let newest = self.newest_seq();
        let next = after.saturating_add(1);
        let skip = usize::try_from(next.saturating_sub(first)).unwrap_or(usize::MAX);
        let mut entries = Vec::new();
        let mut last = None;
        let mut more = false;
        for (i, entry) in self.entries.iter().enumerate().skip(skip) {
            if grep.is_some_and(|re| !re.is_match(&entry.message)) {
                continue;
            }
            if entries.len() == max {
                more = true;
                break;
            }
            let seq = first + i as u64;
            entries.push(entry.value(seq, MATCH_CHARS));
            last = Some(seq);
        }
        Page {
            entries,
            cursor: if more { last.unwrap_or(after.min(newest)) } else { newest },
            more,
            dropped: first.saturating_sub(next),
        }
    }

    /// The latest `max` entries whose message matches, oldest first, with
    /// the number that matched in all.
    pub fn latest(&self, grep: Option<&Regex>, max: usize) -> (Vec<Value>, u64) {
        let first = self.first_seq();
        let mut found = VecDeque::new();
        let mut matched = 0;
        for (i, entry) in self.entries.iter().enumerate() {
            if grep.is_some_and(|re| !re.is_match(&entry.message)) {
                continue;
            }
            matched += 1;
            if max == 0 {
                continue;
            }
            if found.len() == max {
                found.pop_front();
            }
            found.push_back(i);
        }
        let entries = found.into_iter().map(|i| self.entries[i].value(first + i as u64, MATCH_CHARS)).collect();
        (entries, matched)
    }
}

/// Where the previous part goes when the file reaches its size.
pub fn rotated(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

fn open_private(path: &Path) -> std::io::Result<BufWriter<File>> {
    let file = paths::private_file().write(true).create(true).truncate(true).open(path)?;
    #[cfg(unix)]
    {
        // `private_file` sets the mode only on a file it creates.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(BufWriter::with_capacity(FILE_BUFFER, file))
}

/// The file, kept under a size: once the next line would pass it, the file
/// becomes `<file>.1` (replacing an older one) and a new file starts.
///
/// Writes are buffered. A line reaches the file when the buffer fills, on
/// `flush` (the capture flushes every `FLUSH_EVERY` and when it ends) and
/// before a rotation. The buffer only ever holds whole lines, so a killed
/// process loses at most the last few, never half of one.
pub struct Sink {
    path: PathBuf,
    /// `None` only after a rotation that could reopen neither part.
    writer: Option<BufWriter<File>>,
    size: u64,
    cap: u64,
    pub rotations: u32,
}

impl Sink {
    pub fn create(path: &Path, cap: u64) -> std::io::Result<Self> {
        let writer = open_private(path)?;
        // A part left from an earlier capture would read as this one's.
        let _ = std::fs::remove_file(rotated(path));
        Ok(Self { path: path.to_path_buf(), writer: Some(writer), size: 0, cap, rotations: 0 })
    }

    pub fn write(&mut self, line: &str) -> std::io::Result<()> {
        if self.size > 0 && self.size + line.len() as u64 + 1 > self.cap {
            self.rotate()?;
        }
        self.append(line)
    }

    /// Without rotating first, for the `end` line: it goes right after the
    /// last entry, even past the size or after a rotation that failed.
    fn append(&mut self, line: &str) -> std::io::Result<()> {
        let bytes = format!("{line}\n");
        self.writer.as_mut().ok_or_else(Self::closed)?.write_all(bytes.as_bytes())?;
        self.size += bytes.len() as u64;
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.writer.as_mut().map_or(Ok(()), |w| w.flush())
    }

    /// On failure the file that holds the last entry stays open, so the
    /// `end` line still lands after it.
    fn rotate(&mut self) -> std::io::Result<()> {
        // Close the file and clear the old `.1` before the rename, so it
        // does not depend on how a platform treats an open file or an
        // existing target.
        if let Some(writer) = self.writer.as_mut() {
            writer.flush()?;
        }
        self.writer = None;
        let previous = rotated(&self.path);
        let renamed = match std::fs::remove_file(&previous) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => std::fs::rename(&self.path, &previous),
        };
        if let Err(e) = renamed {
            self.writer = reopen(&self.path);
            return Err(e);
        }
        match open_private(&self.path) {
            Ok(writer) => self.writer = Some(writer),
            Err(e) => {
                self.writer = reopen(&previous);
                return Err(e);
            }
        }
        self.size = 0;
        self.rotations += 1;
        Ok(())
    }

    fn closed() -> std::io::Error {
        std::io::Error::other("the file could not be reopened after a rotation")
    }
}

fn reopen(path: &Path) -> Option<BufWriter<File>> {
    let file = std::fs::OpenOptions::new().append(true).open(path).ok()?;
    Some(BufWriter::with_capacity(FILE_BUFFER, file))
}

/// What the reading task and the session share.
pub struct Live {
    ring: Ring,
    sink: Option<Sink>,
    filter: Filter,
    pub seen: u64,
    pub kept: u64,
    /// Entries the library could not parse; the stream goes on after them.
    pub unparsed: u64,
    pub ended: Option<String>,
}

impl Live {
    pub fn new(ring: Ring, sink: Option<Sink>, filter: Filter) -> Self {
        Self { ring, sink, filter, seen: 0, kept: 0, unparsed: 0, ended: None }
    }

    pub fn take(&mut self, entry: Entry, executable: &str) -> std::io::Result<()> {
        if self.ended.is_some() {
            return Ok(());
        }
        self.seen += 1;
        if !self.filter.keeps(&entry, executable) {
            return Ok(());
        }
        let line = self.sink.is_some().then(|| entry.line(self.ring.next_seq));
        self.ring.push(entry);
        self.kept += 1;
        match (self.sink.as_mut(), line) {
            (Some(sink), Some(line)) => sink.write(&line),
            _ => Ok(()),
        }
    }

    /// A line that is not an entry (`start`, `end`), for the file only.
    fn note(&mut self, line: &Value) -> std::io::Result<()> {
        self.sink.as_mut().map_or(Ok(()), |s| s.write(&line.to_string()))
    }

    /// Write the `end` line once; later calls change nothing.
    pub fn end(&mut self, reason: &str) {
        if self.ended.is_some() {
            return;
        }
        let line = json!({
            "event": "end",
            "time": clock::now_utc(),
            "reason": reason,
            "seen": self.seen,
            "kept": self.kept,
            "unparsed": self.unparsed,
            "rotations": self.sink.as_ref().map_or(0, |s| s.rotations),
        });
        // The end line is what tells a reader the capture finished.
        if let Some(sink) = self.sink.as_mut() {
            let _ = sink.append(&line.to_string()).and_then(|()| sink.flush());
        }
        self.ended = Some(reason.to_string());
    }

    fn flush(&mut self) {
        let _ = self.sink.as_mut().map(Sink::flush);
    }

    fn state(&self) -> &'static str {
        if self.ended.is_none() { "running" } else { "ended" }
    }

    /// `status`: what was received, and what the buffer holds.
    fn counts(&self) -> Map<String, Value> {
        let mut out = fields! {
            "state": self.state(),
            "seen": self.seen,
            "kept": self.kept,
            "unparsed": self.unparsed,
            "buffer": self.ring.usage(),
        };
        if let Some(sink) = &self.sink {
            out.insert("rotations".into(), json!(sink.rotations));
        }
        if let Some(reason) = &self.ended {
            out.insert("ended_reason".into(), json!(reason));
        }
        out
    }

    /// `read`: the page, and whether the capture is still running, so a
    /// caller that polls sees it end.
    fn read(&self, after: u64, q: &Query) -> Map<String, Value> {
        let page = self.ring.read(after, q.grep.as_ref(), q.max);
        let mut out = fields! {
            "state": self.state(),
            "count": page.entries.len(),
            "entries": page.entries,
            "cursor": page.cursor,
            "more": page.more,
            "dropped": page.dropped,
        };
        if let Some(reason) = &self.ended {
            out.insert("ended_reason".into(), json!(reason));
        }
        out
    }
}

/// Puts buffered lines in the file while the stream is quiet. Not part of
/// the reading task: that one must never be interrupted between two reads.
async fn flusher(live: Weak<Mutex<Live>>) {
    let mut tick = tokio::time::interval(FLUSH_EVERY);
    loop {
        tick.tick().await;
        let Some(live) = live.upgrade() else { return };
        let mut live = live.lock().unwrap();
        if live.ended.is_some() {
            return;
        }
        live.flush();
    }
}

/// The library reads a whole entry before parsing it, so an entry it cannot
/// parse costs only that entry. A missing leading byte means the stream lost
/// its place, and nothing after it can be trusted.
fn skippable(e: &IdeviceError) -> bool {
    matches!(e, IdeviceError::UnexpectedResponse(m) if !m.contains("leading byte"))
}

async fn pump(mut stream: OsTraceRelayReceiver, live: Arc<Mutex<Live>>) {
    loop {
        // Never under a timeout: an entry is several reads, and giving up
        // between them would leave the stream mid-entry.
        let reason = match stream.next().await {
            Ok(log) => {
                let (entry, executable) = entry_and_executable(log);
                match live.lock().unwrap().take(entry, &executable) {
                    Ok(()) => continue,
                    Err(e) => format!("cannot write the file: {e}"),
                }
            }
            Err(e) if skippable(&e) => {
                live.lock().unwrap().unparsed += 1;
                continue;
            }
            Err(e) => format!("the log stream ended: {e}"),
        };
        live.lock().unwrap().end(&reason);
        return;
    }
}

pub struct Capture {
    spec: Spec,
    /// `spec`'s apps resolved to where they live.
    filter: Filter,
    started_at: String,
    started: std::time::Instant,
    live: Arc<Mutex<Live>>,
    task: tokio::task::JoinHandle<()>,
    /// Only with a file.
    flusher: Option<tokio::task::JoinHandle<()>>,
    reset: Reset,
}

impl Capture {
    /// End it now. The task is stopped first, so the `end` line is the last.
    /// The connection is reset here, not when the stopped task drops the
    /// stream: by then `Session::reset` may have closed the tunnel.
    pub fn end(&self, reason: &str) {
        self.reset.now();
        self.task.abort();
        if let Some(flusher) = &self.flusher {
            flusher.abort();
        }
        self.live.lock().unwrap().end(reason);
    }

    /// After `end`: wait until the connection has been reset, so the relay
    /// has let go of it before the next capture connects or the daemon exits.
    pub async fn settle(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(1), self.reset.sent()).await;
    }

    /// A task that stopped without saying why (a panic in the parser).
    fn check(&self) {
        if self.task.is_finished() {
            self.live.lock().unwrap().end("the log reader stopped unexpectedly");
        }
    }

    pub fn is_running(&self) -> bool {
        self.check();
        self.live.lock().unwrap().ended.is_none()
    }

    /// While nothing has arrived for a while: why that can be. Not with
    /// `--pid`, where a quiet process sends nothing either.
    fn hint_if_silent(&self, live: &Live, out: &mut Map<String, Value>) {
        if live.ended.is_none()
            && self.spec.pid.is_none()
            && live.seen == 0
            && self.started.elapsed() > Duration::from_secs(5)
        {
            // Seen live: a relay connection that was dropped, not reset,
            // held the next capture for about 30 s.
            out.insert("hint".into(), json!(crate::i18n::t(
                "아직 받은 로그가 없습니다. 연결이 끊긴 직후에는 폰이 약 30초 뒤에야 로그를 보냅니다.",
                "No entries yet. Right after a dropped connection the phone starts sending about 30 s later.",
            )));
        }
    }

    pub fn read(&self, after: u64, q: &Query) -> Map<String, Value> {
        self.check();
        let live = self.live.lock().unwrap();
        let mut out = live.read(after, q);
        self.hint_if_silent(&live, &mut out);
        out
    }

    pub fn report(&self) -> Map<String, Value> {
        self.check();
        let live = self.live.lock().unwrap();
        let mut out = live.counts();
        out.insert("started_at".into(), json!(self.started_at));
        if !self.filter.processes.is_empty() {
            out.insert("process".into(), json!(self.filter.processes));
        }
        if !self.filter.apps.is_empty() {
            out.insert("app".into(), json!(self.filter.apps.iter().map(|a| &a.bundle_id).collect::<Vec<_>>()));
        }
        if let Some(pid) = self.spec.pid {
            out.insert("pid".into(), json!(pid));
        }
        if let Some(level) = self.filter.min_level {
            out.insert("level".into(), json!(level.name()));
        }
        if let Some(file) = &self.spec.file {
            out.insert("path".into(), json!(file.path.display().to_string()));
            if live.sink.as_ref().is_some_and(|s| s.rotations > 0) {
                out.insert("previous_part".into(), json!(rotated(&file.path).display().to_string()));
            }
        }
        self.hint_if_silent(&live, &mut out);
        out
    }
}

impl Session {
    pub async fn cmd_syslog(&mut self, args: &Value) -> Res {
        let udid = self.udid.clone();
        match parse(args)? {
            Request::Status => Ok(match &self.capture {
                Some(c) => c.report(),
                None => fields! { "state": "none" },
            }),
            Request::Read { after, query } => match &self.capture {
                Some(c) => Ok(c.read(after, &query)),
                None => Err(Fail::not_sent("no_capture", "no capture to read").hint(crate::i18n::t(
                    "`iph syslog start`로 먼저 시작하세요.",
                    "Start one with `iph syslog start`.",
                ))),
            },
            Request::Start(spec) => {
                if let Some(c) = &self.capture {
                    // An ended capture is kept for `read` until `stop`.
                    let (code, message) = if c.is_running() {
                        ("capture_running", "a capture is already running")
                    } else {
                        ("capture_ended", "an ended capture has not been stopped")
                    };
                    return Err(Fail::not_sent(code, message).with("started_at", c.started_at.clone()).hint(
                        crate::i18n::t(
                            "`iph syslog stop`으로 끝낸 뒤 다시 시작하세요.",
                            "End it with `iph syslog stop` first.",
                        ),
                    ));
                }
                // Before anything is created or opened: an unknown app is an error.
                let apps = self.app_bundles(&spec.apps).await?;
                let filter = Filter { processes: spec.processes.clone(), apps, min_level: spec.min_level };
                // The file is opened only once the relay runs: a start that
                // fails leaves an earlier capture's file as it was.
                let (stream, reset) = self.os_trace(spec.pid).await?;
                let sink = spec
                    .file
                    .as_ref()
                    .map(|f| {
                        Sink::create(&f.path, f.max_bytes).map_err(|e| Fail::invalid(format!("{}: {e}", f.path.display())))
                    })
                    .transpose()?;
                let mut live = Live::new(Ring::new(spec.limits), sink, filter.clone());
                let started_at = clock::now_utc();
                let start = json!({
                    "event": "start",
                    "time": started_at,
                    "udid": udid,
                    "process": filter.processes,
                    "app": filter.apps.iter().map(|a| &a.bundle_id).collect::<Vec<_>>(),
                    "pid": spec.pid,
                    "level": filter.min_level.map(Level::name),
                    "max_mb": spec.file.as_ref().map(|f| f.max_bytes / (1024 * 1024)),
                });
                if let Err(e) = live.note(&start) {
                    let path = spec.file.as_ref().map(|f| f.path.display().to_string()).unwrap_or_default();
                    return Err(Fail::invalid(format!("{path}: {e}")));
                }
                let live = Arc::new(Mutex::new(live));
                let task = tokio::task::spawn_local(pump(stream, live.clone()));
                let flusher = spec.file.is_some().then(|| tokio::task::spawn_local(flusher(Arc::downgrade(&live))));
                let started = std::time::Instant::now();
                let capture = Capture { spec, filter, started_at, started, live, task, flusher, reset };
                let report = capture.report();
                self.capture = Some(capture);
                Ok(report)
            }
            Request::Stop(query) => {
                let Some(mut capture) = self.capture.take() else {
                    return Err(Fail::not_sent("no_capture", "no capture to stop"));
                };
                capture.end("stopped");
                capture.settle().await;
                let mut out = capture.report();
                if let Some(q) = query {
                    let (entries, matched) = capture.live.lock().unwrap().ring.latest(q.grep.as_ref(), q.max);
                    out.insert("matched".into(), json!(matched));
                    out.insert("entries".into(), json!(entries));
                }
                Ok(out)
            }
        }
    }

    /// Each app, named as `iph launch` takes it, with where it is installed.
    async fn app_bundles(&mut self, apps: &[String]) -> Res<Vec<AppBundle>> {
        let mut found: Vec<AppBundle> = Vec::new();
        for app in apps {
            let bundle_id = self.resolve_bundle(app).await?;
            if found.iter().any(|b| b.bundle_id == bundle_id) {
                continue;
            }
            // The list is cached; an app installed since is not in it.
            let mut bundle = find_bundle(&self.list_apps(false).await?, &bundle_id);
            if bundle.is_none() {
                bundle = find_bundle(&self.list_apps(true).await?, &bundle_id);
            }
            found.push(bundle.ok_or_else(|| {
                Fail::not_sent("app_not_found", format!("no installed app has the bundle id {bundle_id}, or the phone gave no path for it"))
                    .hint(crate::i18n::t(
                        "`iph apps`로 이름과 bundle id를 확인하세요.",
                        "See names and bundle ids with `iph apps`.",
                    ))
            })?);
        }
        Ok(found)
    }

    /// End the capture, keeping it for `status`, `read` and `stop`.
    pub fn end_capture(&mut self, reason: &str) {
        if let Some(c) = &self.capture {
            c.end(reason);
        }
    }

    /// Before the daemon exits: end the capture and let its reset go out.
    pub async fn end_capture_before_exit(&mut self, reason: &str) {
        if let Some(c) = &mut self.capture {
            c.end(reason);
            c.settle().await;
        }
    }

    pub fn capture_state(&self) -> Option<&'static str> {
        self.capture.as_ref().map(|c| if c.is_running() { "running" } else { "ended" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(filename: &str, level: &str, label: Option<(&str, &str)>) -> OsTraceLog {
        serde_json::from_value(json!({
            "pid": 412,
            // What the library produces for 01:02:03.004005: micros in the nanos field.
            "timestamp": "2026-11-19T01:02:03.000004005",
            "level": level,
            "image_name": "/System/Library/Frameworks/UIKitCore.framework/UIKitCore",
            "filename": filename,
            "message": "tapped row",
            "label": label.map(|(s, c)| json!({ "subsystem": s, "category": c })),
            "procid": 412,
            "thread_id": 7,
            "image_offset": 0,
            "image_uuid": "00000000-0000-0000-0000-000000000000",
            "process_image_uuid": "00000000-0000-0000-0000-000000000000",
            "mach_timestamp": 0,
        }))
        .unwrap()
    }

    fn entry(process: &str, level: Level, message: &str) -> Entry {
        Entry {
            time: "2026-11-19T01:02:03.000000Z".into(),
            pid: 1,
            process: process.into(),
            sender: process.into(),
            level,
            subsystem: None,
            category: None,
            message: message.into(),
        }
    }

    fn note(message: &str) -> Entry {
        entry("Papago", Level::Info, message)
    }

    /// Where an entry's process lives, as far as tests without apps care.
    fn feed(live: &mut Live, e: Entry) {
        let executable = format!("/usr/libexec/{}", e.process);
        live.take(e, &executable).unwrap();
    }

    fn roomy() -> Limits {
        Limits { entries: 1000, bytes: 1 << 20 }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iph-syslog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line is JSON"))
            .collect()
    }

    fn seqs(page: &Page) -> Vec<u64> {
        page.entries.iter().map(|e| e["seq"].as_u64().unwrap()).collect()
    }

    fn messages(page: &Page) -> Vec<&str> {
        page.entries.iter().map(|e| e["message"].as_str().unwrap()).collect()
    }

    /// A ring holding `n` entries "m1".."mN" (seq 1..=N).
    fn filled(limits: Limits, n: u64) -> Ring {
        let mut ring = Ring::new(limits);
        for i in 1..=n {
            assert_eq!(ring.push(note(&format!("m{i}"))), i);
        }
        ring
    }

    #[test]
    fn an_entry_becomes_one_json_line_named_by_its_executable() {
        let e = Entry::from(log("/Applications/Preferences.app/Preferences", "Error", Some(("com.apple.settings", "ui"))));
        let v: Value = serde_json::from_str(&e.line(7)).unwrap();
        assert_eq!(
            v,
            json!({
                "seq": 7, "time": "2026-11-19T01:02:03.004005Z", "pid": 412, "process": "Preferences", "sender": "UIKitCore",
                "level": "error", "subsystem": "com.apple.settings", "category": "ui", "message": "tapped row",
            })
        );
        let plain = Entry::from(log("/usr/libexec/backboardd", "Notice", None));
        let v: Value = serde_json::from_str(&plain.line(1)).unwrap();
        assert_eq!(v["process"], "backboardd");
        assert_eq!(v["level"], "notice");
        assert!(v.get("subsystem").is_none());
    }

    #[test]
    fn a_long_message_is_cut_in_a_reply_and_whole_in_the_file() {
        let long = note(&"y".repeat(900));
        let cut = long.value(3, MATCH_CHARS);
        assert_eq!(cut["message"].as_str().unwrap().chars().count(), MATCH_CHARS);
        assert_eq!(cut["message_cut"], true);
        let whole: Value = serde_json::from_str(&long.line(3)).unwrap();
        assert_eq!(whole["message"].as_str().unwrap().len(), 900);
        assert!(whole.get("message_cut").is_none());
        let short = note("short").value(3, MATCH_CHARS);
        assert!(short.get("message_cut").is_none());
        // Cut by characters, not bytes.
        let korean = note(&"가".repeat(600)).value(1, MATCH_CHARS);
        assert_eq!(korean["message"].as_str().unwrap().chars().count(), MATCH_CHARS);
    }

    #[test]
    fn the_filter_keeps_named_processes_at_or_above_a_level() {
        let any = Filter::default();
        assert!(any.keeps(&entry("SpringBoard", Level::Debug, ""), "/usr/libexec/SpringBoard"));
        let f = Filter { processes: vec!["Preferences".into(), "Papago".into()], min_level: Some(Level::Notice), ..Filter::default() };
        let keeps = |e: Entry| f.keeps(&e, "/somewhere/else");
        assert!(keeps(entry("Preferences", Level::Notice, "")));
        assert!(keeps(entry("Papago", Level::Fault, "")));
        assert!(!keeps(entry("Preferences", Level::Info, "")));
        assert!(!keeps(entry("SpringBoard", Level::Error, "")));
        // Exact names only: a prefix is another process.
        assert!(!keeps(entry("PreferencesExtension", Level::Error, "")));
    }

    fn papago() -> AppBundle {
        AppBundle::new("com.naver.papago", "file:///private/var/containers/Bundle/Application/AAAA-1111/Papago.app/").unwrap()
    }

    #[test]
    fn an_app_is_matched_by_where_it_is_installed_whichever_way_the_prefix_is_spelled() {
        let app = papago();
        assert_eq!(app.path, "/var/containers/Bundle/Application/AAAA-1111/Papago.app");
        let f = Filter { apps: vec![app], ..Filter::default() };
        let from = |executable: &str| f.keeps(&note(""), executable);
        assert!(from("/private/var/containers/Bundle/Application/AAAA-1111/Papago.app/Papago"));
        assert!(from("/var/containers/Bundle/Application/AAAA-1111/Papago.app/Papago"), "without /private");
        assert!(from("/private/var/containers/Bundle/Application/AAAA-1111/Papago.app/PlugIns/Share.appex/Share"), "an extension of the app");
        assert!(!from("/private/var/containers/Bundle/Application/BBBB-2222/Papago.app/Papago"), "the same name from another install");
        assert!(!from("/private/var/containers/Bundle/Application/AAAA-1111/Papago.appex/Papago"), "a directory that only starts like the bundle");
        assert!(!from("/private/var/containers/Bundle/Application/AAAA-1111/Papago.app"), "the bundle directory itself is not an executable in it");
        assert!(!from("/usr/libexec/backboardd"));
        assert!(!from(""));
        // The same bundle given with a /var path, a URL, a trailing slash.
        for spelling in [
            "/var/containers/Bundle/Application/AAAA-1111/Papago.app",
            "/private/var/containers/Bundle/Application/AAAA-1111/Papago.app/",
            "file:///var/containers/Bundle/Application/AAAA-1111/Papago.app/",
        ] {
            assert_eq!(AppBundle::new("com.naver.papago", spelling).unwrap(), papago(), "{spelling}");
        }
    }

    #[test]
    fn an_app_filter_follows_the_app_across_a_restart_and_matches_non_ascii_paths() {
        let f = Filter { apps: vec![papago()], ..Filter::default() };
        let executable = "/private/var/containers/Bundle/Application/AAAA-1111/Papago.app/Papago";
        for pid in [412, 977] {
            let mut e = note("");
            e.pid = pid;
            assert!(f.keeps(&e, executable), "pid {pid}");
        }
        let notes = AppBundle::new("com.apple.mobilenotes", "file:///Applications/%EB%A9%94%EB%AA%A8.app/").unwrap();
        assert_eq!(notes.path, "/Applications/메모.app");
        let f = Filter { apps: vec![notes], ..Filter::default() };
        assert!(f.keeps(&note(""), "/Applications/메모.app/메모"));
    }

    #[test]
    fn a_process_and_an_app_each_select_and_the_level_narrows_both() {
        let f = Filter { processes: vec!["SpringBoard".into()], apps: vec![papago()], min_level: Some(Level::Notice) };
        let papago_exe = "/private/var/containers/Bundle/Application/AAAA-1111/Papago.app/Papago";
        assert!(f.keeps(&entry("SpringBoard", Level::Notice, ""), "/usr/libexec/SpringBoard"), "a listed process");
        assert!(f.keeps(&entry("Papago", Level::Error, ""), papago_exe), "a listed app");
        assert!(!f.keeps(&entry("Papago", Level::Info, ""), papago_exe), "the level applies to the app");
        assert!(!f.keeps(&entry("SpringBoard", Level::Debug, ""), "/usr/libexec/SpringBoard"), "and to the process");
        assert!(!f.keeps(&entry("backboardd", Level::Fault, ""), "/usr/libexec/backboardd"), "neither listed");
        // A process named like the app's executable is its own selector.
        let only_process = Filter { processes: vec!["Papago".into()], ..Filter::default() };
        assert!(only_process.keeps(&entry("Papago", Level::Debug, ""), "/elsewhere/Papago"));
        // An app listed alone does not let other processes through.
        let only_app = Filter { apps: vec![papago()], ..Filter::default() };
        assert!(!only_app.keeps(&entry("Papago", Level::Debug, ""), "/elsewhere/Papago"));
    }

    fn listed(bundle_id: &str, name: &str, path: &str) -> AppListEntry {
        serde_json::from_value(json!({
            "isRemovable": true, "name": name, "isFirstParty": false, "path": path, "bundleIdentifier": bundle_id,
            "isDeveloperApp": false, "isInternal": false, "isHidden": false, "isAppClip": false,
        }))
        .unwrap()
    }

    #[test]
    fn an_app_is_found_by_bundle_id_in_the_phones_list_and_needs_a_usable_path() {
        let apps = vec![
            listed("com.apple.Preferences", "설정", "file:///Applications/Preferences.app/"),
            listed("com.naver.papago", "Papago", "file:///private/var/containers/Bundle/Application/AAAA-1111/Papago.app/"),
            listed("com.example.nopath", "No path", ""),
        ];
        assert_eq!(find_bundle(&apps, "com.naver.papago"), Some(papago()));
        assert_eq!(find_bundle(&apps, "com.apple.Preferences").unwrap().path, "/Applications/Preferences.app");
        assert_eq!(find_bundle(&apps, "com.example.missing"), None);
        assert_eq!(find_bundle(&apps, "com.example.nopath"), None, "an empty path would match nothing useful");
    }

    #[test]
    fn only_an_entry_the_library_could_not_parse_is_skipped() {
        let unparsed = IdeviceError::UnexpectedResponse("unknown log level byte value".into());
        let lost_place = IdeviceError::UnexpectedResponse("expected leading byte 0x02 at start of log packet".into());
        let gone = IdeviceError::Socket(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert!(skippable(&unparsed));
        assert!(!skippable(&lost_place));
        assert!(!skippable(&gone));
    }

    #[test]
    fn device_levels_rank_by_severity_not_by_code() {
        let levels: Vec<Level> = [LogLevel::Fault, LogLevel::Debug, LogLevel::Error, LogLevel::Notice, LogLevel::Info]
            .into_iter()
            .map(Level::from)
            .collect();
        let mut sorted = levels.clone();
        sorted.sort();
        assert_eq!(sorted, [Level::Debug, Level::Info, Level::Notice, Level::Error, Level::Fault]);
    }

    #[test]
    fn kept_entries_get_seqs_from_one_and_the_oldest_are_evicted_by_count() {
        let mut ring = filled(Limits { entries: 3, bytes: 1 << 20 }, 5);
        let usage = ring.usage();
        assert_eq!(
            (usage["entries"].as_u64(), usage["oldest_seq"].as_u64(), usage["newest_seq"].as_u64(), usage["evicted"].as_u64()),
            (Some(3), Some(3), Some(5), Some(2))
        );
        assert_eq!(usage["max_entries"], 3);
        assert_eq!(ring.push(note("m6")), 6, "a seq is never reused");
        assert_eq!(messages(&ring.read(0, None, 10)), ["m4", "m5", "m6"]);
        let fresh = Ring::new(roomy()).usage();
        assert!(fresh.get("oldest_seq").is_none() && fresh.get("newest_seq").is_none());
        assert_eq!(fresh["evicted"], 0);
    }

    #[test]
    fn the_byte_budget_evicts_by_weight_and_the_count_stays_exact() {
        let w = note("m1").weight();
        let mut ring = Ring::new(Limits { entries: 100, bytes: 3 * w });
        for i in 1..=5 {
            ring.push(note(&format!("m{i}")));
        }
        let usage = ring.usage();
        assert_eq!(usage["entries"], 3);
        assert_eq!(usage["bytes"], 3 * w);
        assert_eq!(usage["oldest_seq"], 3);
        // A heavy entry pushes out as many light ones as it takes.
        ring.push(note(&"z".repeat(w)));
        let usage = ring.usage();
        assert!(usage["bytes"].as_u64().unwrap() <= 3 * w as u64);
        assert_eq!((usage["newest_seq"].as_u64(), usage["oldest_seq"].as_u64(), usage["entries"].as_u64()), (Some(6), Some(5), Some(2)));
    }

    #[test]
    fn an_entry_larger_than_the_whole_budget_evicts_itself_and_the_ring_goes_on() {
        let mut ring = Ring::new(Limits { entries: 10, bytes: note("m1").weight() * 2 });
        ring.push(note("m1"));
        assert_eq!(ring.push(note(&"z".repeat(10_000))), 2);
        let usage = ring.usage();
        assert_eq!((usage["entries"].as_u64(), usage["bytes"].as_u64(), usage["evicted"].as_u64()), (Some(0), Some(0), Some(2)));
        let page = ring.read(0, None, 10);
        assert_eq!((page.entries.len(), page.cursor, page.dropped), (0, 2, 2));
        ring.push(note("m3"));
        assert_eq!(seqs(&ring.read(2, None, 10)), [3]);
    }

    #[test]
    fn a_read_without_after_starts_at_the_oldest_kept_entry() {
        let ring = filled(Limits { entries: 4, bytes: 1 << 20 }, 6);
        let page = ring.read(0, None, 10);
        assert_eq!(seqs(&page), [3, 4, 5, 6]);
        assert_eq!((page.cursor, page.more, page.dropped), (6, false, 2), "the two evicted before this first read are reported");
        let untouched = filled(roomy(), 3).read(0, None, 10);
        assert_eq!((seqs(&untouched), untouched.cursor, untouched.dropped), (vec![1, 2, 3], 3, 0));
    }

    #[test]
    fn a_read_after_a_seq_the_ring_still_holds_returns_only_what_is_newer() {
        let ring = filled(Limits { entries: 4, bytes: 1 << 20 }, 6);
        let page = ring.read(4, None, 10);
        assert_eq!((seqs(&page), page.cursor, page.dropped, page.more), (vec![5, 6], 6, 0, false));
        let page = ring.read(2, None, 10);
        assert_eq!((seqs(&page), page.dropped), (vec![3, 4, 5, 6], 0), "seq 2 was the last one the caller had; 3 is still here");
    }

    #[test]
    fn a_read_after_a_seq_older_than_the_ring_counts_what_was_lost() {
        let ring = filled(Limits { entries: 4, bytes: 1 << 20 }, 10);
        // Held: 7..=10. The caller has 3, so 4, 5 and 6 are gone.
        let page = ring.read(3, None, 10);
        assert_eq!(seqs(&page), [7, 8, 9, 10]);
        assert_eq!((page.dropped, page.cursor), (3, 10));
    }

    #[test]
    fn a_read_after_the_newest_seq_is_empty_and_a_cursor_beyond_it_is_pulled_back() {
        let ring = filled(roomy(), 3);
        let caught_up = ring.read(3, None, 10);
        assert_eq!((caught_up.entries.len(), caught_up.cursor, caught_up.more, caught_up.dropped), (0, 3, false, 0));
        // A cursor from an earlier capture, whose seqs ran higher.
        let stale = ring.read(5000, None, 10);
        assert_eq!((stale.entries.len(), stale.cursor, stale.dropped), (0, 3, 0));
        let empty = Ring::new(roomy()).read(0, None, 10);
        assert_eq!((empty.entries.len(), empty.cursor, empty.more, empty.dropped), (0, 0, false, 0));
        assert_eq!(Ring::new(roomy()).read(u64::MAX, None, 10).cursor, 0);
    }

    #[test]
    fn a_grep_with_no_match_returns_nothing_and_still_moves_the_cursor() {
        let ring = filled(roomy(), 5);
        let re = Regex::new("nothing like this").unwrap();
        let page = ring.read(1, Some(&re), 10);
        assert_eq!((page.entries.len(), page.cursor, page.more, page.dropped), (0, 5, false, 0));
        // Reading on from that cursor looks at nothing already looked at.
        let mut ring = ring;
        ring.push(note("nothing like this, at last"));
        assert_eq!(seqs(&ring.read(page.cursor, Some(&re), 10)), [6]);
    }

    #[test]
    fn max_cuts_a_page_and_the_cursor_continues_exactly_after_the_last_entry_returned() {
        let ring = filled(roomy(), 7);
        let first = ring.read(0, None, 3);
        assert_eq!((seqs(&first), first.cursor, first.more), (vec![1, 2, 3], 3, true));
        let second = ring.read(first.cursor, None, 3);
        assert_eq!((seqs(&second), second.cursor, second.more), (vec![4, 5, 6], 6, true));
        let last = ring.read(second.cursor, None, 3);
        assert_eq!((seqs(&last), last.cursor, last.more), (vec![7], 7, false));
        // A page that is exactly full and has nothing behind it is not "more".
        let exact = ring.read(4, None, 3);
        assert_eq!((seqs(&exact), exact.cursor, exact.more), (vec![5, 6, 7], 7, false));
    }

    #[test]
    fn a_grep_cut_by_max_leaves_the_cursor_on_the_last_match_so_nothing_between_is_skipped() {
        let mut ring = Ring::new(roomy());
        for i in 1..=10 {
            ring.push(note(&if i % 3 == 0 { format!("hit {i}") } else { format!("miss {i}") }));
        }
        let re = Regex::new("^hit").unwrap();
        let first = ring.read(0, Some(&re), 2);
        assert_eq!((seqs(&first), first.cursor, first.more), (vec![3, 6], 6, true));
        let second = ring.read(first.cursor, Some(&re), 2);
        assert_eq!((seqs(&second), second.cursor, second.more), (vec![9], 10, false));
    }

    #[test]
    fn paging_to_the_end_equals_one_read_of_the_same_filter() {
        let mut ring = Ring::new(roomy());
        for i in 1..=60 {
            ring.push(note(&if i % 4 == 0 { format!("hit {i}") } else { format!("miss {i}") }));
        }
        let re = Regex::new("^hit").unwrap();
        let whole = seqs(&ring.read(0, Some(&re), 1000));
        for max in [1, 2, 3, 7, 15] {
            let (mut cursor, mut got) = (0, Vec::new());
            loop {
                let page = ring.read(cursor, Some(&re), max);
                assert_eq!(page.dropped, 0);
                assert!(page.cursor >= cursor, "the cursor never goes back");
                got.extend(seqs(&page));
                cursor = page.cursor;
                if !page.more {
                    break;
                }
            }
            assert_eq!(got, whole, "max {max}");
            assert_eq!(cursor, 60);
        }
    }

    #[test]
    fn eviction_between_pages_is_reported_and_the_cursor_still_leaves_no_gap_in_what_remains() {
        let mut ring = Ring::new(Limits { entries: 5, bytes: 1 << 20 });
        let push = |ring: &mut Ring, from: u64, to: u64| {
            for i in from..=to {
                ring.push(note(&if i % 2 == 0 { format!("hit {i}") } else { format!("miss {i}") }));
            }
        };
        let re = Regex::new("^hit").unwrap();
        push(&mut ring, 1, 8);
        // Held: 4..=8. Seqs 1, 2 and 3 went before anything was read.
        let a = ring.read(0, Some(&re), 2);
        assert_eq!((seqs(&a), a.cursor, a.more, a.dropped), (vec![4, 6], 6, true, 3));
        push(&mut ring, 9, 12);
        // Held: 8..=12. Seq 7 went while the caller was away.
        let b = ring.read(a.cursor, Some(&re), 2);
        assert_eq!((seqs(&b), b.cursor, b.more, b.dropped), (vec![8, 10], 10, true, 1));
        let c = ring.read(b.cursor, Some(&re), 2);
        assert_eq!((seqs(&c), c.cursor, c.more, c.dropped), (vec![12], 12, false, 0));
    }

    #[test]
    fn paging_while_entries_are_evicted_returns_or_reports_every_seq_exactly_once() {
        for (capacity, page_size, burst) in [(6, 3, 4), (5, 1, 2), (10, 4, 9), (3, 2, 5), (50, 7, 1)] {
            let mut ring = Ring::new(Limits { entries: capacity, bytes: 1 << 20 });
            let (mut pushed, mut cursor, mut returned, mut dropped) = (0u64, 0u64, Vec::new(), 0u64);
            for round in 0..40 {
                for _ in 0..burst {
                    pushed += 1;
                    ring.push(note(&format!("m{pushed}")));
                }
                // Read at an uneven pace: some rounds skip, so evictions pile up.
                if round % 3 == 2 {
                    continue;
                }
                loop {
                    let page = ring.read(cursor, None, page_size);
                    dropped += page.dropped;
                    returned.extend(seqs(&page));
                    assert!(page.cursor >= cursor, "the cursor never goes back");
                    cursor = page.cursor;
                    if !page.more {
                        break;
                    }
                }
                assert_eq!(cursor, pushed, "a page that is not cut short has looked at everything");
            }
            assert!(returned.windows(2).all(|w| w[0] < w[1]), "oldest first, never twice ({capacity}, {page_size}, {burst})");
            assert_eq!(returned.len() as u64 + dropped, pushed, "every seq was returned or counted as dropped ({capacity}, {page_size}, {burst})");
        }
    }

    #[test]
    fn latest_returns_the_most_recent_matches_oldest_first_with_the_total() {
        let mut ring = Ring::new(roomy());
        for i in 0..3 {
            ring.push(note(&format!("hit {i}")));
        }
        ring.push(note("miss"));
        ring.push(note(&format!("hit 3 {}", "y".repeat(900))));
        let re = Regex::new("^hit").unwrap();
        let (found, matched) = ring.latest(Some(&re), 2);
        assert_eq!(matched, 4);
        assert_eq!(found.iter().map(|e| e["seq"].as_u64().unwrap()).collect::<Vec<_>>(), [3, 5]);
        assert_eq!(found[0]["message"], "hit 2");
        assert_eq!(found[1]["message"].as_str().unwrap().chars().count(), MATCH_CHARS);
        assert_eq!(found[1]["message_cut"], true);
        let (all, matched) = ring.latest(None, 0);
        assert_eq!((all.len(), matched), (0, 5), "max 0 only counts");
        let (everything, _) = ring.latest(None, 1000);
        assert_eq!(everything.len(), 5);
    }

    #[test]
    fn the_file_rolls_over_at_its_size_with_whole_lines_in_both_parts() {
        let dir = temp_dir("rotate");
        let path = dir.join("cap.jsonl");
        std::fs::write(rotated(&path), "left from before\n").unwrap();
        let line = |i: u32| json!({ "i": format!("{i:02}"), "message": "x".repeat(80) }).to_string();
        let part = 10 * (line(0).len() as u64 + 1);
        let mut sink = Sink::create(&path, part).unwrap();
        assert!(!rotated(&path).exists(), "an earlier part is removed at start");
        // Ten lines fit in a part, so 25 lines rotate twice and the second
        // rotation renames over the `.1` the first one made.
        for i in 0..25 {
            sink.write(&line(i)).unwrap();
        }
        sink.flush().unwrap();
        assert_eq!(sink.rotations, 2);
        let indexes = |p: &Path| lines(p).iter().map(|v| v["i"].as_str().unwrap().parse().unwrap()).collect::<Vec<u32>>();
        assert_eq!(indexes(&rotated(&path)), (10..20).collect::<Vec<_>>(), "only the last full part is kept");
        assert_eq!(indexes(&path), (20..25).collect::<Vec<_>>());
        for p in [&path, &rotated(&path)] {
            assert!(std::fs::metadata(p).unwrap().len() <= part);
            assert!(lines(p).iter().all(|v| v["message"].as_str().unwrap().len() == 80));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(rotated(&path)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rotation_that_fails_still_ends_the_file_with_the_end_line() {
        let dir = temp_dir("rotate-fail");
        let path = dir.join("cap.jsonl");
        let mut sink = Sink::create(&path, 200).unwrap();
        // A directory where the previous part goes: the rotation cannot clear it.
        std::fs::create_dir_all(rotated(&path).join("x")).unwrap();
        sink.write(&json!({ "message": "a".repeat(100) }).to_string()).unwrap();
        assert!(sink.write(&json!({ "message": "b".repeat(100) }).to_string()).is_err());
        let mut live = Live::new(Ring::new(roomy()), Some(sink), Filter::default());
        live.end("cannot write the file");
        let got = lines(&path);
        assert_eq!(got.len(), 2, "the entry before the failed rotation, then the end line");
        assert_eq!(got[1]["event"], "end");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rotation_flushes_what_is_buffered_before_it_moves_the_file() {
        let dir = temp_dir("rotate-flush");
        let path = dir.join("cap.jsonl");
        let mut sink = Sink::create(&path, 200).unwrap();
        sink.write(&json!({ "message": "a".repeat(100) }).to_string()).unwrap();
        // Nothing was flushed yet; the rotation must not lose this line.
        sink.write(&json!({ "message": "b".repeat(100) }).to_string()).unwrap();
        assert_eq!(sink.rotations, 1);
        sink.flush().unwrap();
        assert_eq!(lines(&rotated(&path))[0]["message"], "a".repeat(100));
        assert_eq!(lines(&path)[0]["message"], "b".repeat(100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_flusher_puts_a_quiet_capture_in_the_file() {
        let dir = temp_dir("flusher");
        let path = dir.join("cap.jsonl");
        let sink = Sink::create(&path, 1 << 20).unwrap();
        let live = Arc::new(Mutex::new(Live::new(Ring::new(roomy()), Some(sink), Filter::default())));
        feed(&mut live.lock().unwrap(), note("quiet"));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().start_paused(true).build().unwrap();
        runtime.block_on(async {
            let task = tokio::spawn(flusher(Arc::downgrade(&live)));
            tokio::time::sleep(FLUSH_EVERY * 4).await;
            assert_eq!(lines(&path)[0]["message"], "quiet", "written without an explicit flush");
            live.lock().unwrap().end("stopped");
            tokio::time::timeout(FLUSH_EVERY * 4, task).await.expect("the flusher stops once the capture ended").unwrap();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_capture_ends_with_one_end_line_and_keeps_nothing_after_it() {
        let dir = temp_dir("end");
        let path = dir.join("cap.jsonl");
        let filter = Filter { processes: vec!["Papago".into()], ..Filter::default() };
        let mut live = Live::new(Ring::new(roomy()), Some(Sink::create(&path, 1 << 20).unwrap()), filter);
        feed(&mut live, note("one"));
        feed(&mut live, entry("SpringBoard", Level::Info, "other"));
        live.end("the log stream ended: socket closed");
        live.end("stopped");
        feed(&mut live, note("late"));
        let got = lines(&path);
        assert_eq!(got.len(), 2, "the end line is flushed without anyone asking");
        assert_eq!((got[0]["seq"].as_u64(), got[0]["message"].as_str()), (Some(1), Some("one")));
        assert_eq!(got[1]["event"], "end");
        assert_eq!(got[1]["reason"], "the log stream ended: socket closed");
        assert_eq!((got[1]["seen"].as_u64(), got[1]["kept"].as_u64()), (Some(2), Some(1)));
        assert_eq!(live.ended.as_deref(), Some("the log stream ended: socket closed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_file_nothing_is_written_and_the_ring_alone_serves_reads() {
        let dir = temp_dir("memory");
        let mut live = Live::new(Ring::new(roomy()), None, Filter::default());
        feed(&mut live, note("one"));
        live.note(&json!({ "event": "start" })).unwrap();
        live.end("stopped");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let out = live.read(0, &Query { grep: None, max: 10 });
        assert_eq!(out["count"], 1);
        assert!(live.counts().get("rotations").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_and_the_ring_number_an_entry_alike() {
        let dir = temp_dir("seq");
        let path = dir.join("cap.jsonl");
        let mut live = Live::new(Ring::new(roomy()), Some(Sink::create(&path, 1 << 20).unwrap()), Filter::default());
        for m in ["a", "b", "c"] {
            feed(&mut live, note(m));
        }
        live.end("stopped");
        let in_file: Vec<(u64, String)> = lines(&path)
            .iter()
            .filter(|v| v.get("event").is_none())
            .map(|v| (v["seq"].as_u64().unwrap(), v["message"].as_str().unwrap().to_string()))
            .collect();
        let page = live.ring.read(0, None, 10);
        let in_ring: Vec<(u64, String)> =
            page.entries.iter().map(|e| (e["seq"].as_u64().unwrap(), e["message"].as_str().unwrap().to_string())).collect();
        assert_eq!(in_file, in_ring);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_kept_entries_are_numbered_and_a_read_keeps_working_after_the_capture_ended() {
        let filter = Filter { processes: vec!["Papago".into()], ..Filter::default() };
        let mut live = Live::new(Ring::new(roomy()), None, filter);
        feed(&mut live, note("one"));
        feed(&mut live, entry("SpringBoard", Level::Info, "skipped"));
        feed(&mut live, note("two"));
        let q = Query { grep: None, max: 10 };
        let running = live.read(0, &q);
        assert_eq!((running["state"].as_str(), running["cursor"].as_u64(), running["dropped"].as_u64()), (Some("running"), Some(2), Some(0)));
        assert!(running.get("ended_reason").is_none());
        live.end("the connection to the phone was reset");
        let ended = live.read(1, &q);
        assert_eq!(ended["state"], "ended");
        assert_eq!(ended["ended_reason"], "the connection to the phone was reset");
        assert_eq!(ended["entries"][0]["message"], "two");
        assert_eq!(ended["entries"][0]["seq"], 2);
        let counts = live.counts();
        assert_eq!((counts["seen"].as_u64(), counts["kept"].as_u64()), (Some(3), Some(2)));
        assert_eq!(counts["buffer"]["entries"], 2);
    }

    fn parsed(args: Value) -> Res<Request> {
        parse(&args)
    }

    fn absolute(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn a_start_keeps_to_memory_unless_a_file_is_asked_for() {
        let Ok(Request::Start(spec)) = parsed(json!({ "action": "start", "process": "Preferences", "level": "default", "pid": null })) else {
            panic!("start")
        };
        assert_eq!(spec.processes, ["Preferences"]);
        assert_eq!(spec.min_level, Some(Level::Notice));
        assert_eq!((spec.apps.len(), spec.file), (0, None));
        assert_eq!(spec.limits, Limits { entries: 100_000, bytes: 64 * 1024 * 1024 });
        let out = absolute("x.jsonl");
        let Ok(Request::Start(spec)) = parsed(json!({ "action": "start", "process": ["A", "B"], "pid": 7, "output": out, "max_mb": 2 })) else {
            panic!("start with a file")
        };
        assert_eq!((spec.processes.len(), spec.pid), (2, Some(7)));
        assert_eq!(spec.file, Some(FileSpec { path: out.clone(), max_bytes: 2 * 1024 * 1024 }));
        let Ok(Request::Start(spec)) = parsed(json!({ "action": "start", "output": out })) else { panic!("default size") };
        assert_eq!(spec.file.unwrap().max_bytes, DEFAULT_MAX_MB * 1024 * 1024);
        let Ok(Request::Start(spec)) = parsed(json!({ "action": "start", "output": absolute("y.jsonl"), "max_mb": MAX_FILE_MB })) else {
            panic!("the largest max_mb")
        };
        assert_eq!(spec.file.unwrap().max_bytes, MAX_FILE_MB * 1024 * 1024);
        let Ok(Request::Start(spec)) = parsed(json!({ "action": "start", "buffer_entries": 500, "buffer_mb": 2 })) else {
            panic!("buffer limits")
        };
        assert_eq!(spec.limits, Limits { entries: 500, bytes: 2 * 1024 * 1024 });
    }

    #[test]
    fn an_app_is_a_name_or_a_list_of_names_for_start_only() {
        let Ok(Request::Start(spec)) = parsed(json!({ "action": "start", "app": "Papago", "process": "Preferences" })) else {
            panic!("one app")
        };
        assert_eq!((spec.apps, spec.processes), (strings(&["Papago"]), strings(&["Preferences"])));
        let Ok(Request::Start(spec)) = parsed(json!({ "action": "start", "app": ["com.naver.papago", "설정"] })) else {
            panic!("two apps")
        };
        assert_eq!(spec.apps, strings(&["com.naver.papago", "설정"]));
        for bad in [
            json!({ "action": "start", "app": "" }),
            json!({ "action": "start", "app": ["a", " "] }),
            json!({ "action": "start", "app": 7 }),
            json!({ "action": "start", "app": [1] }),
            json!({ "action": "read", "app": "Papago" }),
            json!({ "action": "stop", "app": "Papago" }),
        ] {
            assert_eq!(parsed(bad.clone()).expect_err(&bad.to_string()).code, "invalid_argument", "{bad}");
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn read_and_stop_take_a_cursor_a_pattern_and_a_count() {
        let Ok(Request::Read { after, query }) = parsed(json!({ "action": "read" })) else { panic!("bare read") };
        assert_eq!((after, query.max, query.grep.is_none()), (0, DEFAULT_MATCHES as usize, true));
        let Ok(Request::Read { after, query }) = parsed(json!({ "action": "read", "after": 120, "grep": "a+", "max": 5000 })) else {
            panic!("read")
        };
        assert_eq!((after, query.max, query.grep.unwrap().as_str()), (120, MAX_MATCHES as usize, "a+"));
        assert!(matches!(parsed(json!({ "action": "stop" })), Ok(Request::Stop(None))));
        let Ok(Request::Stop(Some(q))) = parsed(json!({ "action": "stop", "grep": "a+", "max": 5000 })) else {
            panic!("stop with grep")
        };
        assert_eq!(q.max, MAX_MATCHES as usize);
        let Ok(Request::Stop(Some(q))) = parsed(json!({ "action": "stop", "max": 0 })) else { panic!("stop to count") };
        assert_eq!(q.max, 0);
        assert!(matches!(parsed(json!({ "action": "status" })), Ok(Request::Status)));
    }

    #[test]
    fn each_action_takes_only_its_own_arguments() {
        for bad in [
            json!({}),
            json!({ "action": "pause" }),
            json!({ "action": "stop", "process": "A" }),
            json!({ "action": "stop", "after": 3 }),
            json!({ "action": "read", "process": "A" }),
            json!({ "action": "read", "output": absolute("x.jsonl") }),
            json!({ "action": "start", "after": 3 }),
            json!({ "action": "start", "grep": "a" }),
            json!({ "action": "status", "grep": "a" }),
            json!({ "action": "read", "after": "three" }),
            json!({ "action": "read", "after": -1 }),
            json!({ "action": "read", "max": 0 }),
            json!({ "action": "read", "grep": "(" }),
            json!({ "action": "stop", "grep": "(" }),
            json!({ "action": "start", "output": "relative.jsonl" }),
            json!({ "action": "start", "level": "loud" }),
            json!({ "action": "start", "process": "/Applications/Preferences.app/Preferences" }),
        ] {
            let err = parsed(bad.clone()).expect_err(&bad.to_string());
            assert_eq!(err.code, "invalid_argument", "{bad}");
        }
    }

    #[test]
    fn sizes_are_bounded_and_a_file_size_needs_a_file() {
        let file = absolute("z.jsonl");
        for bad in [
            json!({ "action": "start", "output": file, "max_mb": 0 }),
            json!({ "action": "start", "output": file, "max_mb": MAX_FILE_MB + 1 }),
            json!({ "action": "start", "output": file, "max_mb": u64::MAX }),
            json!({ "action": "start", "max_mb": 8 }),
            json!({ "action": "start", "buffer_entries": 0 }),
            json!({ "action": "start", "buffer_entries": MAX_BUFFER_ENTRIES + 1 }),
            json!({ "action": "start", "buffer_mb": 0 }),
            json!({ "action": "start", "buffer_mb": MAX_BUFFER_MB + 1 }),
            json!({ "action": "start", "buffer_mb": u64::MAX }),
        ] {
            let err = parsed(bad.clone()).expect_err(&bad.to_string());
            assert_eq!(err.code, "invalid_argument", "{bad}");
        }
        assert!(parsed(json!({ "action": "start", "buffer_entries": MAX_BUFFER_ENTRIES, "buffer_mb": MAX_BUFFER_MB })).is_ok());
    }
}
