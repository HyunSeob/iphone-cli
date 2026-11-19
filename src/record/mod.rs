//! `iph record`: the phone's screen to an MP4 file while other commands run.
//!
//! The recorder has a screen-sharing session of its own with the phone, the
//! video of one display, apart from the input session's audio. Each session
//! stops only its own streams (see [`crate::device::media`]), so input
//! sessions open, close and retry as usual while a recording runs.
//!
//! The phone sends one keyframe, at the start of the stream. A packet lost
//! later damages every frame after it until the end of the recording.

pub mod hevc;
pub mod mp4;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use idevice::rsd::RsdHandshake;
use serde_json::{Map, Value, json};

use crate::{
    ax::tree::BuildOptions,
    clock,
    device::{
        media::{self, MediaControl},
        session::{Fresh, Session, bounded, reuse},
    },
    fields,
    geometry::Geometry,
    i18n::t,
    reply::{Fail, Fields, Res},
    tcp::handle::AdapterHandle,
};

use self::{
    hevc::{AccessUnit, Assembler, NAL_PPS, NAL_SPS, NAL_VPS, ParameterSets},
    mp4::{Writer, seconds, ticks_between, ticks_from_micros},
};

/// Longest wait after the video starts for its first keyframe.
const FIRST_FRAME: Duration = Duration::from_secs(5);
/// A video silent for longer than this may have stopped, and the phone is
/// asked. A still screen can be silent that long too: an iPhone 16 Pro once
/// sent no frame for 3 s, while an iPhone Duo kept sending 60 a second.
const VIDEO_STALE_US: u64 = 2_500_000;

pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// The recording file as the video packets fill it. Shared between the task
/// that receives the packets and the session.
pub struct Track {
    assembler: Assembler,
    writer: Option<Writer>,
    params: Option<ParameterSets>,
    /// RTP timestamp of the first frame written, the file's time zero.
    first_ts: Option<u32>,
    /// RTP timestamp and arrival of the last frame written.
    last: Option<(u32, u64)>,
    /// Arrival of the last datagram, written or not.
    last_arrival_us: Option<u64>,
    keyframes: u64,
    /// Why frames are no longer written, when that happened before `stop`,
    /// and the arrival of the frame that was not written.
    ended: Option<(String, u64)>,
}

pub struct Summary {
    pub path: PathBuf,
    pub bytes: u64,
    pub frames: u64,
    pub keyframes: u64,
    pub lost_packets: u64,
    pub first_loss_s: Option<f64>,
    pub duration_s: f64,
    pub width: u32,
    pub height: u32,
}

impl Track {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            assembler: Assembler::default(),
            writer: Some(Writer::create(path)?),
            params: None,
            first_ts: None,
            last: None,
            last_arrival_us: None,
            keyframes: 0,
            ended: None,
        })
    }

    /// One datagram from the video socket.
    pub fn push(&mut self, datagram: &[u8], arrival_us: u64) {
        self.last_arrival_us = Some(arrival_us);
        if self.writer.is_none() || self.ended.is_some() {
            return;
        }
        for au in self.assembler.push(datagram, arrival_us) {
            if self.ended.is_some() {
                break;
            }
            self.take(au);
        }
    }

    fn end(&mut self, reason: impl Into<String>, at_us: u64) {
        self.ended = Some((reason.into(), at_us));
    }

    fn take(&mut self, au: AccessUnit) {
        match &self.params {
            // Frames before the first keyframe cannot be decoded.
            None if !au.is_keyframe() => return,
            None => match ParameterSets::from_keyframe(&au) {
                Some(p) => {
                    self.params = Some(p);
                    self.assembler.start_at(&au);
                }
                None => return,
            },
            Some(p) => {
                let known = [(NAL_VPS, &p.vps), (NAL_SPS, &p.sps), (NAL_PPS, &p.pps)];
                if known.iter().any(|(kind, set)| au.parameter_set(*kind).is_some_and(|new| new != set.as_slice())) {
                    // The file's one hvcC describes the first ones.
                    self.end("the phone changed the video's parameter sets", au.arrival_us);
                    return;
                }
            }
        }
        // A frame whose slices were lost, or that carries only parameter sets
        // or SEI, has no picture. The previous frame lasts until the next one.
        if !au.has_picture() {
            return;
        }
        let sample = au.sample();
        let Some(writer) = self.writer.as_mut() else { return };
        if let Some((ts, _)) = self.last {
            let ticks = ticks_between(ts, au.ts);
            if writer.total_ticks() + u64::from(ticks) >= u64::from(u32::MAX) {
                self.end("the file reached the longest length it can hold", au.arrival_us);
                return;
            }
            writer.set_last_duration(ticks);
        }
        if let Err(e) = writer.append(&sample, au.is_keyframe()) {
            self.end(format!("writing the file failed: {e}"), au.arrival_us);
            return;
        }
        if au.is_keyframe() {
            self.keyframes += 1;
        }
        self.first_ts.get_or_insert(au.ts);
        self.last = Some((au.ts, au.arrival_us));
    }

    /// The picture size, once the first keyframe has come.
    pub fn picture(&self) -> Option<(u32, u32)> {
        self.params.as_ref().map(|p| (p.width, p.height))
    }

    pub fn frames(&self) -> u64 {
        self.writer.as_ref().map_or(0, |w| w.samples().len() as u64)
    }

    pub fn lost_packets(&self) -> u64 {
        self.assembler.lost
    }

    /// Seconds into the file of the first frame a lost packet damaged.
    pub fn first_loss_s(&self) -> Option<f64> {
        let (first, loss) = (self.first_ts?, self.assembler.first_loss_ts?);
        let ticks = loss.wrapping_sub(first);
        Some(if ticks > u32::MAX / 2 { 0.0 } else { seconds(u64::from(ticks)) })
    }

    pub fn discard(&mut self) {
        if let Some(w) = self.writer.take() {
            w.discard();
        }
    }

    /// Close the file: the last frame lasts until `stop_us`. An incomplete
    /// frame still being received is left out.
    pub fn finish(&mut self, stop_us: u64) -> Result<Summary, String> {
        let mut writer = self.writer.take().ok_or("the recording was already closed")?;
        let Some(params) = self.params.clone() else {
            writer.discard();
            return Err("the phone sent no video".into());
        };
        if let Some((_, arrival)) = self.last {
            writer.set_last_duration(ticks_from_micros(stop_us.saturating_sub(arrival)));
        }
        let frames = writer.samples().len() as u64;
        let duration_s = seconds(writer.total_ticks());
        let target = writer.path().to_path_buf();
        let bytes = writer.finish(&params).map_err(|e| format!("writing {}: {e}", target.display()))?;
        Ok(Summary {
            path: target,
            bytes,
            frames,
            keyframes: self.keyframes,
            lost_packets: self.assembler.lost,
            first_loss_s: self.first_loss_s(),
            duration_s,
            width: params.width,
            height: params.height,
        })
    }
}

