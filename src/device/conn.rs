//! Device discovery and the rootless userspace tunnel.
//!
//! lockdown (USB via usbmuxd, Wi-Fi via direct TCP) → CoreDeviceProxy → CDTunnel →
//! software TCP stack → RSD. Nothing here needs root or a utun interface, and
//! nothing depends on Xcode's own CoreDevice tunnel.

use std::{future::Future, net::IpAddr, time::Duration, time::Instant};

use anyhow::{Context, Result, bail};
use idevice::{
    IdeviceService,
    core_device_proxy::CoreDeviceProxy,
    heartbeat::HeartbeatClient,
    provider::{IdeviceProvider, TcpProvider},
    rsd::RsdHandshake,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};

use super::{
    address::{self, WifiAddress},
    discovery::{self, Discovery},
    identity::{self, Identity},
};
use crate::tcp::{adapter::Adapter, handle::AdapterHandle};
use crate::{events, paths};
use serde_json::json;

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
    Ok(discovery::read()
        .await?
        .devices
        .into_iter()
        .map(|d| d.mux)
        .collect())
}

pub fn mux_addr() -> Result<UsbmuxdAddr> {
    UsbmuxdAddr::from_env_var().context("invalid USBMUXD_SOCKET_ADDRESS")
}

pub async fn mux_connect() -> Result<UsbmuxdConnection> {
    let addr = mux_addr()?;
    events::step(
        "usbmuxd.connect",
        json!({ "address": format!("{addr:?}") }),
        STEP_TIMEOUT,
        addr.connect(0),
    )
    .await
}

pub fn describe_raw(devices: &[UsbmuxdDevice]) -> Vec<serde_json::Value> {
    devices.iter().map(|d| json!({
        "udid": d.udid, "device_id": d.device_id, "connection": conn_name(&d.connection_type),
        "address": match &d.connection_type { Connection::Network(ip) => Some(ip.to_string()), _ => None },
    })).collect()
}

/// Connected devices, one entry per phone, USB preferred.
pub async fn list() -> Result<Vec<DeviceEntry>> {
    Ok(summarize(list_raw().await?))
}

pub(crate) fn summarize(devices: impl IntoIterator<Item = UsbmuxdDevice>) -> Vec<DeviceEntry> {
    let mut out: Vec<DeviceEntry> = Vec::new();
    for d in devices {
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
    out
}

pub(crate) fn recall_ip(udid: &str) -> Option<WifiAddress> {
    address::cached(udid)
}

/// What lockdown says about the passcode lock (`PasswordProtected`: true
/// while the phone is locked). `None` when lockdown did not say. Read-only.
pub async fn passcode_locked(provider: &dyn IdeviceProvider) -> Option<bool> {
    use idevice::lockdown::LockdownClient;
    let wait = |secs| Duration::from_secs(secs);
    let mut lockdown = tokio::time::timeout(wait(8), LockdownClient::connect(provider))
        .await
        .ok()?
        .ok()?;
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
    pub(crate) source: &'static str,
    pub(crate) address: Option<WifiAddress>,
    pub(crate) identity: Option<Identity>,
    /// Every Wi-Fi route owns its heartbeat; USB has none.
    pub heartbeat: Option<Heartbeat>,
}

/// Answers the phone's heartbeat for as long as it lives. Over Wi-Fi the phone
/// closes the connections to its services (CoreDeviceProxy, AMFI, the image
/// mounter) unless some client answers its Marco. iph owns this for every
/// Wi-Fi address, regardless of where the address was discovered.
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

    pub(crate) fn is_closed(&self) -> bool {
        self.0.is_finished()
    }

    fn keep(mut client: HeartbeatClient, mut interval: u64) -> Self {
        Self(tokio::spawn(async move {
            // The phone sends a Marco every `interval` seconds; wait a little longer.
            loop {
                let result = async {
                    interval = client
                        .get_marco(interval.saturating_add(HEARTBEAT_GRACE_SECS))
                        .await?;
                    client.send_polo().await
                }
                .await;
                if let Err(e) = result {
                    events::record(
                        "heartbeat",
                        "closed",
                        0,
                        json!({ "error": format!("{e:?}") }),
                    );
                    break;
                }
            }
        }))
    }
}

