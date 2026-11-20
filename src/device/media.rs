//! The phone's screen-sharing streams (`com.apple.coredevice.displayservice`).
//!
//! The device library stops streams only with `stopAll`, which also ends
//! another session's streams: the input session's audio would end a
//! recording's video, and the other way round. The phone also takes a list
//! of streams to stop, each named by the `RemoteSSRC` of its stream
//! configuration (seen on iOS 27.2: stopping a recording's video by it left
//! the audio running and touches working). [`MediaControl`] speaks the
//! service's requests itself so each session stops only its own streams,
//! found in the phone's list by the client session id they were started for.

use std::{
    borrow::Cow,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use idevice::{
    IdeviceError, ReadWrite, RemoteXpcClient, RsdService,
    core_device::{CallInfoBlob, build_screen_video_offer, build_start_video_parameters},
    rsd::RsdHandshake,
    xpc::{Dictionary, XPCObject},
};

use crate::tcp::handle::{AdapterHandle, UdpSocketHandle};

const FEATURE_START: &str = "com.apple.coredevice.feature.startmediastream";
const FEATURE_STOP: &str = "com.apple.coredevice.feature.stopmediastream";
const ACTION_STOP: &str = "com.apple.coredevice.action.mediastreamstop";
const FEATURE_STATUS: &str = "com.apple.coredevice.feature.getmediastreamserverstatus";
/// The CoreDevice version the requests claim, as the device library sends it.
const CORE_DEVICE_VERSION: [u64; 2] = [629, 3];
/// What the controller says it supports. Xcode's device window sends 140.
pub const CLIENT_SUPPORTED_FEATURES: u64 = 140;
/// How long the phone may go on listing streams it was told to stop.
const STOPPING: Duration = Duration::from_millis(1500);
/// Longest wait for one session's streams to stop.
pub(crate) const STOP_WITHIN: Duration = Duration::from_secs(5);

/// One connection to the display service.
struct Link(RemoteXpcClient<Box<dyn ReadWrite>>);

impl RsdService for Link {
    fn rsd_service_name() -> Cow<'static, str> {
        "com.apple.coredevice.displayservice".into()
    }

    async fn from_stream(stream: Box<dyn ReadWrite>) -> Result<Self, IdeviceError> {
        let mut xpc = RemoteXpcClient::new(stream).await?;
        xpc.do_handshake().await?;
        Ok(Self(xpc))
    }
}

pub struct MediaControl {
    adapter: AdapterHandle,
    rsd: RsdHandshake,
    /// Out while a request is on it. A request cut off by a timeout leaves
    /// its answer, or half a message, on the connection, and the next
    /// request would read that as its own; such a connection is never put
    /// back, and the next request makes a new one.
    link: Option<Link>,
}

impl MediaControl {
    pub async fn connect(adapter: &AdapterHandle, rsd: &RsdHandshake) -> Result<Self> {
        let mut media = Self { adapter: adapter.clone(), rsd: rsd.clone(), link: None };
        media.relink().await?;
        Ok(media)
    }

    /// Make a new connection if a request cut off took the last one.
    pub async fn relink(&mut self) -> Result<()> {
        if self.link.is_none() {
            self.link = Some(self.open_link().await?);
        }
        Ok(())
    }

    async fn open_link(&mut self) -> Result<Link> {
        Link::connect_rsd(&mut self.adapter, &mut self.rsd)
            .await
            .map_err(|e| anyhow!("display service: {e:?}"))
    }