impl Drop for Track {
    /// A track dropped before `finish` (a start cut off by a timeout) leaves no file.
    fn drop(&mut self) {
        self.discard();
    }
}

/// The recorder's own screen-sharing session: the video of one display,
/// written to the track as it comes.
pub struct VideoStream {
    media: MediaControl,
    /// Names `session_id` from before the video is asked for until the
    /// phone is seen to run it no more. A stream dropped without
    /// [`stop`](Self::stop), such as a start cut off, is stopped from it by
    /// housekeeping (see `Session::reap_media`).
    marker: PathBuf,
    session_id: uuid::Uuid,
    receive: tokio::task::JoinHandle<()>,
    /// Checks in a row that the phone did not answer.
    unanswered: u32,
}

impl VideoStream {
    async fn open(
        adapter: &mut AdapterHandle,
        rsd: &mut RsdHandshake,
        display: i64,
        track: Arc<Mutex<Track>>,
        marker: PathBuf,
    ) -> anyhow::Result<Self> {
        let mut media = MediaControl::connect(adapter, rsd).await?;
        let session_id = uuid::Uuid::new_v4();
        media::mark(&marker, &session_id.to_string());
        let udp = media::start_video(&mut media, adapter, display, session_id).await?;
        let receive = tokio::spawn(async move {
            while let Ok(packet) = udp.recv().await {
                track.lock().unwrap().push(&packet.data, now_us());
            }
        });
        Ok(Self { media, marker, session_id, receive, unanswered: 0 })
    }

    /// Why the recording must end, asked while the video is silent: the
    /// phone no longer runs it, or did not answer `UNANSWERED` checks in a
    /// row. One check it misses proves nothing. A connection the last check
    /// cut off is made again first, with time of its own.
    async fn gone(&mut self) -> Option<String> {
        let running = async {
            bounded("display service", 5, self.media.relink()).await?;
            let status = bounded("status", 2, self.media.status()).await?;
            Ok(!media::streams_of(&status, &self.session_id.to_string()).is_empty())
        };
        gone(running.await, &mut self.unanswered)
    }

    /// Stop the video on the phone. The input session's streams go on.
    pub async fn stop(mut self) {
        let stopped = self.media.stop_own(&self.session_id.to_string(), &self.marker).await;
        crate::device::ax::trace(|| format!("record: video of session {} stopped: {stopped:?}", self.session_id));
    }
}

/// Status checks in a row the phone may leave unanswered before a silent
/// recording ends.
const UNANSWERED: u32 = 3;

fn gone(running: Result<bool, String>, unanswered: &mut u32) -> Option<String> {
    match running {
        Ok(true) => {
            *unanswered = 0;
            None
        }
        Ok(false) => Some("the phone stopped sending the video".into()),
        Err(e) => {
            *unanswered += 1;
            (*unanswered >= UNANSWERED).then(|| {
                format!("the phone stopped sending the video and did not answer {UNANSWERED} checks in a row ({e})")
            })
        }
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        self.receive.abort();
    }
}

pub struct Live<S> {
    display: i64,
    path: PathBuf,
    started_at: String,
    started: Instant,
    track: Arc<Mutex<Track>>,
    geometry: Geometry,
    /// What must be stopped on the phone when the recording ends.
    stream: S,
}

/// The daemon's one recording slot.
#[derive(Default)]
pub enum Recorder<S = VideoStream> {
    #[default]
    Idle,
    Recording(Live<S>),
    /// Ended. `collected` once its report has been handed out: by `stop`,
    /// the next `start`, or the end of the script that started it.
    Stopped { report: Res<Fields>, collected: bool, at: Instant },
}

impl Recorder {
    /// The client session the recorded video was started for.
    pub fn session_id(&self) -> Option<String> {
        let Recorder::Recording(live) = self else { return None };
        Some(live.stream.session_id.to_string())
    }
}

impl<S> Recorder<S> {
    pub fn can_start(&self) -> Res<()> {
        let Recorder::Recording(live) = self else { return Ok(()) };
        Err(Fail::not_sent("recording_running", "a recording is already running")
            .with("path", live.path.display().to_string())
            .hint(t("`iph record stop`으로 끝낸 뒤 다시 시작하세요.", "End it with `iph record stop` first.")))
    }

