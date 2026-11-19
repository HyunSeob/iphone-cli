//! `iph syslog`: the phone's log stream, written to a file while a test runs,
//! so what an app logged after a tap can be checked afterwards.
//!
//! One capture per daemon. The stream is read by a task of its own that never
//! touches the session, so commands keep working while it runs. The file is
//! JSON Lines: a `start` line, the kept entries, and an `end` line that says
//! why the capture ended. A file without an `end` line belongs to a daemon
//! that was killed.

use std::{
    collections::VecDeque,
    fs::File,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use idevice::{
    IdeviceError,
    os_trace_relay::{LogLevel, OsTraceLog, OsTraceRelayReceiver},
};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::{
    clock,
    device::session::Session,
    fields, paths,
    reply::{Fail, Res},
    tcp::handle::Reset,
};

const DEFAULT_MAX_MB: u64 = 64;
const DEFAULT_MATCHES: u64 = 50;
const MAX_MATCHES: u64 = 1000;
/// Longest message a match returns; the file keeps the whole message.
const MATCH_CHARS: usize = 500;

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

/// One log entry as the file keeps it.
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
        let (subsystem, category) = match log.label {
            Some(l) => (Some(l.subsystem), Some(l.category)),
            None => (None, None),
        };
        Self {
            time: utc_micros(log.timestamp.and_utc().timestamp(), log.timestamp.and_utc().timestamp_subsec_nanos()),
            pid: log.pid,
            process: file_name(&log.filename).to_string(),
            sender: file_name(&log.image_name).to_string(),
            level: log.level.into(),
            subsystem,
            category,
            message: log.message,
        }
    }
}