/// Discovery sources supply candidates, never an alternate Wi-Fi transport.
#[derive(Debug, Clone)]
pub(crate) enum Candidate {
    Usb(UsbmuxdDevice),
    Wifi {
        address: WifiAddress,
        source: &'static str,
    },
}

pub(crate) fn candidates(
    udid: &str,
    found: &Discovery,
    saved: Option<WifiAddress>,
    wifi_first: bool,
) -> Vec<Candidate> {
    let mut usb = Vec::new();
    let mut wifi = Vec::new();
    for device in found.devices.iter().filter(|d| d.mux.udid == udid) {
        if device.mux.connection_type == Connection::Usb {
            usb.push(Candidate::Usb(device.mux.clone()));
        } else if let Some(address) = device.address
            && !wifi
                .iter()
                .any(|c| matches!(c, Candidate::Wifi { address: a, .. } if *a == address))
        {
            wifi.push(Candidate::Wifi {
                address,
                source: "usbmuxd",
            });
        }
    }
    // Fresh IPv4 hints first, then IPv6 (with its interface preserved).
    wifi.sort_by_key(|c| matches!(c, Candidate::Wifi { address, .. } if address.ip.is_ipv6()));
    if let Some(address) = saved
        && !wifi
            .iter()
            .any(|c| matches!(c, Candidate::Wifi { address: a, .. } if *a == address))
    {
        wifi.push(Candidate::Wifi {
            address,
            source: "cached_wifi_address",
        });
    }
    if wifi_first {
        wifi.extend(usb);
        wifi
    } else {
        usb.extend(wifi);
        usb
    }
}

/// Retry connection establishment only. This never runs a user's command.
async fn try_candidates<T, F: Future<Output = Result<T>>>(
    plan: Vec<Candidate>,
    mut open: impl FnMut(Candidate) -> F,
) -> Result<T> {
    let mut failures = Vec::new();
    for candidate in plan {
        let description = match &candidate {
            Candidate::Usb(_) => "USB".to_string(),
            Candidate::Wifi { address, source } => format!("TCP {address} ({source})"),
        };
        match open(candidate).await {
            Ok(value) => return Ok(value),
            Err(e) => failures.push(format!("{description}: {e:#}")),
        }
    }
    bail!("no usable connection: {}", failures.join("; "))
}

async fn open_candidate(udid: &str, candidate: &Candidate) -> Result<Route> {
    match candidate {
        Candidate::Usb(device) => Ok(Route {
            provider: Box::new(device.to_provider(mux_addr()?, LABEL)),
            connection: "usb".into(),
            source: "usbmuxd",
            address: None,
            identity: None,
            heartbeat: None,
        }),
        Candidate::Wifi { address, source } => {
            let provider = direct_provider(udid, *address).await?;
            let identity = identity::identify(&provider)
                .await
                .map_err(|f| anyhow::anyhow!(f.to_string()))?;
            if !identity.udid.eq_ignore_ascii_case(udid) {
                bail!("paired-session UDID does not match the selected iPhone");
            }
            let heartbeat = Heartbeat::start(&provider).await?;
            events::record(
                "route",
                "ok",
                0,
                json!({ "udid": udid, "transport": "tcp", "source": source, "address": address.to_string() }),
            );
            Ok(Route {
                provider: Box::new(provider),
                connection: "wifi".into(),
                source,
                address: Some(*address),
                identity: Some(identity),
                heartbeat: Some(heartbeat),
            })
        }
    }
}