    pub fn is_recording(&self) -> bool {
        matches!(self, Recorder::Recording(_))
    }

    /// Whether the daemon must stay: while recording, and for `linger` after
    /// a recording ended on its own, so its report can still be collected.
    pub fn holds_daemon(&self, linger: Duration) -> bool {
        match self {
            Recorder::Recording(_) => true,
            Recorder::Stopped { collected: false, at, .. } => at.elapsed() < linger,
            _ => false,
        }
    }

    /// The report of a recording that ended without anyone being told.
    pub fn take_uncollected(&mut self) -> Option<Res<Fields>> {
        match self {
            Recorder::Stopped { report, collected: collected @ false, .. } => {
                *collected = true;
                Some(report.clone())
            }
            _ => None,
        }
    }

    /// End the recording now: close the file, keep the report for `stop`,
    /// and hand back what must be stopped on the phone.
    pub fn end(&mut self, reason: &str) -> Option<S> {
        self.end_at(reason, now_us())
    }

    /// `end`, with the last frame lasting until `stop_us`.
    pub fn end_at(&mut self, reason: &str, stop_us: u64) -> Option<S> {
        match std::mem::take(self) {
            Recorder::Recording(live) => {
                let (report, stream) = live.finish(stop_us, Some(reason));
                *self = Recorder::Stopped { report, collected: false, at: Instant::now() };
                Some(stream)
            }
            other => {
                *self = other;
                None
            }
        }
    }

    /// End a recording whose file stopped taking frames, or that has run
    /// for `max`.
    pub fn check(&mut self, now_us: u64, max: Duration) -> Option<S> {
        let Recorder::Recording(live) = self else { return None };
        let ended = live.track.lock().unwrap().ended.clone();
        let (reason, at) = if let Some((reason, at)) = ended {
            (reason, at)
        } else if live.started.elapsed() >= max {
            (format!("the recording reached its longest length, {} s (record_max_secs)", max.as_secs()), now_us)
        } else {
            return None;
        };
        self.end_at(&reason, at)
    }

    /// When the last video packet came, if none has come since for longer
    /// than `VIDEO_STALE_US`.
    pub fn silent_since(&self, now_us: u64) -> Option<u64> {
        let Recorder::Recording(live) = self else { return None };
        let last = live.track.lock().unwrap().last_arrival_us;
        last.filter(|l| now_us.saturating_sub(*l) > VIDEO_STALE_US)
    }

    fn stream_mut(&mut self) -> Option<&mut S> {
        match self {
            Recorder::Recording(live) => Some(&mut live.stream),
            _ => None,
        }
    }

    pub fn stop(&mut self, stop_us: u64) -> (Res<Fields>, Option<S>) {
        match std::mem::take(self) {
            Recorder::Recording(live) => {
                let (report, stream) = live.finish(stop_us, None);
                *self = Recorder::Stopped { report: report.clone(), collected: true, at: Instant::now() };
                (report, Some(stream))
            }
            other => {
                *self = other;
                let report = self.take_uncollected().unwrap_or_else(|| Err(Fail::not_sent("no_recording", "no recording to stop")));
                (report, None)
            }
        }
    }

    pub fn status(&self) -> Fields {
        match self {
            Recorder::Idle => fields! { "state": "idle" },
            Recorder::Recording(live) => live.report(),
            Recorder::Stopped { report, .. } => report_fields(report.clone()),
        }
    }
}

/// A report for a reply: the fields of a recording, or why it failed.
pub fn report_fields(report: Res<Fields>) -> Fields {
    report.unwrap_or_else(|f| fields! { "state": "failed", "error": f.error_json() })
}