impl Entry {
    pub fn line(&self) -> String {
        let mut v = json!({
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
        v["message"] = json!(self.message);
        v.to_string()
    }
}

/// What is kept on this side. A pid is filtered by the phone instead.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub processes: Vec<String>,
    pub min_level: Option<Level>,
}

impl Filter {
    pub fn keeps(&self, e: &Entry) -> bool {
        (self.processes.is_empty() || self.processes.contains(&e.process))
            && self.min_level.is_none_or(|min| e.level >= min)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub filter: Filter,
    pub pid: Option<u32>,
    pub path: PathBuf,
    pub max_bytes: u64,
}

#[derive(Debug)]
pub struct Query {
    pub grep: Option<Regex>,
    pub max: usize,
}

#[derive(Debug)]
pub enum Request {
    Start(Spec),
    /// `None`: report the counts only.
    Stop(Option<Query>),
    Status,
}

/// Parse the arguments every surface sends. `default_path` is used when no
/// `output` is given; a given one must be absolute, since the daemon writes it.
pub fn parse(args: &Value, default_path: impl FnOnce() -> PathBuf) -> Res<Request> {
    let given: Map<String, Value> = match args {
        Value::Null => Map::new(),
        Value::Object(m) => m.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => return Err(Fail::invalid("syslog takes an object of arguments")),
    };
    let action = given
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Fail::invalid("syslog needs an action: start, stop or status"))?;
    let allowed: &[&str] = match action {
        "start" => &["action", "process", "pid", "level", "output", "max_mb"],
        "stop" => &["action", "grep", "max"],
        "status" => &["action"],
        other => return Err(Fail::invalid(format!("unknown syslog action `{other}` (start, stop, status)"))),
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
    match action {
        "start" => {
            let processes = match given.get("process") {
                None => Vec::new(),
                Some(Value::String(s)) => vec![s.clone()],
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|v| v.as_str().map(String::from))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| Fail::invalid("`process` takes names"))?,
                Some(_) => return Err(Fail::invalid("`process` takes a name or a list of names")),
            };
            if processes.iter().any(|p| p.is_empty() || p.contains('/')) {
                return Err(Fail::invalid("`process` takes executable names such as Preferences, not paths"));
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
            let path = match given.get("output") {
                None => default_path(),
                Some(v) => {
                    let path = PathBuf::from(v.as_str().ok_or_else(|| Fail::invalid("`output` takes a path"))?);
                    if !path.is_absolute() {
                        return Err(Fail::invalid("`output` must be an absolute path"));
                    }
                    path
                }
            };
            let max_mb = number("max_mb")?.unwrap_or(DEFAULT_MAX_MB);
            if max_mb == 0 {
                return Err(Fail::invalid("`max_mb` must be at least 1"));
            }
            Ok(Request::Start(Spec {
                filter: Filter { processes, min_level },
                pid,
                path,
                max_bytes: max_mb * 1024 * 1024,
            }))
        }
        "stop" => {
            let grep = match given.get("grep") {
                None => None,
                Some(v) => {
                    let pattern = v.as_str().ok_or_else(|| Fail::invalid("`grep` takes a regular expression"))?;
                    Some(Regex::new(pattern).map_err(|e| Fail::invalid(format!("`grep`: {e}")))?)
                }
            };
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

/// Where the previous part goes when the file reaches its size.
pub fn rotated(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

fn open_private(path: &Path) -> std::io::Result<File> {
    let file = paths::private_file().write(true).create(true).truncate(true).open(path)?;
    #[cfg(unix)]
    {
        // `private_file` sets the mode only on a file it creates.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// The file, kept under a size: once the next line would pass it, the file
/// becomes `<file>.1` (replacing an older one) and a new file starts.
pub struct Sink {
    path: PathBuf,
    file: File,
    size: u64,
    cap: u64,
    pub rotations: u32,
}

impl Sink {
    pub fn create(path: &Path, cap: u64) -> std::io::Result<Self> {
        let file = open_private(path)?;
        // A part left from an earlier capture would read as this one's.
        let _ = std::fs::remove_file(rotated(path));
        Ok(Self { path: path.to_path_buf(), file, size: 0, cap, rotations: 0 })
    }

    /// Write one line in a single call, so nothing that stops the writer
    /// leaves half a line behind.
    pub fn write(&mut self, line: &str) -> std::io::Result<()> {
        let bytes = format!("{line}\n");
        let len = bytes.len() as u64;
        if self.size > 0 && self.size + len > self.cap {
            std::fs::rename(&self.path, rotated(&self.path))?;
            self.file = open_private(&self.path)?;
            self.size = 0;
            self.rotations += 1;
        }
        self.file.write_all(bytes.as_bytes())?;
        self.size += len;
        Ok(())
    }
}

/// What the reading task and the session share.
pub struct Live {
    sink: Sink,
    filter: Filter,
    pub seen: u64,
    pub kept: u64,
    /// Entries the library could not parse; the stream goes on after them.
    pub unparsed: u64,
    pub ended: Option<String>,
}

impl Live {
    pub fn new(sink: Sink, filter: Filter) -> Self {
        Self { sink, filter, seen: 0, kept: 0, unparsed: 0, ended: None }
    }

    pub fn take(&mut self, entry: &Entry) -> std::io::Result<()> {
        if self.ended.is_some() {
            return Ok(());
        }
        self.seen += 1;
        if self.filter.keeps(entry) {
            self.sink.write(&entry.line())?;
            self.kept += 1;
        }
        Ok(())
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
            "rotations": self.sink.rotations,
        });
        let _ = self.sink.write(&line.to_string());
        self.ended = Some(reason.to_string());
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
            Ok(log) => match live.lock().unwrap().take(&Entry::from(log)) {
                Ok(()) => continue,
                Err(e) => format!("cannot write the file: {e}"),
            },
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
    started_at: String,
    started: std::time::Instant,
    live: Arc<Mutex<Live>>,
    task: tokio::task::JoinHandle<()>,
    reset: Reset,
}

impl Capture {
    /// End it now. The task is stopped first, so the `end` line is the last.
    /// The connection is reset here, not when the stopped task drops the
    /// stream: by then `Session::reset` may have closed the tunnel.
    pub fn end(&self, reason: &str) {
        self.reset.now();
        self.task.abort();
        self.live.lock().unwrap().end(reason);
    }

    /// After `end`: wait until the connection has been reset, so the relay
    /// has let go of it before the next capture connects or the daemon exits.
    pub async fn settle(&mut self) {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), self.reset.sent()).await;
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

    pub fn report(&self) -> Map<String, Value> {
        self.check();
        let live = self.live.lock().unwrap();
        let mut out = fields! {
            "state": if live.ended.is_none() { "running" } else { "ended" },
            "path": self.spec.path.display().to_string(),
            "started_at": self.started_at,
            "seen": live.seen,
            "kept": live.kept,
            "unparsed": live.unparsed,
            "rotations": live.sink.rotations,
        };
        if !self.spec.filter.processes.is_empty() {
            out.insert("process".into(), json!(self.spec.filter.processes));
        }
        if let Some(pid) = self.spec.pid {
            out.insert("pid".into(), json!(pid));
        }
        if let Some(level) = self.spec.filter.min_level {
            out.insert("level".into(), json!(level.name()));
        }
        if live.sink.rotations > 0 {
            out.insert("previous_part".into(), json!(rotated(&self.spec.path).display().to_string()));
        }
        if let Some(reason) = &live.ended {
            out.insert("ended_reason".into(), json!(reason));
        }
        // Not with a pid: a quiet process sends nothing either.
        if live.ended.is_none()
            && self.spec.pid.is_none()
            && live.seen == 0
            && self.started.elapsed() > std::time::Duration::from_secs(5)
        {
            // Seen live: a relay connection that was dropped, not reset,
            // held the next capture for about 30 s.
            out.insert("hint".into(), json!(crate::i18n::t(
                "아직 받은 로그가 없습니다. 연결이 끊긴 직후에는 폰이 약 30초 뒤에야 로그를 보냅니다.",
                "No entries yet. Right after a dropped connection the phone starts sending about 30 s later.",
            )));
        }
        out
    }
}

/// Entries in `files` (oldest first) whose message matches, the most recent
/// `max` of them, with the number that matched in all.
pub fn search(files: &[PathBuf], grep: Option<&Regex>, max: usize) -> (Vec<Value>, u64) {
    let mut found = VecDeque::new();
    let mut matched = 0;
    for path in files {
        let Ok(file) = File::open(path) else { continue };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(Value::Object(mut entry)) = serde_json::from_str::<Value>(&line) else { continue };
            if entry.contains_key("event") {
                continue;
            }
            let message = entry.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
            if grep.is_some_and(|re| !re.is_match(&message)) {
                continue;
            }
            matched += 1;
            if max == 0 {
                continue;
            }
            if message.chars().count() > MATCH_CHARS {
                entry.insert("message".into(), json!(message.chars().take(MATCH_CHARS).collect::<String>()));
                entry.insert("message_cut".into(), json!(true));
            }
            if found.len() == max {
                found.pop_front();
            }
            found.push_back(Value::Object(entry));
        }
    }
    (found.into(), matched)
}

impl Session {
    pub async fn cmd_syslog(&mut self, args: &Value) -> Res {
        let udid = self.udid.clone();
        let request = parse(args, || paths::state_dir().join(format!("{udid}.syslog.jsonl")))?;
        match request {
            Request::Status => Ok(match &self.capture {
                Some(c) => c.report(),
                None => fields! { "state": "none" },
            }),
            Request::Start(spec) => {
                if let Some(c) = self.capture.as_ref().filter(|c| c.is_running()) {
                    return Err(Fail::not_sent("capture_running", "a capture is already running")
                        .with("path", c.spec.path.display().to_string())
                        .hint(crate::i18n::t(
                            "`iph syslog stop`으로 끝낸 뒤 다시 시작하세요.",
                            "End it with `iph syslog stop` first.",
                        )));
                }
                // The file is opened only once the relay runs: a start that
                // fails leaves an earlier capture's file as it was.
                let (stream, reset) = self.os_trace(spec.pid).await?;
                let sink = Sink::create(&spec.path, spec.max_bytes)
                    .map_err(|e| Fail::invalid(format!("{}: {e}", spec.path.display())))?;
                let mut live = Live::new(sink, spec.filter.clone());
                let started_at = clock::now_utc();
                let start = json!({
                    "event": "start",
                    "time": started_at,
                    "udid": self.udid,
                    "process": spec.filter.processes,
                    "pid": spec.pid,
                    "level": spec.filter.min_level.map(Level::name),
                    "max_mb": spec.max_bytes / (1024 * 1024),
                });
                if let Err(e) = live.sink.write(&start.to_string()) {
                    return Err(Fail::invalid(format!("{}: {e}", spec.path.display())));
                }
                let live = Arc::new(Mutex::new(live));
                let task = tokio::task::spawn_local(pump(stream, live.clone()));
                let started = std::time::Instant::now();
                let capture = Capture { spec, started_at, started, live, task, reset };
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
                    let files = [rotated(&capture.spec.path), capture.spec.path.clone()];
                    let (matches, matched) = search(&files, q.grep.as_ref(), q.max);
                    out.insert("matched".into(), json!(matched));
                    out.insert("matches".into(), json!(matches));
                }
                Ok(out)
            }
        }
    }

    /// End the capture, keeping it for `status` and `stop` to report.
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

    #[test]
    fn an_entry_becomes_one_json_line_named_by_its_executable() {
        let e = Entry::from(log("/Applications/Preferences.app/Preferences", "Error", Some(("com.apple.settings", "ui"))));
        let v: Value = serde_json::from_str(&e.line()).unwrap();
        assert_eq!(
            v,
            json!({
                "time": "2026-11-19T01:02:03.004005Z", "pid": 412, "process": "Preferences", "sender": "UIKitCore",
                "level": "error", "subsystem": "com.apple.settings", "category": "ui", "message": "tapped row",
            })
        );
        let plain = Entry::from(log("/usr/libexec/backboardd", "Notice", None));
        let v: Value = serde_json::from_str(&plain.line()).unwrap();
        assert_eq!(v["process"], "backboardd");
        assert_eq!(v["level"], "notice");
        assert!(v.get("subsystem").is_none());
    }

    #[test]
    fn the_filter_keeps_named_processes_at_or_above_a_level() {
        let any = Filter::default();
        assert!(any.keeps(&entry("SpringBoard", Level::Debug, "")));
        let f = Filter { processes: vec!["Preferences".into(), "Papago".into()], min_level: Some(Level::Notice) };
        assert!(f.keeps(&entry("Preferences", Level::Notice, "")));
        assert!(f.keeps(&entry("Papago", Level::Fault, "")));
        assert!(!f.keeps(&entry("Preferences", Level::Info, "")));
        assert!(!f.keeps(&entry("SpringBoard", Level::Error, "")));
        // Exact names only: a prefix is another process.
        assert!(!f.keeps(&entry("PreferencesExtension", Level::Error, "")));
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
    fn the_file_rolls_over_at_its_size_with_whole_lines_in_both_parts() {
        let dir = temp_dir("rotate");
        let path = dir.join("cap.jsonl");
        std::fs::write(rotated(&path), "left from before\n").unwrap();
        let mut sink = Sink::create(&path, 1000).unwrap();
        assert!(!rotated(&path).exists(), "an earlier part is removed at start");
        let line = json!({ "message": "x".repeat(80) }).to_string();
        for _ in 0..30 {
            sink.write(&line).unwrap();
        }
        assert!(sink.rotations >= 2);
        for p in [&path, &rotated(&path)] {
            assert!(std::fs::metadata(p).unwrap().len() <= 1000);
            assert!(lines(p).iter().all(|v| v["message"].as_str().unwrap().len() == 80));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_capture_ends_with_one_end_line_and_keeps_nothing_after_it() {
        let dir = temp_dir("end");
        let path = dir.join("cap.jsonl");
        let filter = Filter { processes: vec!["Papago".into()], min_level: None };
        let mut live = Live::new(Sink::create(&path, 1 << 20).unwrap(), filter);
        live.take(&entry("Papago", Level::Info, "one")).unwrap();
        live.take(&entry("SpringBoard", Level::Info, "other")).unwrap();
        live.end("the log stream ended: socket closed");
        live.end("stopped");
        live.take(&entry("Papago", Level::Info, "late")).unwrap();
        let got = lines(&path);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["message"], "one");
        assert_eq!(got[1]["event"], "end");
        assert_eq!(got[1]["reason"], "the log stream ended: socket closed");
        assert_eq!((got[1]["seen"].as_u64(), got[1]["kept"].as_u64()), (Some(2), Some(1)));
        assert_eq!(live.ended.as_deref(), Some("the log stream ended: socket closed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_returns_the_latest_matches_cut_to_length() {
        let dir = temp_dir("search");
        let older = dir.join("cap.jsonl.1");
        let newer = dir.join("cap.jsonl");
        // The current part first: starting it removes an earlier `.1`.
        let mut b = Sink::create(&newer, 1 << 20).unwrap();
        let mut a = Sink::create(&older, 1 << 20).unwrap();
        a.write(r#"{"event":"start"}"#).unwrap();
        for i in 0..3 {
            a.write(&entry("Papago", Level::Info, &format!("hit {i}")).line()).unwrap();
        }
        b.write(&entry("Papago", Level::Info, "miss").line()).unwrap();
        b.write(&entry("Papago", Level::Info, &format!("hit 3 {}", "y".repeat(900))).line()).unwrap();
        b.write(r#"{"event":"end","reason":"hit"}"#).unwrap();
        let files = [older, newer];
        let re = Regex::new("^hit").unwrap();
        let (found, matched) = search(&files, Some(&re), 2);
        assert_eq!(matched, 4);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0]["message"], "hit 2");
        assert_eq!(found[1]["message"].as_str().unwrap().chars().count(), MATCH_CHARS);
        assert_eq!(found[1]["message_cut"], true);
        let (all, matched) = search(&files, None, 0);
        assert_eq!((all.len(), matched), (0, 5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn parsed(args: Value) -> Res<Request> {
        parse(&args, || PathBuf::from("/state/u.syslog.jsonl"))
    }

    #[test]
    fn each_action_takes_only_its_own_arguments() {
        let Ok(Request::Start(spec)) = parsed(json!({
            "action": "start", "process": "Preferences", "level": "default", "pid": null, "max_mb": 2,
        })) else {
            panic!("start")
        };
        assert_eq!(spec.filter.processes, ["Preferences"]);
        assert_eq!(spec.filter.min_level, Some(Level::Notice));
        assert_eq!(spec.path, PathBuf::from("/state/u.syslog.jsonl"));
        assert_eq!(spec.max_bytes, 2 * 1024 * 1024);
        let Ok(Request::Start(spec)) = parsed(json!({ "action": "start", "process": ["A", "B"], "pid": 7, "output": "/tmp/x.jsonl" })) else {
            panic!("start with a list")
        };
        assert_eq!((spec.filter.processes.len(), spec.pid), (2, Some(7)));
        assert_eq!(spec.path, PathBuf::from("/tmp/x.jsonl"));

        assert!(matches!(parsed(json!({ "action": "stop" })), Ok(Request::Stop(None))));
        let Ok(Request::Stop(Some(q))) = parsed(json!({ "action": "stop", "grep": "a+", "max": 5000 })) else {
            panic!("stop with grep")
        };
        assert_eq!(q.max, MAX_MATCHES as usize);
        assert!(matches!(parsed(json!({ "action": "status" })), Ok(Request::Status)));

        for bad in [
            json!({}),
            json!({ "action": "pause" }),
            json!({ "action": "stop", "process": "A" }),
            json!({ "action": "status", "grep": "a" }),
            json!({ "action": "start", "output": "relative.jsonl" }),
            json!({ "action": "start", "level": "loud" }),
            json!({ "action": "start", "process": "/Applications/Preferences.app/Preferences" }),
            json!({ "action": "start", "max_mb": 0 }),
            json!({ "action": "stop", "grep": "(" }),
        ] {
            let err = parsed(bad.clone()).expect_err(&bad.to_string());
            assert_eq!(err.code, "invalid_argument", "{bad}");
        }
    }
}