async fn with_discovery<T, F: Future<Output = Result<T>>>(
    udid: &str,
    found: Result<Discovery>,
    finish: impl Fn(Route) -> F + Copy,
) -> Result<T> {
    let (found, discovery_error) = match found {
        Ok(found) => (found, None),
        Err(e) => {
            let message = format!("{e:#}");
            events::record("discovery", "error", 0, json!({ "error": message }));
            (Discovery::default(), Some(message))
        }
    };
    let wifi_first = std::env::var("IPH_PREFER").is_ok_and(|v| v.eq_ignore_ascii_case("wifi"));
    let plan = candidates(udid, &found, recall_ip(udid), wifi_first);
    if plan.is_empty() {
        if let Some(e) = discovery_error {
            bail!("device {udid}: discovery failed ({e}); no cached Wi-Fi address");
        }
        bail!("device {udid} is not connected (usbmuxd does not list it); no cached Wi-Fi address");
    }
    let result = try_candidates(plan, |candidate| async move {
        let route = open_candidate(udid, &candidate).await?;
        let result = finish(route).await?;
        if let Candidate::Wifi { address, .. } = candidate
            && let Err(e) = address::save(&paths::state_dir(), udid, address)
        {
            events::record(
                "wifi_address.save",
                "error",
                0,
                json!({ "udid": udid, "error": e.to_string() }),
            );
        }
        Ok(result)
    })
    .await;
    result.with_context(|| match discovery_error {
        Some(e) => format!("discovery unavailable: {e}"),
        None => format!("connection candidates for {udid} failed"),
    })
}

/// Prefer USB, then verified direct TCP candidates. A failed discovery query
/// does not suppress a saved address; pairing still comes from Apple's store.
pub async fn route(udid: &str) -> Result<Route> {
    route_from(udid, discovery::read().await).await
}

pub(crate) async fn route_from(udid: &str, found: Result<Discovery>) -> Result<Route> {
    with_discovery(udid, found, |route| std::future::ready(Ok(route))).await
}

pub(crate) async fn route_device(device: &discovery::Device) -> Result<Route> {
    let candidate = if device.mux.connection_type == Connection::Usb {
        Candidate::Usb(device.mux.clone())
    } else {
        Candidate::Wifi {
            address: device
                .address
                .context("device has no usable Wi-Fi address")?,
            source: "usbmuxd",
        }
    };
    open_candidate(&device.mux.udid, &candidate).await
}