impl<S> Live<S> {
    fn report(&self) -> Fields {
        let track = self.track.lock().unwrap();
        let mut out = fields! {
            "state": "recording",
            "path": self.path.display().to_string(),
            "display": self.display,
            "started_at": self.started_at,
            "seconds": (self.started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            "frames": track.frames(),
            "keyframes": track.keyframes,
            "lost_packets": track.lost_packets(),
            "width": self.geometry.width,
            "height": self.geometry.height,
            "scale": self.geometry.scale,
            "ended_reason": track.ended.as_ref().map(|(reason, _)| reason.clone()),
        };
        add_loss(&mut out, track.lost_packets(), track.first_loss_s());
        out
    }

    fn finish(self, stop_us: u64, ended: Option<&str>) -> (Res<Fields>, S) {
        let report = self.report_finished(stop_us, ended);
        (report, self.stream)
    }

    fn report_finished(&self, stop_us: u64, ended: Option<&str>) -> Res<Fields> {
        let mut track = self.track.lock().unwrap();
        let summary = track.finish(stop_us).map_err(|e| {
            let mut f = Fail::not_sent("record_failed", e);
            if let Some(reason) = ended {
                f = f.with("ended_reason", reason);
            }
            f
        })?;
        let mut out = fields! {
            "state": "stopped",
            "path": summary.path.display().to_string(),
            "duration_s": (summary.duration_s * 1000.0).round() / 1000.0,
            "frames": summary.frames,
            "keyframes": summary.keyframes,
            "lost_packets": summary.lost_packets,
            "bytes": summary.bytes,
            "width": summary.width,
            "height": summary.height,
            "scale": self.geometry.scale,
            "display": self.display,
            "started_at": self.started_at,
        };
        let stopped_early = track.ended.as_ref().map(|(reason, _)| reason.clone());
        if let Some(reason) = ended.map(String::from).or(stopped_early) {
            out.insert("ended_reason".into(), json!(reason));
        }
        add_loss(&mut out, summary.lost_packets, summary.first_loss_s);
        Ok(out)
    }
}

/// Where the picture is likely damaged: the phone sends one keyframe, so
/// it stays damaged from the first loss to the end. AVFoundation still
/// calls such a file playable.
fn add_loss(out: &mut Fields, lost_packets: u64, first_loss_s: Option<f64>) {
    if lost_packets == 0 {
        return;
    }
    let at = first_loss_s.map(|s| (s * 1000.0).round() / 1000.0);
    if let Some(at) = at {
        out.insert("first_loss_s".into(), json!(at));
    }
    let from = at.map_or_else(|| "a lost packet".to_string(), |s| format!("{s} s"));
    out.insert("warning".into(), json!({
        "code": "packets_lost",
        "message": format!("{lost_packets} video packets were lost; the picture is likely damaged from {from} to the end"),
        "hint": t(
            "폰은 녹화 처음에만 키프레임을 보내므로 패킷이 빠지면 그 뒤 화면이 끝까지 깨집니다. USB로 연결해 다시 녹화하세요.",
            "The phone sends a keyframe only at the start, so a lost packet damages the rest of the file. Record again over USB.",
        ),
    }));
}

#[derive(Debug, PartialEq)]
pub enum Request {
    Start { path: PathBuf, display: Option<i64> },
    Stop,
    Status,
}

/// Parse the arguments every surface sends. `default_path` is used when no
/// `path` is given; a given one must be absolute, since the daemon writes it.
pub fn parse(args: &Value, default_path: impl FnOnce() -> PathBuf) -> Res<Request> {
    let given: Map<String, Value> = match args {
        Value::Null => Map::new(),
        Value::Object(m) => m.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => return Err(Fail::invalid("record takes an object of arguments")),
    };
    let action = given
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Fail::invalid("record needs an action: start, stop or status"))?;
    let allowed: &[&str] = match action {
        "start" => &["action", "path", "display"],
        "stop" | "status" => &["action"],
        other => return Err(Fail::invalid(format!("unknown record action `{other}` (start, stop, status)"))),
    };
    if let Some(key) = given.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(Fail::invalid(format!("`record {action}` takes no `{key}`")));
    }
    match action {
        "start" => {
            let path = match given.get("path") {
                None => default_path(),
                Some(v) => {
                    let path = PathBuf::from(v.as_str().ok_or_else(|| Fail::invalid("`path` takes a path"))?);
                    if !path.is_absolute() {
                        return Err(Fail::invalid("`path` must be an absolute path"));
                    }
                    path
                }
            };
            let display = match given.get("display") {
                None => None,
                Some(v) => Some(
                    v.as_i64()
                        .filter(|d| (0..=15).contains(d))
                        .ok_or_else(|| Fail::invalid("`display` takes a display number from 0 to 15"))?,
                ),
            };
            Ok(Request::Start { path, display })
        }
        "stop" => Ok(Request::Stop),
        _ => Ok(Request::Status),
    }
}

/// A file in iph's state directory, for a recording whose caller did not
/// name one.
pub fn default_path() -> PathBuf {
    crate::paths::state_dir().join(format!("iph-record-{}.mp4", clock::file_stamp()))
}

/// Displays tried in order when `start` names none. The first whose video
/// has the shape of the screen being read is recorded.
pub const DEFAULT_DISPLAYS: [i64; 3] = [1, 0, 2];
/// The longest `start` tries displays. With the screen read before and the
/// last attempt's stop after, it stays within the daemon's 90 s for a
/// request, which would cut the start off with its video running.
const START_WITHIN: Duration = Duration::from_secs(45);
/// Longest wait for the phone to start one display's video.
const OPEN_WITHIN: Duration = Duration::from_secs(20);


impl Session {
    /// End a recording that ran out (see [`Recorder::check`]) or whose
    /// video the phone no longer sends, and stop its video on the phone.
    pub(crate) async fn check_recording(&mut self) {
        let max = Duration::from_secs(self.config.daemon.record_max_secs.max(1));
        let now = now_us();
        let mut ended = self.recorder.check(now, max);
        let silent = self.recorder.silent_since(now);
        let gone = match (silent, self.recorder.stream_mut()) {
            (None, Some(stream)) => {
                stream.unanswered = 0;
                None
            }
            (Some(_), Some(stream)) if ended.is_none() => stream.gone().await,
            _ => None,
        };
        if let (Some(reason), Some(last)) = (gone, silent) {
            ended = self.recorder.end_at(&reason, last);
        }
        if let Some(stream) = ended {
            stream.stop().await;
        }
    }

    /// End the recording with `reason` if it runs, and hand out its report
    /// if nobody has had it yet.
    pub(crate) async fn collect_recording(&mut self, reason: &str) -> Option<Res<Fields>> {
        self.check_recording().await;
        if let Some(stream) = self.recorder.end(reason) {
            stream.stop().await;
        }
        self.recorder.take_uncollected()
    }

    pub async fn cmd_record(&mut self, args: &Value) -> Res {
        let request = parse(args, default_path)?;
        self.check_recording().await;
        match request {
            Request::Status => Ok(self.recorder.status()),
            Request::Stop => {
                // The file ends now, before the frames that keep coming
                // while the stream stops.
                let (report, stream) = self.recorder.stop(now_us());
                if let Some(stream) = stream {
                    stream.stop().await;
                }
                report
            }
            Request::Start { path, display } => {
                self.recorder.can_start()?;
                // A recording that ended on its own is reported here rather than lost.
                let previous = self.recorder.take_uncollected().map(report_fields);
                let started = self.start_recording(&path, display).await;
                match (started, previous) {
                    (Ok(mut out), Some(p)) => {
                        out.insert("previous".into(), Value::Object(p));
                        Ok(out)
                    }
                    (Err(f), Some(p)) => Err(f.with("previous", Value::Object(p))),
                    (started, None) => started,
                }
            }
        }
    }

