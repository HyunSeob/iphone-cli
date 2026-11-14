//! Device discovery and the rootless userspace tunnel.
//!
//! lockdown (via usbmuxd, USB or Wi-Fi) → CoreDeviceProxy → CDTunnel →
//! software TCP stack → RSD. Nothing here needs root or a utun interface, and
//! nothing depends on Xcode's own CoreDevice tunnel.

use std::{net::IpAddr, time::Duration, time::Instant};

use anyhow::{Context, Result, anyhow, bail};
use idevice::{
    IdeviceService,
    core_device_proxy::CoreDeviceProxy,
    heartbeat::HeartbeatClient,
    provider::{IdeviceProvider, TcpProvider},
    rsd::RsdHandshake,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};

use crate::{events, paths};
use serde_json::json;
use crate::tcp::{adapter::Adapter, handle::AdapterHandle};

pub const LABEL: &str = "iphone-cli";
const STEP_TIMEOUT: Duration = Duration::from_secs(12);
/// How long the first Marco may take, and the slack on later ones.
const HEARTBEAT_FIRST_SECS: u64 = 10;
const HEARTBEAT_GRACE_SECS: u64 = 5;

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceEntry {
    pub udid: String,
    /// "usb" | "wifi" | "unknown"
    pub connection: String,
}

pub fn conn_name(c: &Connection) -> &'static str {
    match c {
        Connection::Usb => "usb",
        Connection::Network(_) => "wifi",
        Connection::Unknown(_) => "unknown",
    }
}

/// Every (udid, transport) pair usbmuxd knows about. A phone reachable over
/// both USB and Wi-Fi shows up twice.
pub async fn list_raw() -> Result<Vec<UsbmuxdDevice>> {
    let mut mux = mux_connect().await?;
    let devices = events::step("usbmuxd.ListDevices", json!({}), STEP_TIMEOUT, mux.get_devices()).await?;
    events::record("discovery", "ok", 0, json!({ "devices": describe_raw(&devices) }));
    Ok(devices)
}

pub fn mux_addr() -> Result<UsbmuxdAddr> {
    UsbmuxdAddr::from_env_var().context("invalid USBMUXD_SOCKET_ADDRESS")
}

pub async fn mux_connect() -> Result<UsbmuxdConnection> {
    let addr = mux_addr()?;
    events::step("usbmuxd.connect", json!({ "address": format!("{addr:?}") }), STEP_TIMEOUT, addr.connect(0)).await
}

pub fn describe_raw(devices: &[UsbmuxdDevice]) -> Vec<serde_json::Value> {
    devices.iter().map(|d| json!({
        "udid": d.udid, "device_id": d.device_id, "connection": conn_name(&d.connection_type),
        "address": match &d.connection_type { Connection::Network(ip) => Some(ip.to_string()), _ => None },
    })).collect()
}

/// Connected devices, one entry per phone, USB preferred.
pub async fn list() -> Result<Vec<DeviceEntry>> {
    let mut out: Vec<DeviceEntry> = Vec::new();
    for d in list_raw().await? {
        let name = conn_name(&d.connection_type);
        match out.iter_mut().find(|e| e.udid == d.udid) {
            Some(existing) => {
                if name == "usb" {
                    existing.connection = "usb".into();
                }
            }
            None => out.push(DeviceEntry {
                udid: d.udid,
                connection: name.to_string(),
            }),
        }
    }
    Ok(out)
}

fn ip_cache(udid: &str) -> std::path::PathBuf {
    paths::state_dir().join(format!("{udid}.addr"))
}

/// Remember the phone's Wi-Fi address. usbmuxd forgets Wi-Fi devices now and
/// then while the phone is still reachable; the address gets us back in.
fn remember_ip(udid: &str, ip: &IpAddr) {
    if paths::ensure_private_dir(&paths::state_dir()).is_ok() {
        let _ = std::fs::write(ip_cache(udid), ip.to_string());
    }
}

fn recall_ip(udid: &str) -> Option<IpAddr> {
    std::fs::read_to_string(ip_cache(udid)).ok()?.trim().parse().ok()
}

