//! Subscription to the phone's accessibility tree.
//!
//! One DTX connection to `…axAuditDaemon.remoteAXService`.
//!
//! What the phone does (iOS 27.0.1, measured 2026-11-10):
//! - `clientNeedsAccessibility:` (true) asks for a dump of the whole screen.
//!   The answer comes in two passes: the first about 1.1 s after the request
//!   on a still screen (up to several seconds while something animates), the
//!   second about 0.5 s later.
//! - The 1.1 s is a quiet period that every new request restarts. Requests
//!   sent every second never get an answer. So: one request, then wait.
//! - After a navigation in Settings the phone sent nothing on its own; a
//!   fresh reading is a new request on the same connection. Some apps do make
//!   the phone send dumps unasked (Calculator on every key, Notes when the
//!   keyboard comes or goes). Those lag behind what triggered them, so one
//!   can arrive after a later request and look like its answer.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use idevice::{
    dvt::{message::AuxValue, remote_server::RemoteServerClient},
    rsd::RsdHandshake,
};
use tokio::sync::{mpsc, watch};

use crate::ax::{
    archive::Ka,
    dump::{Answer, Dump, DumpKind, ObjKey, Payload},
    request,
};
use crate::tcp::handle::AdapterHandle;

pub const SERVICE: &str = "com.apple.accessibility.axAuditDaemon.remoteAXService";
/// The phone never answers a request sooner than this (about 1.1 s typical).
pub const ANSWER_AT_LEAST: Duration = Duration::from_millis(700);
const SUBSCRIBE: &str = "clientNeedsAccessibility:";
const PUSH: &str = "processDataFromRemoteDevice:";
const ASK: &str = "processDataFromHost:";

#[derive(Default)]
pub struct AxState {
    /// Number of first-pass dumps received so far.
    pub seq: u64,
    pub initial: Option<Arc<Dump>>,
    /// When the latest first-pass dump arrived.
    pub initial_at: Option<Instant>,
    /// Arrival times of the most recent first-pass dumps.
    pub arrivals: std::collections::VecDeque<Instant>,
    /// When the last request went out, and whether its answer is still due.
    pub asked_at: Option<Instant>,
    pub answer_due: bool,
    /// When the phone last sent a first-pass dump nobody asked for.
    pub unasked_at: Option<Instant>,
    /// Payloads the parser gave up on.
    pub unparsed: u64,
    /// `seq` of the reading that `full` belongs to.
    pub full_seq: u64,
    /// First pass merged with the second pass (identifiers, hints).
    pub full: Option<Arc<Dump>>,
    /// Dumps that arrived without any element (seen during animations).
    pub empty: u64,
    pub closed: bool,
    /// Answers to `ask`, newest last, until the asker takes them.
    pub answers: std::collections::VecDeque<(Instant, Answer)>,
}

enum Cmd {
    Subscribe,
    Ask(Vec<u8>),
}

/// `IPH_TRACE=1` logs every request and dump with a millisecond clock, to the
/// daemon's log.
pub fn trace(msg: impl FnOnce() -> String) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    static START: OnceLock<Instant> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var("IPH_TRACE").is_ok_and(|v| v == "1")) {
        let t = START.get_or_init(Instant::now).elapsed().as_millis();
        eprintln!("[trace {t:>8}] {}", msg());
    }
}

pub struct AxSub {
    cmd: mpsc::UnboundedSender<Cmd>,
    state: Arc<Mutex<AxState>>,
    changed: watch::Receiver<u64>,
    task: tokio::task::JoinHandle<()>,
    /// When the connection's first request went out.
    pub opened: Instant,
}