    async fn start_recording(&mut self, path: &Path, display: Option<i64>) -> Res {
        let (_, screen) = self.screen(Fresh::MaxAge(reuse()), false, BuildOptions::default()).await?;
        let points = (screen.width, screen.height);
        // A display the caller named is tried alone; its failure is the answer.
        let candidates = display.map_or(DEFAULT_DISPLAYS.to_vec(), |d| vec![d]);
        let deadline = Instant::now() + START_WITHIN;
        let mut tried = Vec::new();
        let mut refused = None;
        for candidate in candidates {
            if Instant::now() >= deadline {
                return Err(start_timeout().with("tried", tried));
            }
            match self.start_on(path, candidate, points, deadline).await {
                Ok(live) => {
                    self.recorder = Recorder::Recording(live);
                    return Ok(self.recorder.status());
                }
                Err(f) if f.code == "start_timeout" => return Err(f.with("tried", tried)),
                Err(f) if display.is_none() && matches!(f.code.as_str(), "no_video" | "display_mismatch" | "record_failed") => {
                    tried.push(json!({ "display": candidate, "error": f.code, "message": f.message }));
                    if f.code == "record_failed" {
                        refused = Some(f);
                    }
                }
                Err(f) => return Err(f),
            }
        }
        // The phone refused every display: why is in its answer, not in the screen.
        if let Some(f) = refused.filter(|_| tried.iter().all(|t| t["error"] == "record_failed")) {
            return Err(f.with("tried", tried));
        }
        Err(Fail::not_sent("no_video", "no display sent video of the screen being read")
            .with("tried", tried)
            .hint(t(
                "폰이 화면을 녹화할 수 없는 상태입니다. 잠금을 풀고 다시 시도하세요.",
                "The phone would not share this screen. Unlock it and try again.",
            )))
    }

    /// Start the video of `display` going to a new file, and wait for the
    /// first keyframe, giving up at `deadline`.
    async fn start_on(&mut self, path: &Path, display: i64, points: (f64, f64), deadline: Instant) -> Res<Live<VideoStream>> {
        let track = Track::create(path)
            .map_err(|e| Fail::not_sent("write_failed", format!("{}: {e}", path.display())))?;
        let track = Arc::new(Mutex::new(track));
        let marker = crate::paths::media_session(&self.udid);
        let tunnel = self.ensure_tunnel().await?;
        let mut adapter = tunnel.adapter.clone();
        let open = VideoStream::open(&mut adapter, &mut tunnel.rsd, display, track.clone(), marker);
        let stream = match tokio::time::timeout_at(deadline.min(Instant::now() + OPEN_WITHIN).into(), open).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => return Err(Fail::not_sent("record_failed", format!("video stream: {e:#}")).with("display", display)),
            Err(_) => return Err(Fail::not_sent("record_failed", "video stream: no answer in time").with("display", display)),
        };
        let until = deadline.min(Instant::now() + FIRST_FRAME);
        let picture = loop {
            if let Some(p) = track.lock().unwrap().picture() {
                break Some(p);
            }
            if Instant::now() >= until {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let checked = match picture {
            None if until == deadline => Err(start_timeout()),
            None => Err(Fail::not_sent("no_video", format!("display {display} sent no video"))
                .hint(t("다른 디스플레이를 `--display`로 고르세요.", "Pick another display with `--display`."))),
            Some(pixels) => Geometry::fit(points, pixels),
        };
        match checked {
            Ok(geometry) => Ok(Live {
                display,
                path: path.to_path_buf(),
                started_at: clock::now_utc(),
                started: Instant::now(),
                track,
                geometry,
                stream,
            }),
            Err(f) => {
                // Now, not when the receiving task lets go of it: the next
                // display's attempt writes the same file.
                track.lock().unwrap().discard();
                stream.stop().await;
                Err(f.with("display", display))
            }
        }
    }
}

/// `start` ran out of its time. Not a verdict on any display, so it is
/// returned as it is and never passed over for the next display.
fn start_timeout() -> Fail {
    Fail::not_sent("start_timeout", format!("`start` gave up after {} s before any display sent video", START_WITHIN.as_secs()))
        .hint(t("다시 시도하거나 `--display`로 디스플레이를 고르세요.", "Try again, or pick a display with `--display`."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::hevc::{PAYLOAD_TYPE, tests::sps_nal};

    fn rtp(seq: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
        packet(seq, ts, true, payload)
    }

    fn packet(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut d = vec![0x80, PAYLOAD_TYPE | if marker { 0x80 } else { 0 }];
        d.extend_from_slice(&seq.to_be_bytes());
        d.extend_from_slice(&ts.to_be_bytes());
        d.extend_from_slice(&7u32.to_be_bytes());
        d.extend_from_slice(payload);
        d
    }

    /// A keyframe in one aggregation packet: VPS, SPS, PPS and an IDR slice.
    fn keyframe_with(pps: u8) -> Vec<u8> {
        let mut ap = vec![48 << 1, 1];
        for nal in [vec![NAL_VPS << 1, 1, 0xaa], sps_nal(1408, 2016), vec![NAL_PPS << 1, 1, pps], vec![19 << 1, 1, 0x11, 0x22]] {
            ap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            ap.extend_from_slice(&nal);
        }
        ap
    }

    fn keyframe() -> Vec<u8> {
        keyframe_with(0xbb)
    }

    /// A keyframe in two packets: the parameter sets, then the IDR slice.
    fn keyframe_parts() -> [Vec<u8>; 2] {
        let mut ap = vec![48 << 1, 1];
        for nal in [vec![NAL_VPS << 1, 1, 0xaa], sps_nal(1408, 2016), vec![NAL_PPS << 1, 1, 0xbb]] {
            ap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            ap.extend_from_slice(&nal);
        }
        [ap, vec![19 << 1, 1, 0x11, 0x22]]
    }

    const P_FRAME: [u8; 4] = [1 << 1, 1, 0x33, 0x44];

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("iph-record-{}-{name}", std::process::id())).join("clip.mp4")
    }

    /// A recording of two frames, the second arriving at 16 666 µs. Its
    /// stream is a plain value to see that ending hands it back.
    fn live(path: &Path) -> Live<&'static str> {
        let mut track = Track::create(path).unwrap();
        track.push(&rtp(1, 1000, &keyframe()), 0);
        track.push(&rtp(2, 1400, &P_FRAME), 16_666);
        live_of(path, track)
    }

