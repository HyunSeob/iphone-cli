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
    provider::{IdeviceProvider, TcpProvider},
    rsd::RsdHandshake,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};

use crate::paths;
use crate::tcp::{adapter::Adapter, handle::AdapterHandle};

pub const LABEL: &str = "iphone-cli";
const STEP_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceEntry {
    pub udid: String,
    /// "usb" | "wifi" | "unknown"
    pub connection: String,
}

fn conn_name(c: &Connection) -> &'static str {
    match c {
        Connection::Usb => "usb",
        Connection::Network(_) => "wifi",
        Connection::Unknown(_) => "unknown",
    }
}

/// Every (udid, transport) pair usbmuxd knows about. A phone reachable over
/// both USB and Wi-Fi shows up twice.
pub async fn list_raw() -> Result<Vec<UsbmuxdDevice>> {
    let mut mux = UsbmuxdConnection::default()
        .await
        .context("cannot reach usbmuxd")?;
    mux.get_devices().await.context("usbmuxd ListDevices")
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
        return Ok(Route {
            connection: conn_name(&dev.connection_type).to_string(),
            provider: Box::new(dev.to_provider(UsbmuxdAddr::default(), LABEL)),
        });
    }
    // Not listed. Try the last Wi-Fi address with the pairing record usbmuxd
    // keeps for this phone.
    let Some(ip) = recall_ip(udid) else {
        bail!("device {udid} is not connected (usbmuxd does not list it)");
    };
    let mut mux = UsbmuxdConnection::default()
        .await
        .context("cannot reach usbmuxd")?;
    let pairing_file = mux
        .get_pair_record(udid)
        .await
        .map_err(|e| anyhow!("device {udid} is not listed and has no pairing record: {e:?}"))?;
    Ok(Route {
        connection: "wifi".into(),
        provider: Box::new(TcpProvider {
            addr: ip,
            scope_id: None,
            pairing_file,
            label: LABEL.into(),
        }),
    })
}

pub struct Tunnel {
    pub adapter: AdapterHandle,
    pub rsd: RsdHandshake,
    pub connection: String,
    pub open_ms: u128,
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
    match tokio::time::timeout(STEP_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(anyhow!("{what}: {e:?}")),
        Err(_) => Err(anyhow!("{what}: timed out")),
    }
}

fn t0_stamp() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Open the userspace tunnel and read the RSD service list.
pub async fn open_tunnel(udid: &str) -> Result<Tunnel> {
    let t0 = Instant::now();
    let route = route(udid).await?;
    let proxy = step("CoreDeviceProxy", CoreDeviceProxy::connect(&*route.provider)).await?;
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
        connection: route.connection,
        open_ms: t0.elapsed().as_millis(),
    })
}