impl Drop for AxSub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AxSub {
    pub async fn open(adapter: &mut AdapterHandle, rsd: &RsdHandshake) -> Result<Self> {
        let port = rsd
            .services
            .get(SERVICE)
            .ok_or_else(|| anyhow!("the phone does not offer {SERVICE}"))?
            .port;
        let stream = adapter.connect(port).await?;
        let mut dtx = RemoteServerClient::new(stream);
        subscribe(&mut dtx).await?;
        let opened = Instant::now();

        let state = Arc::new(Mutex::new(AxState {
            asked_at: Some(opened),
            answer_due: true,
            ..Default::default()
        }));
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
        let (changed_tx, changed_rx) = watch::channel(0u64);
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let mut ticks = 0u64;
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => match cmd {
                        Some(Cmd::Subscribe) => {
                            trace(|| "ax request".into());
                            if subscribe(&mut dtx).await.is_err() {
                                break;
                            }
                        }
                        Some(Cmd::Ask(bytes)) => {
                            trace(|| "ax ask".into());
                            let sent = dtx
                                .call_method(0, Some(ASK), Some(vec![AuxValue::Array(bytes)]), false)
                                .await;
                            if sent.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                    msg = dtx.read_message(0) => {
                        let Ok(msg) = msg else { break };
                        let is_push = matches!(&msg.data, Some(plist::Value::String(s)) if s == PUSH);
                        if !is_push {
                            continue;
                        }
                        let Some(AuxValue::Array(bytes)) =
                            msg.aux.and_then(|a| a.values.into_iter().next())
                        else {
                            continue;
                        };
                        let arrived = Instant::now();
                        debug_keep(&bytes, false);
                        let size = bytes.len();
                        // Off the async threads: a payload that takes long to
                        // unfold must not stall the tunnel and the timers.
                        let parsed = tokio::task::spawn_blocking(move || {
                            let r = Payload::parse(&bytes);
                            (r, bytes)
                        })
                        .await;
                        let dump = match parsed {
                            Ok((Ok(Payload::Dump(dump)), _)) => dump,
                            Ok((Ok(Payload::Notification(n)), _)) => {
                                trace(|| format!("ax notification {n} bytes={size}"));
                                continue;
                            }
                            Ok((Ok(Payload::Answer(a)), _)) => {
                                trace(|| format!("ax answer attribute={} bytes={size}", a.attribute));
                                let mut s = task_state.lock().unwrap();
                                s.answers.push_back((arrived, a));
                                while s.answers.len() > 32 {
                                    s.answers.pop_front();
                                }
                                drop(s);
                                ticks += 1;
                                let _ = changed_tx.send(ticks);
                                continue;
                            }
                            Ok((Err(e), bytes)) => {
                                debug_keep(&bytes, true);
                                eprintln!("[{}] accessibility payload of {size} bytes not understood: {e:#}", crate::clock::now_utc());
                                task_state.lock().unwrap().unparsed += 1;
                                ticks += 1;
                                let _ = changed_tx.send(ticks);
                                continue;
                            }
                            Err(_) => continue,
                        };
                        trace(|| format!("ax dump {:?} elements={} bytes={size}", dump.kind, dump.elements.len()));
                        if accept(&task_state, dump, arrived) {
                            ticks += 1;
                            let _ = changed_tx.send(ticks);
                        }
                    }
                }
            }
            task_state.lock().unwrap().closed = true;
            ticks += 1;
            let _ = changed_tx.send(ticks);
        });

        Ok(Self {
            cmd: cmd_tx,
            state,
            changed: changed_rx,
            task,
            opened,
        })
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// Ask the phone for a new dump. Returns when the request was made.
    ///
    /// The phone answers a request about 1.1 s after the *last* one it got:
    /// every new request restarts that wait, so asking again while an answer
    /// is on its way only delays it.
    pub fn request(&self) -> Result<Instant> {
        self.cmd
            .send(Cmd::Subscribe)
            .map_err(|_| anyhow!("accessibility connection closed"))?;
        let now = Instant::now();
        let mut s = self.state.lock().unwrap();
        s.asked_at = Some(now);
        s.answer_due = true;
        Ok(now)
    }

    /// True when the phone sent a dump nobody asked for at or after `since`.
    pub fn unasked_since(&self, since: Instant) -> bool {
        self.state.lock().unwrap().unasked_at.is_some_and(|t| t >= since)
    }

    pub fn unparsed_count(&self) -> u64 {
        self.state.lock().unwrap().unparsed
    }

    /// Wait for a first-pass dump that arrived at or after `earliest`.
    pub async fn wait_arrived(&mut self, earliest: Instant, timeout: Duration) -> Option<(u64, Arc<Dump>)> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let s = self.state.lock().unwrap();
                if s.initial_at.is_some_and(|at| at >= earliest)
                    && let Some(d) = s.initial.clone() {
                        return Some((s.seq, d));
                    }
                if s.closed {
                    return None;
                }
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            match tokio::time::timeout(left, self.changed.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }

    /// Ask for one attribute of one element. The phone answers in tens of
    /// milliseconds and, unlike a dump, does not wait for the screen to be
    /// still. `Some(Ka::Null)` means the element has no such attribute (or is
    /// gone); `None` that no answer came within `timeout`.
    pub async fn ask(&mut self, key: ObjKey, attribute: i64, timeout: Duration) -> Option<Ka> {
        let asked = Instant::now();
        self.cmd.send(Cmd::Ask(request::attribute(key, attribute))).ok()?;
        let deadline = asked + timeout;
        loop {
            {
                let mut s = self.state.lock().unwrap();
                let hit = s.answers.iter().position(|(at, a)| {
                    *at >= asked && a.attribute == attribute && a.key == Some(key)
                });
                if let Some(i) = hit {
                    return s.answers.remove(i).map(|(_, a)| a.value);
                }
                if s.closed {
                    return None;
                }
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            match tokio::time::timeout(left, self.changed.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }

    pub fn seq(&self) -> u64 {
        self.state.lock().unwrap().seq
    }

    pub fn empty_count(&self) -> u64 {
        self.state.lock().unwrap().empty
    }

    /// Latest first-pass dump with its sequence number and age.
    pub fn latest(&self) -> Option<(u64, Arc<Dump>, Duration)> {
        let s = self.state.lock().unwrap();
        Some((s.seq, s.initial.clone()?, s.initial_at?.elapsed()))
    }

    /// When the latest first-pass dump arrived.
    pub fn latest_arrival(&self) -> Option<Instant> {
        self.state.lock().unwrap().initial_at
    }

    /// How many first-pass dumps arrived at or after `since`.
    pub fn initials_since(&self, since: Instant) -> usize {
        self.state.lock().unwrap().arrivals.iter().filter(|t| **t >= since).count()
    }

    /// Latest dump with second-pass attributes, if it belongs to reading `seq`.
    pub fn full(&self, seq: u64) -> Option<Arc<Dump>> {
        let s = self.state.lock().unwrap();
        (s.full_seq == seq).then(|| s.full.clone()).flatten()
    }

    /// Wait for the second pass of reading `seq`.
    pub async fn wait_full(&mut self, seq: u64, timeout: Duration) -> Option<Arc<Dump>> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let s = self.state.lock().unwrap();
                if s.full_seq == seq
                    && let Some(d) = s.full.clone() {
                        return Some(d);
                    }
                // A newer reading replaced this one; its second pass will not come.
                if s.seq > seq || s.closed {
                    return None;
                }
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            match tokio::time::timeout(left, self.changed.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }
}

/// `IPH_DUMP_DIR=<dir>` keeps the last few raw payloads on disk before they
/// are parsed, to study a payload that the parser chokes on. The payloads
/// hold whatever is on the phone's screen: use it on test screens only and
/// delete the files afterwards.
fn debug_keep(bytes: &[u8], failed: bool) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let Ok(dir) = std::env::var("IPH_DUMP_DIR") else {
        return;
    };
    if failed {
        let n = N.load(Ordering::Relaxed);
        let _ = std::fs::write(format!("{dir}/failed-{n}.raw.bin"), bytes);
        return;
    }
    let n = N.fetch_add(1, Ordering::Relaxed);
    let _ = std::fs::write(format!("{dir}/payload-{}.raw.bin", n % 8), bytes);
    trace(|| format!("ax payload #{n} bytes={} kept as payload-{}", bytes.len(), n % 8));
}

async fn subscribe(dtx: &mut RemoteServerClient<Box<dyn idevice::ReadWrite>>) -> Result<()> {
    dtx.call_method(
        0,
        Some(SUBSCRIBE),
        Some(vec![AuxValue::archived_value(true)]),
        false,
    )
    .await
    .map_err(|e| anyhow!("{SUBSCRIBE} failed: {e:?}"))
}

/// Store a dump. Returns false for dumps that carry nothing.
fn accept(state: &Arc<Mutex<AxState>>, dump: Dump, arrived: Instant) -> bool {
    if dump.elements.is_empty() || dump.applications.is_empty() {
        state.lock().unwrap().empty += 1;
        return false;
    }
    match dump.kind {
        DumpKind::Initial => {
            let mut s = state.lock().unwrap();
            s.seq += 1;
            s.initial = Some(Arc::new(dump));
            s.initial_at = Some(arrived);
            // An answer needs about a second. A dump that comes while none is
            // due, or sooner than an answer can, was sent unasked: the app is
            // one that makes the phone do that.
            let answers = s.answer_due
                && s.asked_at.is_some_and(|at| arrived >= at + ANSWER_AT_LEAST);
            if answers {
                s.answer_due = false;
            } else {
                s.unasked_at = Some(arrived);
            }
            if s.arrivals.len() >= 32 {
                s.arrivals.pop_front();
            }
            s.arrivals.push_back(arrived);
            true
        }
        DumpKind::Additional => {
            let (seq, initial) = {
                let s = state.lock().unwrap();
                match s.initial.clone() {
                    Some(i) => (s.seq, i),
                    None => return false,
                }
            };
            // The second pass belongs to the newest first pass only if it
            // describes the same elements.
            if !initial.same_elements(&dump) {
                return false;
            }
            // Merge without holding the lock; readers must never wait on it.
            let mut merged = (*initial).clone();
            merged.absorb(&dump);
            let mut s = state.lock().unwrap();
            if s.seq != seq {
                return false;
            }
            s.full = Some(Arc::new(merged));
            s.full_seq = seq;
            true
        }
        DumpKind::Other(_) => false,
    }
}