pub(crate) async fn direct_provider(udid: &str, address: WifiAddress) -> Result<TcpProvider> {
    let mut mux = mux_connect().await?;
    let pairing_file = step("usbmuxd.ReadPairRecord", mux.get_pair_record(udid))
        .await
        .with_context(|| format!("no readable pairing record for device {udid}"))?;
    Ok(TcpProvider {
        addr: address.ip,
        scope_id: Some(address.scope_id),
        pairing_file,
        label: LABEL.into(),
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

impl Tunnel {
    /// Why the tunnel carries nothing more: its heartbeat ended (Wi-Fi), or
    /// its packet task did (any connection, USB included).
    pub(crate) fn closed(&self) -> Option<&'static str> {
        if self.heartbeat.as_ref().is_some_and(Heartbeat::is_closed) {
            Some("heartbeat ended")
        } else {
            self.adapter.is_closed().then_some("tunnel task ended")
        }
    }
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Open the userspace tunnel and read the RSD service list.
pub async fn open_tunnel(udid: &str) -> Result<Tunnel> {
    with_discovery(udid, discovery::read().await, |mut route| async move {
        let mut tunnel = open_tunnel_with_provider(&*route.provider, &route.connection).await?;
        tunnel.heartbeat = route.heartbeat.take();
        Ok(tunnel)
    })
    .await
}

/// Test each transport independently, including Wi-Fi while USB is present.
pub async fn open_tunnel_with_provider(
    provider: &dyn IdeviceProvider,
    connection: &str,
) -> Result<Tunnel> {
    let t0 = Instant::now();
    let proxy = step("CoreDeviceProxy", CoreDeviceProxy::connect(provider)).await?;
    let info = proxy.tunnel_info().clone();
    let rsd_port = info.server_rsd_port;
    let ours: IpAddr = info
        .client_address
        .parse()
        .context("tunnel client address")?;
    let theirs: IpAddr = info
        .server_address
        .parse()
        .context("tunnel server address")?;
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

    fn discovered(
        udid: &str,
        connection: Connection,
        address: Option<WifiAddress>,
    ) -> discovery::Device {
        discovery::Device {
            mux: UsbmuxdDevice {
                udid: udid.into(),
                device_id: 1,
                connection_type: connection,
            },
            address,
        }
    }

    fn label(candidate: &Candidate) -> String {
        match candidate {
            Candidate::Usb(_) => "usb".into(),
            Candidate::Wifi { address, source } => format!("{address}:{source}"),
        }
    }

    #[test]
    fn priority_is_usb_then_fresh_addresses_then_cache_unless_wifi_is_requested() {
        let v4: WifiAddress = "192.0.2.1".parse().unwrap();
        let v6: WifiAddress = "fe80::1%7".parse().unwrap();
        let saved: WifiAddress = "192.0.2.2".parse().unwrap();
        let found = Discovery {
            devices: vec![
                discovered("OTHER", Connection::Network(saved.ip), Some(saved)),
                discovered("PHONE", Connection::Network(v6.ip), Some(v6)),
                discovered("PHONE", Connection::Network(v4.ip), Some(v4)),
                discovered("PHONE", Connection::Usb, None),
                discovered("PHONE", Connection::Network(v4.ip), Some(v4)),
            ],
            warnings: vec![],
        };
        let names: Vec<_> = candidates("PHONE", &found, Some(saved), false)
            .iter()
            .map(label)
            .collect();
        assert_eq!(
            names,
            [
                "usb",
                "192.0.2.1:usbmuxd",
                "fe80::1%7:usbmuxd",
                "192.0.2.2:cached_wifi_address"
            ]
        );
        let names: Vec<_> = candidates("PHONE", &found, Some(v4), true)
            .iter()
            .map(label)
            .collect();
        assert_eq!(names, ["192.0.2.1:usbmuxd", "fe80::1%7:usbmuxd", "usb"]);
        assert!(candidates("MISSING", &found, None, false).is_empty());
    }

    #[tokio::test]
    async fn only_failed_connection_setups_fall_through_and_success_stops_the_loop() {
        let address: WifiAddress = "192.0.2.1".parse().unwrap();
        let found = Discovery {
            devices: vec![discovered("PHONE", Connection::Usb, None)],
            warnings: vec![],
        };
        let mut calls = Vec::new();
        let got = try_candidates(candidates("PHONE", &found, Some(address), false), |c| {
            calls.push(label(&c));
            std::future::ready(if matches!(c, Candidate::Usb(_)) {
                Err(anyhow::anyhow!("USB failed before command"))
            } else {
                Ok("tcp")
            })
        })
        .await
        .unwrap();
        assert_eq!(got, "tcp");
        assert_eq!(calls, ["usb", "192.0.2.1:cached_wifi_address"]);
        calls.clear();
        try_candidates(candidates("PHONE", &found, Some(address), false), |c| {
            calls.push(label(&c));
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();
        assert_eq!(calls, ["usb"]);
    }

    #[tokio::test]
    async fn connection_failure_keeps_each_candidate_error() {
        let plan = vec![
            Candidate::Wifi {
                address: "192.0.2.1".parse().unwrap(),
                source: "usbmuxd",
            },
            Candidate::Wifi {
                address: "192.0.2.2".parse().unwrap(),
                source: "cached_wifi_address",
            },
        ];
        let error = try_candidates(plan, |_| {
            std::future::ready(Err::<(), _>(anyhow::anyhow!("session rejected")))
        })
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("192.0.2.1")
                && text.contains("192.0.2.2")
                && text.contains("session rejected")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_timeout_is_observable_instead_of_leaving_a_live_flag() {
        let (host, _phone) = duplex(4096);
        let heartbeat = Heartbeat::keep(
            HeartbeatClient::new(Idevice::new(Box::new(host), "test")),
            1,
        );
        tokio::task::yield_now().await;
        assert!(!heartbeat.is_closed());
        tokio::time::advance(Duration::from_secs(7)).await;
        tokio::task::yield_now().await;
        assert!(heartbeat.is_closed());
    }

    #[tokio::test]
    async fn sleepy_time_or_eof_invalidates_the_cached_session_before_the_next_command() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for sleepy in [false, true] {
                    let (host, mut phone) = duplex(4096);
                    let heartbeat = Heartbeat::keep(
                        HeartbeatClient::new(Idevice::new(Box::new(host), "test")),
                        1,
                    );
                    let (transport, _peer) = duplex(4096);
                    let rsd = RsdHandshake {
                        services: Default::default(),
                        properties: Default::default(),
                        protocol_version: 1,
                        uuid: "test".into(),
                    };
                    let adapter = Adapter::new(
                        Box::new(transport),
                        "fd00::1".parse().unwrap(),
                        "fd00::2".parse().unwrap(),
                    )
                    .to_async_handle();
                    let mut session = crate::device::session::Session::new(
                        "TEST-PHONE".into(),
                        crate::config::Config::default(),
                    );
                    session.tunnel = Some(Tunnel {
                        adapter,
                        rsd,
                        connection: "wifi".into(),
                        open_ms: 0,
                        heartbeat: Some(heartbeat),
                    });
                    session.refresh_connection();
                    assert!(
                        session.tunnel.is_some(),
                        "a live heartbeat must keep the existing session"
                    );
                    if sleepy {
                        send(
                            &mut phone,
                            plist::Dictionary::from_iter([(
                                "Command".to_string(),
                                plist::Value::from("SleepyTime"),
                            )]),
                        )
                        .await;
                    }
                    drop(phone);
                    tokio::time::timeout(Duration::from_secs(1), async {
                        while session.tunnel.as_ref().unwrap().closed().is_none() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    assert_eq!(session.tunnel.as_ref().unwrap().closed(), Some("heartbeat ended"));
                    session.refresh_connection();
                    assert!(session.tunnel.is_none());
                    assert!(session.connection().is_none());
                }
            })
            .await;
    }

    /// One length-prefixed plist, as lockdown services speak.
    async fn send(phone: &mut (impl AsyncWriteExt + Unpin), dict: plist::Dictionary) {
        let mut body = Vec::new();
        plist::Value::Dictionary(dict)
            .to_writer_xml(&mut body)
            .unwrap();
        phone
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .unwrap();
        phone.write_all(&body).await.unwrap();
    }

    async fn recv(phone: &mut (impl AsyncReadExt + Unpin)) -> plist::Dictionary {
        let mut len = [0u8; 4];
        phone.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        phone.read_exact(&mut body).await.unwrap();
        plist::Value::from_reader(std::io::Cursor::new(body))
            .unwrap()
            .into_dictionary()
            .unwrap()
    }

    #[tokio::test]
    async fn heartbeat_answers_every_marco_with_a_polo_until_dropped() {
        let (host, mut phone) = duplex(4096);
        let client = HeartbeatClient::new(Idevice::new(Box::new(host), "test"));
        let heartbeat = Heartbeat::keep(client, 1);
        for _ in 0..2 {
            let marco =
                plist::Dictionary::from_iter([("Interval".to_string(), plist::Value::from(1u64))]);
            send(&mut phone, marco).await;
            let polo = recv(&mut phone).await;
            assert_eq!(
                polo.get("Command").and_then(|v| v.as_string()),
                Some("Polo")
            );
        }
        drop(heartbeat);
        // Dropping stops the task, which closes the host end of the pipe.
        let mut rest = Vec::new();
        assert_eq!(phone.read_to_end(&mut rest).await.unwrap(), 0);
    }
}
