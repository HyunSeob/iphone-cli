//! Touch, keyboard and hardware-button input.
//!
//! The phone drops every synthetic HID event unless a displayservice media
//! stream is active, so an [`InputSession`] first starts a screen-sharing
//! stream (audio + video, the way Xcode's Devices window does), throws the RTP
//! away, and then talks to the universal HID service.
//!
//! The media-session bring-up is adapted from the `coredevice_stream` example
//! tool in jkcoxson/idevice (MIT, see LICENSE.idevice).

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use idevice::{
    ReadWrite, RsdService,
    core_device::{
        CallInfoBlob, DisplayServiceClient, build_screen_audio_offer, build_screen_video_offer,
        build_start_audio_parameters, build_start_video_parameters,
        hid::{
            ButtonState, IndigoHidClient, TOUCHSCREEN_STATE_CONTACT, TOUCHSCREEN_STATE_RELEASE,
            UniversalHidServiceClient,
        },
    },
    rsd::RsdHandshake,
    tcp::handle::{AdapterHandle, UdpSocketHandle},
};

/// What the controller says it supports. Xcode's device window sends 140.
const CLIENT_SUPPORTED_FEATURES: u64 = 140;

/// Time the phone needs to match its HID surfaces to a new media stream.
const WARMUP: Duration = Duration::from_millis(1000);
/// Keep the touch connection quiet for this long after a release; closing or
/// reusing it earlier makes the phone drop some taps. The wait happens before
/// the next input (and before closing), not after each one, so the caller can
/// already ask for the new screen meanwhile.
const SETTLE: Duration = Duration::from_millis(300);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct Drain {
    last_packet_ms: Arc<AtomicU64>,
    packets: Arc<AtomicU64>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drain {
    fn start(sockets: Vec<UdpSocketHandle>) -> Self {
        let last_packet_ms = Arc::new(AtomicU64::new(0));
        let packets = Arc::new(AtomicU64::new(0));
        let tasks = sockets
            .into_iter()
            .map(|s| {
                let last = last_packet_ms.clone();
                let n = packets.clone();
                tokio::spawn(async move {
                    while s.recv().await.is_ok() {
                        last.store(now_ms(), Ordering::Relaxed);
                        n.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        Self {
            last_packet_ms,
            packets,
            tasks,
        }
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

pub struct InputSession {
    display: DisplayServiceClient<Box<dyn ReadWrite>>,
    hid: UniversalHidServiceClient<Box<dyn ReadWrite>>,
    indigo: Option<IndigoHidClient<Box<dyn ReadWrite>>>,
    drain: Drain,
    pub started: Instant,
    last_release: Option<Instant>,
}

/// Normalized HID coordinate (0..=65535) for a point in logical screen points.
pub fn normalize(v: f64, extent: f64) -> u16 {
    if extent <= 0.0 {
        return 0;
    }
    ((v / extent) * 65535.0).round().clamp(0.0, 65535.0) as u16
}

impl InputSession {
    pub async fn open(adapter: &mut AdapterHandle, rsd: &mut RsdHandshake) -> Result<Self> {
        let mut display = DisplayServiceClient::connect_rsd(adapter, rsd)
            .await
            .map_err(|e| anyhow!("display service: {e:?}"))?;

        let audio_udp = adapter.bind_udp(0).await?;
        let video_udp = adapter.bind_udp(0).await?;
        let receiver_ip = adapter.host_ip().to_string();
        let sender_ip = adapter.peer_ip().to_string();

        let call_info = CallInfoBlob {
            call_id: 0,
            client_version: 1,
            device_type: "Mac17,7".into(),
            framework_version: "2205.3.1".into(),
            os_version: "25F71".into(),
            device_name: None,
            audio_device_uid: None,
        };
        let session_id = uuid::Uuid::new_v4();

        let audio_call = uuid::Uuid::new_v4().to_string().to_uppercase();
        let audio_offer = build_screen_audio_offer(&audio_call, &call_info)
            .map_err(|e| anyhow!("audio offer: {e:?}"))?;
        display
            .start_media_stream(build_start_audio_parameters(
                &receiver_ip,
                audio_udp.local_port(),
                &sender_ip,
                50000,
                audio_offer,
                CLIENT_SUPPORTED_FEATURES,
                session_id,
            ))
            .await
            .map_err(|e| anyhow!("start audio stream: {e:?}"))?;

        let video_call = uuid::Uuid::new_v4().to_string().to_uppercase();
        let ssrc = uuid::Uuid::new_v4().as_u128() as u32;
        let video_offer = build_screen_video_offer(&video_call, &call_info, ssrc)
            .map_err(|e| anyhow!("video offer: {e:?}"))?;
        display
            .start_media_stream(build_start_video_parameters(
                &receiver_ip,
                video_udp.local_port(),
                &sender_ip,
                50001,
                video_offer,
                CLIENT_SUPPORTED_FEATURES,
                1,
                session_id,
            ))
            .await
            .map_err(|e| anyhow!("start video stream: {e:?}"))?;

        let drain = Drain::start(vec![audio_udp, video_udp]);
        tokio::time::sleep(WARMUP).await;

        let hid = UniversalHidServiceClient::connect_rsd(adapter, rsd)
            .await
            .map_err(|e| anyhow!("hid service: {e:?}"))?;
        let indigo = IndigoHidClient::connect_rsd(adapter, rsd).await.ok();

        Ok(Self {
            display,
            hid,
            indigo,
            drain,
            started: Instant::now(),
            last_release: None,
        })
    }

    /// Milliseconds since the phone last sent a media packet, if it ever did.
    pub fn media_idle_ms(&self) -> Option<u64> {
        let last = self.drain.last_packet_ms.load(Ordering::Relaxed);
        (last > 0).then(|| now_ms().saturating_sub(last))
    }

    pub fn media_packets(&self) -> u64 {
        self.drain.packets.load(Ordering::Relaxed)
    }

    async fn settle_previous(&mut self) {
        if let Some(t) = self.last_release {
            let since = t.elapsed();
            if since < SETTLE {
                tokio::time::sleep(SETTLE - since).await;
            }
        }
    }

    async fn contact(&mut self, x: u16, y: u16) -> Result<()> {
        self.hid
            .send_touchscreen(TOUCHSCREEN_STATE_CONTACT, x, y, None)
            .await
            .map_err(|e| anyhow!("touch contact: {e:?}"))
    }

    async fn release(&mut self, x: u16, y: u16) -> Result<()> {
        let r = self
            .hid
            .send_touchscreen(TOUCHSCREEN_STATE_RELEASE, x, y, None)
            .await
            .map_err(|e| anyhow!("touch release: {e:?}"));
        crate::device::ax::trace(|| "touch released".into());
        self.last_release = Some(Instant::now());
        r
    }

    /// One finger down at (x, y), held for `hold`, then lifted. Coordinates are
    /// normalized 0..=65535.
    pub async fn tap(&mut self, x: u16, y: u16, hold: Duration) -> Result<()> {
        self.settle_previous().await;
        crate::device::ax::trace(|| format!("tap down {x},{y}"));
        self.contact(x, y).await?;
        tokio::time::sleep(hold).await;
        self.release(x, y).await
    }

    pub async fn double_tap(&mut self, x: u16, y: u16) -> Result<()> {
        self.settle_previous().await;
        for i in 0..2 {
            self.contact(x, y).await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.release(x, y).await?;
            if i == 0 {
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        }
        Ok(())
    }

    /// Drag from (x1, y1) to (x2, y2) over `duration`, then lift. `end_hold`
    /// keeps the finger still before lifting so a list stops without a fling.
    pub async fn drag(
        &mut self,
        from: (u16, u16),
        to: (u16, u16),
        duration: Duration,
        end_hold: Duration,
    ) -> Result<()> {
        self.settle_previous().await;
        let steps = (duration.as_millis() / 16).clamp(4, 240) as u32;
        let start = Instant::now();
        crate::device::ax::trace(|| format!("drag start {from:?} -> {to:?}"));
        let mut sent_any = false;
        let res: Result<()> = async {
            for i in 0..=steps {
                let t = i as f64 / steps as f64;
                let x = (from.0 as f64 + (to.0 as f64 - from.0 as f64) * t).round() as u16;
                let y = (from.1 as f64 + (to.1 as f64 - from.1 as f64) * t).round() as u16;
                self.contact(x, y).await?;
                sent_any = true;
                let due = duration.mul_f64(t);
                let elapsed = start.elapsed();
                if due > elapsed {
                    tokio::time::sleep(due - elapsed).await;
                }
            }
            if !end_hold.is_zero() {
                tokio::time::sleep(end_hold).await;
                self.contact(to.0, to.1).await?;
            }
            Ok(())
        }
        .await;
        if sent_any {
            // Always lift, also after a failed sample.
            let lifted = self.release(to.0, to.1).await;
            res?;
            lifted?;
        } else {
            res?;
        }
        Ok(())
    }

    /// Press and release a hardware button (HID usage page / usage).
    pub async fn button(&mut self, usage_page: u64, usage: u64, hold: Duration) -> Result<()> {
        let indigo = self
            .indigo
            .as_mut()
            .ok_or_else(|| anyhow!("indigo HID service unavailable"))?;
        indigo
            .send_button(usage_page, usage, ButtonState::Down)
            .await
            .map_err(|e| anyhow!("button down: {e:?}"))?;
        tokio::time::sleep(hold).await;
        indigo
            .send_button(usage_page, usage, ButtonState::Up)
            .await
            .map_err(|e| anyhow!("button up: {e:?}"))?;
        self.last_release = Some(Instant::now());
        Ok(())
    }

    /// Press a key (HID keyboard usage) with optional modifier usages held
    /// around it, e.g. `[0xE3]` for Command.
    pub async fn key(&mut self, usage: u64, modifiers: &[u64]) -> Result<()> {
        let indigo = self
            .indigo
            .as_mut()
            .ok_or_else(|| anyhow!("indigo HID service unavailable"))?;
        let mut down: Vec<u64> = Vec::new();
        let res: Result<()> = async {
            for m in modifiers {
                indigo
                    .send_keyboard(*m, ButtonState::Down)
                    .await
                    .map_err(|e| anyhow!("modifier down: {e:?}"))?;
                down.push(*m);
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            indigo
                .send_keyboard(usage, ButtonState::Down)
                .await
                .map_err(|e| anyhow!("key down: {e:?}"))?;
            down.push(usage);
            tokio::time::sleep(Duration::from_millis(40)).await;
            Ok(())
        }
        .await;
        // Release in reverse order whatever happened above.
        let mut release_err = None;
        for k in down.iter().rev() {
            if let Err(e) = indigo.send_keyboard(*k, ButtonState::Up).await {
                release_err = Some(anyhow!("key up: {e:?}"));
            }
        }
        res?;
        if let Some(e) = release_err {
            return Err(e);
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        Ok(())
    }

    pub async fn close(mut self) {
        self.settle_previous().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.display.stop_media_stream()).await;
    }
}