    fn live_of(path: &Path, track: Track) -> Live<&'static str> {
        Live {
            display: 1,
            path: path.to_path_buf(),
            started_at: "2026-11-19T00:00:00Z".into(),
            started: Instant::now(),
            track: Arc::new(Mutex::new(track)),
            geometry: Geometry::fit((352.0, 504.0), (1408, 2016)).unwrap(),
            stream: "video",
        }
    }

    const LONG: Duration = Duration::from_secs(3600);

    #[test]
    fn a_start_that_ran_out_of_time_is_its_own_failure_and_no_display_miss() {
        let f = start_timeout();
        assert_eq!(f.code, "start_timeout");
        assert!(f.message.contains("45 s"), "{}", f.message);
        assert!(!matches!(f.code.as_str(), "no_video" | "display_mismatch" | "record_failed"));
    }

    #[test]
    fn a_track_starts_at_the_first_keyframe_and_its_last_frame_lasts_until_stop() {
        let path = temp("track");
        let mut track = Track::create(&path).unwrap();
        track.push(&rtp(1, 600, &P_FRAME), 0);
        assert_eq!((track.frames(), track.picture()), (0, None), "nothing before a keyframe");
        track.push(&rtp(2, 1000, &keyframe()), 10);
        assert_eq!(track.picture(), Some((1408, 2016)));
        track.push(&rtp(3, 1400, &P_FRAME), 16_676);
        track.push(&rtp(5, 1800, &P_FRAME), 33_342);
        let s = track.finish(1_033_342).unwrap();
        assert_eq!((s.frames, s.keyframes, s.lost_packets), (3, 1, 1));
        assert!((s.duration_s - (800.0 + 24_000.0) / 24_000.0).abs() < 1e-9);
        assert_eq!(std::fs::metadata(&s.path).unwrap().len(), s.bytes);
        assert!(track.finish(0).is_err(), "closed once");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_frame_without_a_picture_is_not_written_and_the_one_before_lasts_through_it() {
        let [params_only, _] = keyframe_parts();
        let path = temp("empty-sample");
        let mut track = Track::create(&path).unwrap();
        track.push(&rtp(1, 1000, &keyframe()), 0);
        track.push(&rtp(2, 1200, &params_only), 8_333);
        assert_eq!(track.frames(), 1, "parameter sets alone are no sample");
        track.push(&rtp(3, 1400, &P_FRAME), 16_666);
        track.push(&rtp(4, 1500, &[39 << 1, 1, 5, 0x80]), 20_000);
        assert_eq!(track.frames(), 2, "an SEI alone is no sample");
        // The fragments of the frame at 1600 lose a packet in between.
        track.push(&packet(5, 1600, false, &[49 << 1, 1, 0x80 | 1, 9]), 25_000);
        track.push(&packet(7, 1600, true, &[49 << 1, 1, 0x40 | 1, 9]), 25_000);
        assert_eq!((track.frames(), track.lost_packets()), (2, 1), "a frame that lost its slice is no sample");
        track.push(&rtp(8, 1800, &P_FRAME), 33_333);
        let s = track.finish(1_033_333).unwrap();
        assert_eq!(s.frames, 3);
        assert!((s.duration_s - (800.0 + 24_000.0) / 24_000.0).abs() < 1e-9, "{}", s.duration_s);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn packets_lost_before_the_first_keyframe_damage_nothing() {
        let path = temp("early-loss");
        let mut track = Track::create(&path).unwrap();
        track.push(&rtp(1, 600, &P_FRAME), 0);
        // Packets 2 to 4 never come.
        track.push(&rtp(5, 1000, &keyframe()), 10);
        track.push(&rtp(6, 1400, &P_FRAME), 16_676);
        assert_eq!((track.lost_packets(), track.first_loss_s()), (0, None));
        let mut r = Recorder::Recording(live_of(&path, track));
        let (report, _) = r.stop(1_000_000);
        let report = report.unwrap();
        assert_eq!(report["lost_packets"], 0);
        assert!(report.get("warning").is_none(), "{report:?}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn packets_lost_from_the_first_keyframe_on_are_reported() {
        let [params, slice] = keyframe_parts();
        let path = temp("keyframe-loss");
        // (packets before the keyframe, inside it, right after it) -> (lost, first_loss_s)
        for (before, inside, after, says) in [
            (false, true, false, (1, Some(0.0))),
            (true, true, false, (1, Some(0.0))),
            (true, false, false, (0, None)),
            (false, false, true, (2, Some(400.0 / 24_000.0))),
        ] {
            let mut track = Track::create(&path).unwrap();
            track.push(&rtp(1, 600, &P_FRAME), 0);
            let mut seq = if before { 3 } else { 2 };
            track.push(&packet(seq, 1000, false, &params), 10);
            seq += if inside { 2 } else { 1 };
            track.push(&packet(seq, 1000, true, &slice), 20);
            seq += if after { 3 } else { 1 };
            track.push(&rtp(seq, 1400, &P_FRAME), 16_676);
            assert_eq!(track.picture(), Some((1408, 2016)));
            assert_eq!((track.lost_packets(), track.first_loss_s()), says, "before {before}, inside {inside}, after {after}");
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_whole_frame_after_one_cut_short_is_written_too() {
        let path = temp("cut-short");
        let mut track = Track::create(&path).unwrap();
        track.push(&rtp(1, 1000, &keyframe()), 0);
        // The frame at 1400 loses its last packet; the one at 1800 is whole.
        track.push(&packet(2, 1400, false, &P_FRAME), 16_666);
        track.push(&rtp(4, 1800, &P_FRAME), 33_332);
        assert_eq!(track.frames(), 3, "the frame at 1800 is not held back");
        let s = track.finish(50_000).unwrap();
        assert_eq!((s.frames, s.lost_packets), (3, 1));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_silent_video_ends_on_a_definite_answer_or_several_missed_checks() {
        let mut unanswered = 0;
        assert_eq!(gone(Err("no answer within 2 s".into()), &mut unanswered), None, "one missed check proves nothing");
        assert_eq!(gone(Err("timed out".into()), &mut unanswered), None);
        assert_eq!(gone(Ok(true), &mut unanswered), None);
        assert_eq!(unanswered, 0, "an answer starts the count again");
        assert_eq!(gone(Err("a".into()), &mut unanswered), None);
        assert_eq!(gone(Err("b".into()), &mut unanswered), None);
        let reason = gone(Err("not connected".into()), &mut unanswered).unwrap();
        assert!(reason.contains("did not answer 3 checks in a row") && reason.contains("not connected"), "{reason}");
        assert_eq!(gone(Ok(false), &mut 0).as_deref(), Some("the phone stopped sending the video"));
    }

    #[test]
    fn a_new_parameter_set_ends_the_recording_with_its_reason() {
        for (name, changed) in [("pps", keyframe_with(0xcc)), ("same", keyframe())] {
            let path = temp(&format!("params-{name}"));
            let l = live(&path);
            let track = l.track.clone();
            let mut r = Recorder::Recording(l);
            track.lock().unwrap().push(&rtp(3, 1800, &changed), 33_332);
            track.lock().unwrap().push(&rtp(4, 2200, &P_FRAME), 50_000);
            let ended = r.check(50_000, LONG);
            if name == "same" {
                assert!(ended.is_none() && r.is_recording(), "the keyframe it began with again changes nothing");
                assert_eq!(track.lock().unwrap().frames(), 4);
                std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
                continue;
            }
            assert_eq!(ended, Some("video"), "the stream is handed back to be stopped");
            let report = r.take_uncollected().unwrap().unwrap();
            assert_eq!(report["ended_reason"], "the phone changed the video's parameter sets");
            assert_eq!(report["frames"], 2, "nothing written against the old hvcC");
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn a_second_start_and_a_stop_with_nothing_running_are_refused_and_change_nothing() {
        let path = temp("slot");
        let mut r: Recorder<&str> = Recorder::Idle;
        assert_eq!(r.stop(0).0.unwrap_err().code, "no_recording");
        assert!(matches!(r, Recorder::Idle));
        assert!(r.can_start().is_ok());

        r = Recorder::Recording(live(&path));
        let refused = r.can_start().unwrap_err();
        assert_eq!(refused.code, "recording_running");
        assert_eq!(refused.extra["path"], json!(path.display().to_string()));
        assert!(r.is_recording());
        assert_eq!(r.status()["state"], "recording");
        assert_eq!(r.status()["frames"], 2);

        let (report, stream) = r.stop(1_016_666);
        let report = report.unwrap();
        assert_eq!(stream, Some("video"));
        assert_eq!(report["state"], "stopped");
        assert!(report.get("warning").is_none() && report.get("first_loss_s").is_none(), "no loss, no warning");
        assert_eq!((report["frames"].clone(), report["width"].clone()), (json!(2), json!(1408)));
        assert!(path.exists());
        assert_eq!(r.stop(0).0.unwrap_err().code, "no_recording");
        assert_eq!(r.status()["state"], "stopped", "status keeps the last report");
        assert!(r.can_start().is_ok());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_loss_is_reported_with_the_time_into_the_file_where_the_damage_starts() {
        let path = temp("loss");
        let l = live(&path);
        let track = l.track.clone();
        let mut r = Recorder::Recording(l);
        assert!(r.status().get("warning").is_none());
        // Packets 3 and 4 never come; the frame after them starts 1 s into the file.
        track.lock().unwrap().push(&rtp(5, 25_000, &P_FRAME), 1_000_000);
        let status = r.status();
        assert_eq!((status["lost_packets"].clone(), status["first_loss_s"].clone()), (json!(2), json!(1.0)));
        assert_eq!(status["warning"]["code"], "packets_lost");
        let report = r.stop(2_000_000).0.unwrap();
        assert_eq!(report["first_loss_s"], 1.0);
        assert_eq!(report["warning"]["code"], "packets_lost");
        assert!(report["warning"]["message"].as_str().unwrap().contains("1 s"));
        assert!(report["warning"]["hint"].as_str().is_some());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_recording_that_ended_on_its_own_is_reported_once_and_holds_the_daemon_a_while() {
        let path = temp("ended");
        let mut r = Recorder::Recording(live(&path));
        assert!(r.holds_daemon(Duration::ZERO), "a running recording holds the daemon");
        assert_eq!(r.end("the connection to the phone was reset"), Some("video"));
        assert!(!r.is_recording());
        assert!(path.exists(), "the file is complete without a stop");
        assert!(r.holds_daemon(LONG), "its report waits to be collected");
        assert!(!r.holds_daemon(Duration::ZERO), "but not for longer than a request would keep the daemon");
        assert_eq!(r.status()["ended_reason"], "the connection to the phone was reset", "status does not collect");
        let report = r.take_uncollected().unwrap().unwrap();
        assert_eq!(report["ended_reason"], "the connection to the phone was reset");
        assert!(r.take_uncollected().is_none(), "handed out once");
        assert!(!r.holds_daemon(LONG));
        assert_eq!(r.stop(0).0.unwrap_err().code, "no_recording");
        assert_eq!(r.end("again"), None, "nothing more to stop on the phone");

        let mut r = Recorder::Recording(live(&path));
        r.end("the daemon stopped");
        assert_eq!(r.stop(0).0.unwrap()["ended_reason"], "the daemon stopped", "stop collects it too");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_video_that_went_silent_ends_where_its_last_packet_came() {
        let path = temp("silent");
        let l = live(&path);
        let track = l.track.clone();
        let mut r = Recorder::Recording(l);
        assert_eq!(r.silent_since(16_666 + VIDEO_STALE_US), None, "not yet silent long enough");
        assert_eq!(r.silent_since(16_667 + VIDEO_STALE_US), Some(16_666));
        assert!(r.check(16_667 + VIDEO_STALE_US, LONG).is_none(), "silence alone ends nothing: a still screen is silent too");
        // A datagram that makes no frame still shows the video is coming.
        track.lock().unwrap().push(&[0x80, 72, 0, 9], 20_000);
        assert_eq!(r.silent_since(16_667 + VIDEO_STALE_US), None);
        let last = r.silent_since(20_001 + VIDEO_STALE_US).unwrap();
        assert_eq!(r.end_at("the phone stopped sending the video", last), Some("video"));
        let report = r.take_uncollected().unwrap().unwrap();
        assert_eq!(report["ended_reason"], "the phone stopped sending the video");
        assert_eq!(report["duration_s"], json!(((400.0 + 80.0) / 24_000.0 * 1000.0_f64).round() / 1000.0));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_recording_ends_at_its_longest_length() {
        let path = temp("longest");
        let mut r = Recorder::Recording(live(&path));
        assert!(r.check(16_666, LONG).is_none());
        assert_eq!(r.check(16_666, Duration::ZERO), Some("video"));
        let report = r.take_uncollected().unwrap().unwrap();
        assert_eq!(report["ended_reason"], "the recording reached its longest length, 0 s (record_max_secs)");
        assert!(path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_track_given_up_on_does_not_take_the_next_attempts_file_with_it() {
        let path = temp("next-attempt");
        let mut given_up = Track::create(&path).unwrap();
        given_up.discard();
        let mut next = Track::create(&path).unwrap();
        next.push(&rtp(1, 1000, &keyframe()), 0);
        // The receiving task of the given-up attempt lets go of it late.
        drop(given_up);
        assert!(mp4::Writer::part_path(&path).exists());
        assert!(next.finish(1_000).is_ok() && path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_track_given_up_before_it_was_finished_leaves_no_file() {
        let path = temp("given-up");
        let mut track = Track::create(&path).unwrap();
        track.push(&rtp(1, 1000, &keyframe()), 0);
        assert!(mp4::Writer::part_path(&path).exists());
        drop(track);
        assert!(!mp4::Writer::part_path(&path).exists() && !path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn each_action_takes_only_its_own_arguments() {
        let parsed = |args: Value| parse(&args, || PathBuf::from("/state/default.mp4"));
        let absolute = std::env::temp_dir().join("a.mp4");
        assert_eq!(
            parsed(json!({"action": "start", "path": absolute, "display": 0})).unwrap(),
            Request::Start { path: absolute.clone(), display: Some(0) }
        );
        assert_eq!(
            parsed(json!({"action": "start", "display": null})).unwrap(),
            Request::Start { path: "/state/default.mp4".into(), display: None }
        );
        assert_eq!(parsed(json!({"action": "stop"})).unwrap(), Request::Stop);
        assert_eq!(parsed(json!({"action": "status"})).unwrap(), Request::Status);
        for (args, says) in [
            (json!({"action": "start", "path": "a.mp4"}), "absolute"),
            (json!({"action": "start", "display": 16}), "display"),
            (json!({"action": "start", "display": "1"}), "display"),
            (json!({"action": "stop", "path": "/tmp/a.mp4"}), "takes no `path`"),
            (json!({"action": "pause"}), "unknown record action"),
            (json!({}), "needs an action"),
            (json!("start"), "object"),
        ] {
            let f = parsed(args.clone()).unwrap_err();
            assert_eq!(f.code, "invalid_argument", "{args}");
            assert!(f.message.contains(says), "{args}: {}", f.message);
        }
    }
}