    /// One CoreDevice invocation, built the way the device library builds it.
    async fn invoke(&mut self, feature: &str, input: Dictionary, action: Option<&str>) -> Result<plist::Value> {
        let mut version = Dictionary::new();
        version.insert("originalComponentsCount".into(), XPCObject::Int64(CORE_DEVICE_VERSION.len() as i64));
        version.insert("components".into(), XPCObject::Array(CORE_DEVICE_VERSION.map(XPCObject::UInt64).to_vec()));
        version.insert(
            "stringValue".into(),
            XPCObject::String(CORE_DEVICE_VERSION.map(|c| c.to_string()).join(".")),
        );
        let mut req = Dictionary::new();
        req.insert("CoreDevice.CoreDeviceDDIProtocolVersion".into(), XPCObject::Int64(if action.is_some() { 2 } else { 0 }));
        req.insert("CoreDevice.action".into(), XPCObject::Dictionary(Dictionary::new()));
        req.insert("CoreDevice.coreDeviceVersion".into(), XPCObject::Dictionary(version));
        req.insert("CoreDevice.deviceIdentifier".into(), XPCObject::String(uuid::Uuid::new_v4().to_string()));
        req.insert("CoreDevice.featureIdentifier".into(), XPCObject::String(feature.into()));
        req.insert("CoreDevice.input".into(), XPCObject::Dictionary(input));
        req.insert("CoreDevice.invocationIdentifier".into(), XPCObject::String(uuid::Uuid::new_v4().to_string()));
        if let Some(action) = action {
            req.insert("CoreDevice.actionIdentifier".into(), XPCObject::String(action.into()));
        }
        let mut link = match self.link.take() {
            Some(link) => link,
            None => self.open_link().await?,
        };
        link.0.send_object(req, true).await.map_err(|e| anyhow!("{e:?}"))?;
        let res = link.0.recv().await.map_err(|e| anyhow!("{e:?}"))?;
        self.link = Some(link);
        let mut res = res.into_dictionary().ok_or_else(|| anyhow!("the answer is not a dictionary"))?;
        match res.remove("CoreDevice.output") {
            Some(out) => Ok(out),
            None => Err(anyhow!("{:?}", res.get("CoreDevice.error").unwrap_or(&plist::Value::Dictionary(res.clone())))),
        }
    }

    /// Start one stream.
    pub async fn start(&mut self, params: Dictionary) -> Result<()> {
        let started = self.invoke(FEATURE_START, params, None).await?;
        crate::device::ax::trace(|| format!("media: started a stream, RemoteSSRC {:?}", remote_ssrc(&started)));
        Ok(())
    }

    pub async fn status(&mut self) -> Result<plist::Value> {
        self.invoke(FEATURE_STATUS, Dictionary::new(), None).await
    }

    /// Stop the streams the phone runs for the client session `id`, and no
    /// other session's: by name, and with `stopAll` only when the phone runs
    /// nothing else (see [`next_stop`]).
    pub async fn stop_session(&mut self, id: &str) -> Result<()> {
        let mut asked = Asked::Nothing;
        // Until when the phone may still list streams it was asked to stop.
        let mut settling = Instant::now();
        loop {
            let status = self.status().await?;
            match next_stop(&status, id, asked) {
                Stop::Done => return Ok(()),
                _ if Instant::now() < settling => tokio::time::sleep(Duration::from_millis(100)).await,
                Stop::Named(ssrcs) => {
                    let mut input = Dictionary::new();
                    input.insert("stopAll".into(), XPCObject::Bool(false));
                    // The phone decodes each as a UInt32 and refuses an int64.
                    input.insert(
                        "identifiers".into(),
                        XPCObject::Array(ssrcs.iter().map(|s| XPCObject::UInt64(u64::from(*s))).collect()),
                    );
                    let stopped = self.invoke(FEATURE_STOP, input, Some(ACTION_STOP)).await;
                    crate::device::ax::trace(|| format!("media: stop {ssrcs:?} of session {id}: {:?}", stopped.as_ref().map(|_| ())));
                    asked = Asked::ByName;
                    settling = Instant::now() + if stopped.is_ok() { STOPPING } else { Duration::ZERO };
                }
                Stop::All => {
                    crate::device::ax::trace(|| format!("media: the phone kept the streams of session {id}; nothing else runs, stopping all"));
                    let mut all = Dictionary::new();
                    all.insert("stopAll".into(), XPCObject::Bool(true));
                    self.invoke(FEATURE_STOP, all, Some(ACTION_STOP)).await?;
                    asked = Asked::All;
                    settling = Instant::now() + STOPPING;
                }
                Stop::Kept { others } => {
                    return Err(anyhow!("the phone kept the streams of session {id} ({others} stream(s) of other sessions run)"));
                }
            }
        }
    }