/// What lockdown says about the passcode lock (`PasswordProtected`: true
/// while the phone is locked). `None` when lockdown did not say. Read-only.
pub async fn passcode_locked(provider: &dyn IdeviceProvider) -> Option<bool> {
    use idevice::lockdown::LockdownClient;
    let wait = |secs| Duration::from_secs(secs);
    let mut lockdown = tokio::time::timeout(wait(8), LockdownClient::connect(provider)).await.ok()?.ok()?;
    if let Ok(Ok(pairing)) = tokio::time::timeout(wait(8), provider.get_pairing_file()).await {
        let _ = tokio::time::timeout(wait(8), lockdown.start_session(&pairing)).await;
    }
    tokio::time::timeout(wait(8), lockdown.get_value(Some("PasswordProtected"), None))
        .await
        .ok()?
        .ok()?
        .as_boolean()
}

pub struct Route {
    pub provider: Box<dyn IdeviceProvider>,
    /// "usb" | "wifi"
    pub connection: String,
    /// Only on the cached-address route, which no muxer sits behind.
    pub heartbeat: Option<Heartbeat>,
}

/// Answers the phone's heartbeat for as long as it lives. Over Wi-Fi the phone
/// closes the connections to its services (CoreDeviceProxy, AMFI, the image
/// mounter) unless some client answers its Marco; usbmuxd and netmuxd do that
/// for the phones they list, so only a route iph builds itself needs one.
pub struct Heartbeat(tokio::task::JoinHandle<()>);

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Heartbeat {
    /// Connect, answer the first Marco so the phone sees a client before any
    /// service is asked for, then keep answering in the background.
    async fn start(provider: &dyn IdeviceProvider) -> Result<Self> {
        let mut client = step("Heartbeat", HeartbeatClient::connect(provider)).await?;
        let interval = step("Heartbeat Marco", client.get_marco(HEARTBEAT_FIRST_SECS)).await?;
        step("Heartbeat Polo", client.send_polo()).await?;
        Ok(Self::keep(client, interval))
    }

    fn keep(mut client: HeartbeatClient, mut interval: u64) -> Self {
        Self(tokio::spawn(async move {
            // The phone sends a Marco every `interval` seconds; wait a little longer.
            while let Ok(next) = client.get_marco(interval + HEARTBEAT_GRACE_SECS).await {
                interval = next;
                if client.send_polo().await.is_err() {
                    break;
                }
            }
        }))
    }
}

/// How to reach `udid`: USB when plugged in, otherwise Wi-Fi.
pub async fn route(udid: &str) -> Result<Route> {
    let mut mine: Vec<UsbmuxdDevice> = list_raw()
        .await?
        .into_iter()
        .filter(|d| d.udid == udid)
        .collect();
    // The cable first, unless `IPH_PREFER=wifi` asks for the other way round
    // (to compare the two with the cable left in).
    let wifi_first = std::env::var("IPH_PREFER").is_ok_and(|v| v.eq_ignore_ascii_case("wifi"));
    mine.sort_by_key(|d| match d.connection_type {
        Connection::Usb => u8::from(wifi_first),
        Connection::Network(_) => u8::from(!wifi_first),
        Connection::Unknown(_) => 2,
    });
    if let Some(dev) = mine.into_iter().next() {
        if let Connection::Network(ip) = &dev.connection_type {
            remember_ip(udid, ip);
        }
        events::record("route", "ok", 0, json!({ "udid": udid, "source": "usbmuxd", "device": describe_raw(std::slice::from_ref(&dev)) }));
        return Ok(Route {
            connection: conn_name(&dev.connection_type).to_string(),
            provider: Box::new(dev.to_provider(mux_addr()?, LABEL)),
            heartbeat: None,
        });
    }
    // Not listed. Try the last Wi-Fi address with the pairing record usbmuxd
    // keeps for this phone.
    let Some(ip) = recall_ip(udid) else {
        events::record("route", "error", 0, json!({ "udid": udid, "error": "not listed by usbmuxd; no cached Wi-Fi address" }));
        bail!("device {udid} is not connected (usbmuxd does not list it)");
    };
    events::record("route", "fallback", 0, json!({ "udid": udid, "address": ip, "source": "cached_wifi_address" }));
    let mut mux = mux_connect().await?;
    let pairing_file = mux
        .get_pair_record(udid)
        .await
        .map_err(|e| anyhow!("device {udid} is not listed and has no pairing record: {e:?}"))?;
    let provider = TcpProvider {
        addr: ip,
        scope_id: None,
        pairing_file,
        label: LABEL.into(),
    };
    // Without it the phone refuses its services over Wi-Fi (`tunnel_failed`,
    // TLS closed without close_notify). A failure is already recorded by `step`;
    // lockdown alone still works without a heartbeat.
    let heartbeat = Heartbeat::start(&provider).await.ok();
    Ok(Route {
        connection: "wifi".into(),
        provider: Box::new(provider),
        heartbeat,
    })
}

