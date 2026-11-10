//! Subscription to the phone's accessibility tree.
//!
//! One DTX connection to `…axAuditDaemon.remoteAXService`. Each
//! `clientNeedsAccessibility:` (true) makes the phone send one dump of the
//! whole screen, in two passes: the first about 1.1 s after the request on a
//! still screen (later while something animates), the second about 0.5 s
//! after that. The phone does not send dumps on its own when the screen
//! changes, so every fresh reading is a new request on the same connection.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use idevice::{
    dvt::{message::AuxValue, remote_server::RemoteServerClient},
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
};
use tokio::sync::{mpsc, watch};

use crate::ax::dump::{Dump, DumpKind};

pub const SERVICE: &str = "com.apple.accessibility.axAuditDaemon.remoteAXService";
const SUBSCRIBE: &str = "clientNeedsAccessibility:";
const PUSH: &str = "processDataFromRemoteDevice:";

#[derive(Default)]
pub struct AxState {
    /// Number of first-pass dumps received so far.
    pub seq: u64,
    pub initial: Option<Arc<Dump>>,
    pub initial_at: Option<Instant>,
    /// `seq` of the reading that `full` belongs to.
    pub full_seq: u64,
    /// First pass merged with the second pass (identifiers, hints).
    pub full: Option<Arc<Dump>>,
    /// Dumps that arrived without any element (seen during animations).
    pub empty: u64,
    pub closed: bool,
}

enum Cmd {
    Subscribe,
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

        let state = Arc::new(Mutex::new(AxState::default()));
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
                        None => break,
                    },
                    msg = dtx.read_message(0) => {
                        let Ok(msg) = msg else { break };
                        let is_push = matches!(&msg.data, Some(plist::Value::String(s)) if s == PUSH);
                        if !is_push {
                            continue;
                        }
                        let Some(AuxValue::Array(bytes)) =
                            msg.aux.as_ref().and_then(|a| a.values.first())
                        else {
                            continue;
                        };
                        let Ok(dump) = Dump::parse(bytes) else { continue };
                        trace(|| format!("ax dump {:?} elements={} bytes={}", dump.kind, dump.elements.len(), bytes.len()));
                        if accept(&task_state, dump) {
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
        })
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// Ask the phone for a new dump.
    pub fn request(&self) -> Result<()> {
        self.cmd
            .send(Cmd::Subscribe)
            .map_err(|_| anyhow!("accessibility connection closed"))
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

    /// Latest dump with second-pass attributes, if it belongs to reading `seq`.
    pub fn full(&self, seq: u64) -> Option<Arc<Dump>> {
        let s = self.state.lock().unwrap();
        (s.full_seq == seq).then(|| s.full.clone()).flatten()
    }

    /// Wait for a first-pass dump newer than `after`.
    pub async fn wait_newer(&mut self, after: u64, timeout: Duration) -> Option<(u64, Arc<Dump>)> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let s = self.state.lock().unwrap();
                if s.seq > after {
                    if let Some(d) = s.initial.clone() {
                        return Some((s.seq, d));
                    }
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

    /// Wait for the second pass of reading `seq`.
    pub async fn wait_full(&mut self, seq: u64, timeout: Duration) -> Option<Arc<Dump>> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let s = self.state.lock().unwrap();
                if s.full_seq == seq {
                    if let Some(d) = s.full.clone() {
                        return Some(d);
                    }
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
fn accept(state: &Arc<Mutex<AxState>>, dump: Dump) -> bool {
    let mut s = state.lock().unwrap();
    if dump.elements.is_empty() || dump.applications.is_empty() {
        s.empty += 1;
        return false;
    }
    match dump.kind {
        DumpKind::Initial => {
            s.seq += 1;
            s.initial = Some(Arc::new(dump));
            s.initial_at = Some(Instant::now());
            true
        }
        DumpKind::Additional => {
            let Some(initial) = s.initial.clone() else {
                return false;
            };
            // The second pass belongs to the newest first pass only if it
            // describes the same elements.
            if !initial.same_elements(&dump) {
                return false;
            }
            let mut merged = (*initial).clone();
            merged.absorb(&dump);
            s.full = Some(Arc::new(merged));
            s.full_seq = s.seq;
            true
        }
        DumpKind::Other(_) => false,
    }
}