    /// Stop the streams of session `id` within a few seconds, and strike
    /// it from `marker` once the phone runs none of them.
    pub async fn stop_own(&mut self, id: &str, marker: &Path) -> Result<()> {
        let stopped = match tokio::time::timeout(STOP_WITHIN, self.stop_session(id)).await {
            Ok(stopped) => stopped,
            Err(_) => Err(anyhow!("no answer within {} s", STOP_WITHIN.as_secs())),
        };
        if stopped.is_ok() {
            unmark(marker, id);
        }
        stopped
    }
}

/// The client sessions whose streams may run on the phone, one per line of
/// `marker`. Kept across daemons; a session is struck only once the phone
/// is seen to run none of its streams.
pub fn marked(marker: &Path) -> Vec<String> {
    std::fs::read_to_string(marker)
        .map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// Add session `id` to `marker`, before its first stream is asked for.
pub fn mark(marker: &Path, id: &str) {
    let mut ids = marked(marker);
    if !ids.iter().any(|m| m == id) {
        ids.push(id.to_string());
    }
    let _ = std::fs::write(marker, ids.join("\n") + "\n");
}

/// The next session of `marker` whose streams no `live` session owns. Left
/// running by an iph that was killed, a start cut off, or a stop that
/// failed, they run for as long as their RTCP timeout, an hour (seen
/// 2026-11-19 after `kill -9`), with the phone encoding into nowhere. It
/// moves to the back of `marker`, so one whose stop hangs keeps no other
/// waiting.
pub fn next_leftover(marker: &Path, live: &[String]) -> Option<String> {
    let id = marked(marker).into_iter().find(|id| !live.contains(id))?;
    unmark(marker, &id);
    mark(marker, &id);
    Some(id)
}

fn unmark(marker: &Path, id: &str) {
    let ids: Vec<String> = marked(marker).into_iter().filter(|m| m != id).collect();
    let _ = if ids.is_empty() { std::fs::remove_file(marker) } else { std::fs::write(marker, ids.join("\n") + "\n") };
}

/// What the phone was last asked to stop of one client session.
#[derive(Clone, Copy, PartialEq)]
enum Asked {
    Nothing,
    ByName,
    All,
}

/// The next step in stopping the streams of one client session, from what
/// the phone lists.
#[derive(Debug, PartialEq)]
enum Stop {
    /// The phone runs none of them.
    Done,
    /// Stop these by name.
    Named(Vec<u32>),
    /// They were asked by name (or have none) and are still listed, and
    /// nothing else runs: `stopAll` ends only them.
    All,
    /// They are still listed after `stopAll`, or `others` streams of other
    /// sessions run, which `stopAll` would end too: the recorder's video,
    /// the input session's audio, or another client's screen view.
    Kept { others: usize },
}

fn next_stop(status: &plist::Value, id: &str, asked: Asked) -> Stop {
    // A status without the list has a shape not seen: no stream can be told
    // apart, so `stopAll` is sent and trusted.
    if status.as_dictionary().and_then(|d| d.get("sessions")).and_then(|v| v.as_array()).is_none() {
        return if asked == Asked::All { Stop::Done } else { Stop::All };
    }
    let ours = streams_of(status, id);
    if ours.is_empty() {
        return Stop::Done;
    }
    if asked == Asked::Nothing && let Some(ssrcs) = ours.iter().copied().collect::<Option<Vec<u32>>>() {
        return Stop::Named(ssrcs);
    }
    match sessions(status).count() - ours.len() {
        0 if asked != Asked::All => Stop::All,
        others => Stop::Kept { others },
    }
}

/// What the controller says it is, as Xcode's device window does.
pub fn call_info() -> CallInfoBlob {
    CallInfoBlob {
        call_id: 0,
        client_version: 1,
        device_type: "Mac17,7".into(),
        framework_version: "2205.3.1".into(),
        os_version: "25F71".into(),
        device_name: None,
        audio_device_uid: None,
    }
}

/// Start the video of `display` for the client session `session_id`, sent
/// to a new socket.
pub async fn start_video(
    media: &mut MediaControl,
    adapter: &mut AdapterHandle,
    display: i64,
    session_id: uuid::Uuid,
) -> Result<UdpSocketHandle> {
    let udp = adapter.bind_udp(0).await?;
    let call = uuid::Uuid::new_v4().to_string().to_uppercase();
    let ssrc = uuid::Uuid::new_v4().as_u128() as u32;
    let offer = build_screen_video_offer(&call, &call_info(), ssrc).map_err(|e| anyhow!("video offer: {e:?}"))?;
    media
        .start(build_start_video_parameters(
            &adapter.host_ip().to_string(),
            udp.local_port(),
            &adapter.peer_ip().to_string(),
            50001,
            offer,
            CLIENT_SUPPORTED_FEATURES,
            display,
            session_id,
        ))
        .await
        .map_err(|e| anyhow!("start video stream: {e}"))?;
    Ok(udp)
}

/// The `RemoteSSRC` of a stream, the name the phone stops it by.
fn remote_ssrc(stream: &plist::Value) -> Option<u32> {
    let config = stream.as_dictionary()?.get("connection")?.as_dictionary()?.get("streamConfig")?.as_dictionary()?;
    u32::try_from(config.get("RemoteSSRC")?.as_unsigned_integer()?).ok()
}

fn sessions(status: &plist::Value) -> impl Iterator<Item = &plist::Value> {
    status.as_dictionary().and_then(|d| d.get("sessions")).and_then(|v| v.as_array()).into_iter().flatten()
}

/// The names of the streams the media server runs for the client session `id`.
pub fn streams_of(status: &plist::Value, id: &str) -> Vec<Option<u32>> {
    let client = |s: &plist::Value| -> Option<String> {
        let options = s.as_dictionary()?.get("connection")?.as_dictionary()?.get("options")?.as_dictionary()?;
        let uuid = options.get("avcMediaStreamOptionClientSessionID")?.as_dictionary()?.get("uuid")?;
        uuid.as_string().map(str::to_lowercase)
    };
    sessions(status).filter(|s| client(s).as_deref() == Some(&id.to_lowercase())).map(remote_ssrc).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(pairs: Vec<(&str, plist::Value)>) -> plist::Value {
        plist::Value::Dictionary(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// A stream as the media server lists it (seen on iOS 27.2), started by
    /// the client session `id`.
    fn stream(id: &str, ssrc: u64) -> plist::Value {
        let uuid = dict(vec![("uuid", plist::Value::String(id.into()))]);
        let options = dict(vec![("avcMediaStreamOptionClientSessionID", uuid)]);
        let config = dict(vec![("RemoteSSRC", plist::Value::Integer(ssrc.into())), ("LocalSSRC", plist::Value::Integer(7.into()))]);
        dict(vec![("connection", dict(vec![("options", options), ("streamConfig", config)]))])
    }

    fn status(sessions: Vec<plist::Value>) -> plist::Value {
        dict(vec![("sessions", plist::Value::Array(sessions))])
    }

    #[test]
    fn only_the_streams_of_one_client_session_are_named_for_stopping() {
        let ours = "f608fea1-4ad9-4b5e-be7b-de38a2fbb0df";
        let theirs = stream("0b7e9d3c-1111-4c2a-9e43-5d1f0a6b7c88", 11);
        assert!(streams_of(&status(vec![theirs.clone()]), ours).is_empty(), "another client's screen sharing is left running");
        let all = status(vec![stream(ours, 2_292_078_377), theirs, stream(&ours.to_uppercase(), 5)]);
        assert_eq!(streams_of(&all, ours), vec![Some(2_292_078_377), Some(5)]);
        assert!(streams_of(&status(vec![]), ours).is_empty());
        assert!(streams_of(&plist::Value::Boolean(false), ours).is_empty(), "an answer of another shape stops nothing");
    }

    #[test]
    fn a_stream_is_named_by_its_remote_ssrc() {
        assert_eq!(remote_ssrc(&stream("a", 2_292_078_377)), Some(2_292_078_377));
        assert_eq!(remote_ssrc(&dict(vec![("connection", dict(vec![]))])), None);
        assert_eq!(remote_ssrc(&stream("a", u64::from(u32::MAX) + 1)), None, "not a UInt32, not a name the phone takes");
    }

    #[test]
    fn all_streams_are_stopped_only_when_nothing_but_the_sessions_own_runs() {
        let ours = "f608fea1-4ad9-4b5e-be7b-de38a2fbb0df";
        let recorder = stream("0b7e9d3c-1111-4c2a-9e43-5d1f0a6b7c88", 11);
        let mut nameless = stream(ours, 0);
        nameless.as_dictionary_mut().unwrap().insert("connection".into(), dict(vec![(
            "options",
            dict(vec![("avcMediaStreamOptionClientSessionID", dict(vec![("uuid", plist::Value::String(ours.into()))]))]),
        )]));

        assert_eq!(next_stop(&status(vec![recorder.clone()]), ours, Asked::Nothing), Stop::Done, "nothing of ours runs");
        assert_eq!(next_stop(&status(vec![stream(ours, 5), recorder.clone()]), ours, Asked::Nothing), Stop::Named(vec![5]));
        // Asked by name and still listed: the phone may not have caught up.
        assert_eq!(next_stop(&status(vec![stream(ours, 5)]), ours, Asked::ByName), Stop::All);
        assert_eq!(
            next_stop(&status(vec![stream(ours, 5), recorder.clone()]), ours, Asked::ByName),
            Stop::Kept { others: 1 },
            "never stopAll while the recorder's video runs"
        );
        assert_eq!(next_stop(&status(vec![nameless.clone()]), ours, Asked::Nothing), Stop::All, "a stream without a name");
        assert_eq!(
            next_stop(&status(vec![nameless.clone()]), ours, Asked::All),
            Stop::Kept { others: 0 },
            "still listed after stopAll: an error, not a second stopAll"
        );
        assert_eq!(next_stop(&status(vec![]), ours, Asked::All), Stop::Done, "confirmed by the phone's list");
        let unlisted = dict(vec![]);
        assert_eq!(next_stop(&unlisted, ours, Asked::Nothing), Stop::All, "a status without the list");
        assert_eq!(next_stop(&unlisted, ours, Asked::All), Stop::Done, "stopAll trusted without the list");
        assert_eq!(next_stop(&status(vec![nameless, recorder]), ours, Asked::Nothing), Stop::Kept { others: 1 });
    }

    #[test]
    fn a_marker_keeps_every_session_until_it_is_struck() {
        let dir = std::env::temp_dir().join(format!("iph-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("udid.media");
        assert!(marked(&marker).is_empty());
        mark(&marker, "a");
        mark(&marker, "b");
        mark(&marker, "a");
        assert_eq!(marked(&marker), vec!["a", "b"], "a session whose stop was not confirmed stays named");
        unmark(&marker, "a");
        assert_eq!(marked(&marker), vec!["b"]);
        unmark(&marker, "b");
        assert!(!marker.exists());
        std::fs::write(&marker, "c").unwrap();
        assert_eq!(marked(&marker), vec!["c"], "a marker an earlier build wrote, without a line end");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn leftovers_skip_live_sessions_and_take_turns() {
        let dir = std::env::temp_dir().join(format!("iph-leftover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("udid.media");
        for id in ["input", "stuck", "old"] {
            mark(&marker, id);
        }
        let live = vec!["input".to_string()];
        assert_eq!(next_leftover(&marker, &live).as_deref(), Some("stuck"));
        assert_eq!(next_leftover(&marker, &live).as_deref(), Some("old"), "one whose stop hung keeps no other waiting");
        assert_eq!(next_leftover(&marker, &live).as_deref(), Some("stuck"));
        assert_eq!(marked(&marker).len(), 3, "trying a session does not strike it");
        unmark(&marker, "stuck");
        unmark(&marker, "old");
        assert_eq!(next_leftover(&marker, &live), None, "the live session's streams are its own to stop");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