pub struct Tunnel {
    pub adapter: AdapterHandle,
    pub rsd: RsdHandshake,
    pub connection: String,
    pub open_ms: u128,
    /// Kept so the phone goes on answering the tunnel while it is open.
    pub heartbeat: Option<Heartbeat>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // Stop the background packet task; clones of the handle die with it.
        let mut adapter = self.adapter.clone();
        tokio::spawn(async move {
            let _ = adapter.close().await;
        });
    }
}

async fn step<T, E: std::fmt::Debug>(
    what: &str,
    fut: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    events::step(what, json!({}), STEP_TIMEOUT, fut).await
}

fn t0_stamp() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Open the userspace tunnel and read the RSD service list.
pub async fn open_tunnel(udid: &str) -> Result<Tunnel> {
    let mut route = route(udid).await?;
    let mut tunnel = open_tunnel_with_provider(&*route.provider, &route.connection).await?;
    tunnel.heartbeat = route.heartbeat.take();
    Ok(tunnel)
}

/// Test each transport independently, including Wi-Fi while USB is present.
pub async fn open_tunnel_with_provider(provider: &dyn IdeviceProvider, connection: &str) -> Result<Tunnel> {
    let t0 = Instant::now();
    let proxy = step("CoreDeviceProxy", CoreDeviceProxy::connect(provider)).await?;
    let info = proxy.tunnel_info().clone();
    let rsd_port = info.server_rsd_port;
    let ours: IpAddr = info.client_address.parse().context("tunnel client address")?;
    let theirs: IpAddr = info.server_address.parse().context("tunnel server address")?;
    let mut adapter = Adapter::new(Box::new(proxy.into_tunnel().into_inner()), ours, theirs);
    // Room for the IPv6 and TCP headers.
    adapter.set_mss((info.mtu as usize).saturating_sub(60));
    // `IPH_PCAP=<folder>`: every packet of this tunnel, for Wireshark. Holds
    // whatever the phone sends, so only for a short look, then delete it.
    if let Ok(dir) = std::env::var("IPH_PCAP") {
        let path = std::path::Path::new(&dir).join(format!("tunnel-{}.pcap", t0_stamp()));
        if let Err(e) = adapter.pcap(&path).await {
            tracing::warn!("pcap {}: {e}", path.display());
        }
    }
    let mut adapter = adapter.to_async_handle();
    let stream = step("RSD connect", adapter.connect(rsd_port)).await?;
    let rsd = step("RSD handshake", RsdHandshake::new(stream)).await?;
    Ok(Tunnel {
        adapter,
        rsd,
        connection: connection.into(),
        open_ms: t0.elapsed().as_millis(),
        heartbeat: None,
    })
}

#[cfg(test)]
mod tests {
    use idevice::Idevice;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    /// One length-prefixed plist, as lockdown services speak.
    async fn send(phone: &mut (impl AsyncWriteExt + Unpin), dict: plist::Dictionary) {
        let mut body = Vec::new();
        plist::Value::Dictionary(dict).to_writer_xml(&mut body).unwrap();
        phone.write_all(&(body.len() as u32).to_be_bytes()).await.unwrap();
        phone.write_all(&body).await.unwrap();
    }

    async fn recv(phone: &mut (impl AsyncReadExt + Unpin)) -> plist::Dictionary {
        let mut len = [0u8; 4];
        phone.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        phone.read_exact(&mut body).await.unwrap();
        plist::Value::from_reader(std::io::Cursor::new(body)).unwrap().into_dictionary().unwrap()
    }

    #[tokio::test]
    async fn heartbeat_answers_every_marco_with_a_polo_until_dropped() {
        let (host, mut phone) = duplex(4096);
        let client = HeartbeatClient::new(Idevice::new(Box::new(host), "test"));
        let heartbeat = Heartbeat::keep(client, 1);
        for _ in 0..2 {
            let marco = plist::Dictionary::from_iter([("Interval".to_string(), plist::Value::from(1u64))]);
            send(&mut phone, marco).await;
            let polo = recv(&mut phone).await;
            assert_eq!(polo.get("Command").and_then(|v| v.as_string()), Some("Polo"));
        }
        drop(heartbeat);
        // Dropping stops the task, which closes the host end of the pipe.
        let mut rest = Vec::new();
        assert_eq!(phone.read_to_end(&mut rest).await.unwrap(), 0);
    }
}
