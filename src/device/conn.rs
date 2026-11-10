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
    tcp::handle::AdapterHandle,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};

use crate::paths;

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
    mine.sort_by_key(|d| match d.connection_type {
        Connection::Usb => 0,
        Connection::Network(_) => 1,
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

/// Open the userspace tunnel and read the RSD service list.
pub async fn open_tunnel(udid: &str) -> Result<Tunnel> {
    let t0 = Instant::now();
    let route = route(udid).await?;
    let proxy = step("CoreDeviceProxy", CoreDeviceProxy::connect(&*route.provider)).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let adapter = proxy
        .create_software_tunnel()
        .map_err(|e| anyhow!("software tunnel: {e:?}"))?;
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
