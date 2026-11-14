//! USB-only wireless lockdown inspection and setup. Network discovery is a
//! separate observation: setting the flags alone does not prove Wi-Fi works.

use std::{future::Future, time::Duration};

use idevice::{
    IdeviceError, IdeviceService,
    lockdown::LockdownClient,
    provider::IdeviceProvider,
    usbmuxd::{Connection, UsbmuxdDevice},
};

use super::conn;

const DOMAIN: &str = "com.apple.mobile.wireless_lockdown";
const KEYS: [&str; 4] = [
    "SupportsWifi",
    "SupportsWifiSyncing",
    "EnableWifiConnections",
    "EnableWifiDebugging",
];
const STEP_TIMEOUT: Duration = Duration::from_secs(3);
const DISCOVERY_WAIT: Duration = Duration::from_secs(4);

#[derive(Debug, Default)]
pub struct Wireless {
    /// Missing, unreadable and non-boolean values stay unknown, never false.
    pub values: [Option<bool>; 4],
    pub changed: Vec<&'static str>,
    pub errors: Vec<String>,
}

impl Wireless {
    pub fn supported(&self) -> Option<bool> {
        match (self.values[0], self.values[1]) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        }
    }

    pub fn disabled(&self) -> Vec<&'static str> {
        (2..4)
            .filter(|&i| self.values[i] == Some(false))
            .map(|i| KEYS[i])
            .collect()
    }

    pub fn enabled(&self) -> bool {
        self.values[2..].iter().all(|v| *v == Some(true))
    }

    pub fn detail(&self) -> String {
        let mut parts: Vec<String> = KEYS
            .iter()
            .zip(self.values)
            .map(|(key, value)| {
                format!(
                    "{key}={}",
                    match value {
                        Some(true) => "true",
                        Some(false) => "false",
                        None => "unknown",
                    }
                )
            })
            .collect();
        if !self.changed.is_empty() {
            parts.push(format!(
                "enabled and read back: {}",
                self.changed.join(", ")
            ));
        }
        parts.extend(self.errors.iter().cloned());
        format!("{DOMAIN}: {}", parts.join("; "))
    }
}

async fn step<T>(
    name: &str,
    fut: impl Future<Output = Result<T, IdeviceError>>,
) -> Result<T, String> {
    match tokio::time::timeout(STEP_TIMEOUT, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(format!("{name}: {e:?}")),
        Err(_) => Err(format!("{name}: timed out")),
    }
}

async fn read_flag(client: &mut LockdownClient, key: &str) -> Result<bool, String> {
    step(
        &format!("GetValue({key})"),
        client.get_value(Some(key), Some(DOMAIN)),
    )
    .await?
    .as_boolean()
    .ok_or_else(|| format!("GetValue({key}): expected a boolean"))
}

async fn inspect_client(client: &mut LockdownClient, fix: bool) -> Wireless {
    let mut state = Wireless::default();
    for (i, key) in KEYS.iter().enumerate() {
        match read_flag(client, key).await {
            Ok(value) => state.values[i] = Some(value),
            Err(e) => {
                let timed_out = e.ends_with(": timed out");
                state.errors.push(e);
                // A late reply must not be mistaken for the next request's.
                if timed_out {
                    return state;
                }
            }
        }
    }
    if !fix || state.disabled().is_empty() {
        return state;
    }
    if state.supported() != Some(true) {
        state
            .errors
            .push("wireless support not confirmed; no flags were changed".into());
        return state;
    }
    for i in 2..4 {
        if state.values[i] != Some(false) {
            continue;
        }
        let key = KEYS[i];
        if let Err(e) = step(
            &format!("SetValue({key}=true)"),
            client.set_value(key, true.into(), Some(DOMAIN)),
        )
        .await
        {
            let timed_out = e.ends_with(": timed out");
            state.errors.push(e);
            if timed_out {
                state.values[i] = None;
                return state;
            }
            continue;
        }
        // Do not call an acknowledged write a fix until the device confirms it.
        match read_flag(client, key).await {
            Ok(true) => {
                state.values[i] = Some(true);
                state.changed.push(key);
            }
            Ok(false) => state
                .errors
                .push(format!("SetValue({key}=true): read back false")),
            Err(e) => {
                state.values[i] = None;
                let timed_out = e.ends_with(": timed out");
                state.errors.push(e);
                if timed_out {
                    return state;
                }
            }
        }
    }
    state
}

pub async fn inspect(provider: &dyn IdeviceProvider, fix: bool) -> Wireless {
    let result = async {
        let mut client = step(
            "wireless lockdown connect",
            LockdownClient::connect(provider),
        )
        .await?;
        let pairing = step("wireless pairing record", provider.get_pairing_file()).await?;
        step("wireless lockdown session", client.start_session(&pairing)).await?;
        Ok::<_, String>(inspect_client(&mut client, fix).await)
    }
    .await;
    result.unwrap_or_else(|e| Wireless {
        errors: vec![e],
        ..Default::default()
    })
}

/// Always select the target's USB entry, even with IPH_PREFER=wifi.
pub fn usb_device<'a>(devices: &'a [UsbmuxdDevice], udid: &str) -> Option<&'a UsbmuxdDevice> {
    devices
        .iter()
        .find(|d| d.udid == udid && matches!(d.connection_type, Connection::Usb))
}

pub fn has_network(devices: &[UsbmuxdDevice], udid: &str) -> bool {
    devices
        .iter()
        .any(|d| d.udid == udid && matches!(d.connection_type, Connection::Network(_)))
}

async fn wait_for_network<F, Fut>(mut probe: F) -> Result<bool, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool, String>>,
{
    let deadline = tokio::time::Instant::now() + DISCOVERY_WAIT;
    loop {
        match tokio::time::timeout_at(deadline, probe()).await {
            Ok(Ok(true)) => return Ok(true),
            Ok(Ok(false)) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("usbmuxd Network discovery timed out".into()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep_until(
            (tokio::time::Instant::now() + Duration::from_millis(250)).min(deadline),
        )
        .await;
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
    }
}

pub async fn discover(udid: &str) -> Result<bool, String> {
    wait_for_network(|| async {
        conn::list_raw()
            .await
            .map(|all| has_network(&all, udid))
            .map_err(|e| format!("{e:#}"))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use idevice::Idevice;
    use plist::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Exchange {
        request: &'static str,
        key: &'static str,
        response: Value,
    }

    fn reply(key: &str, value: impl Into<Value>) -> Value {
        Value::Dictionary([(key.to_string(), value.into())].into_iter().collect())
    }

    fn get(key: &'static str, value: impl Into<Value>) -> Exchange {
        Exchange {
            request: "GetValue",
            key,
            response: reply("Value", value),
        }
    }

    fn set(key: &'static str) -> Exchange {
        Exchange {
            request: "SetValue",
            key,
            response: reply("Request", "SetValue"),
        }
    }

    fn flags(values: [bool; 4]) -> Vec<Exchange> {
        KEYS.into_iter()
            .zip(values)
            .map(|(key, value)| get(key, value))
            .collect()
    }

    // Real LockdownClient over an in-memory plist stream: check the actual
    // domain, keys, boolean type, protocol errors and read-back behavior.
    async fn inspect_script(script: Vec<Exchange>, fix: bool) -> Wireless {
        let (client, mut device) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(async move {
            for exchange in script {
                let len = device.read_u32().await.unwrap();
                let mut bytes = vec![0; len as usize];
                device.read_exact(&mut bytes).await.unwrap();
                let request: Value = plist::from_bytes(&bytes).unwrap();
                let request = request.as_dictionary().unwrap();
                assert_eq!(request["Request"].as_string(), Some(exchange.request));
                assert_eq!(request["Domain"].as_string(), Some(DOMAIN));
                assert_eq!(request["Key"].as_string(), Some(exchange.key));
                if exchange.request == "SetValue" {
                    assert_eq!(request["Value"].as_boolean(), Some(true));
                }
                let mut reply = Vec::new();
                exchange.response.to_writer_xml(&mut reply).unwrap();
                device.write_u32(reply.len() as u32).await.unwrap();
                device.write_all(&reply).await.unwrap();
            }
        });
        let mut client = LockdownClient::new(Idevice::new(Box::new(client), conn::LABEL));
        let state = inspect_client(&mut client, fix).await;
        drop(client);
        server.await.unwrap();
        state
    }

    #[tokio::test]
    async fn ordinary_doctor_reads_all_four_flags_without_writing() {
        let state = inspect_script(flags([true, true, false, false]), false).await;
        assert_eq!(
            state.disabled(),
            ["EnableWifiConnections", "EnableWifiDebugging"]
        );
        assert!(state.changed.is_empty());
        assert!(state.errors.is_empty());
    }

    #[tokio::test]
    async fn setup_enables_both_flags_and_verifies_each_write() {
        let mut script = flags([true, true, false, false]);
        for key in &KEYS[2..] {
            script.extend([set(key), get(key, true)]);
        }
        let state = inspect_script(script, true).await;
        assert!(state.enabled());
        assert_eq!(
            state.changed,
            ["EnableWifiConnections", "EnableWifiDebugging"]
        );
        assert!(state.errors.is_empty(), "{state:?}");
    }

    #[tokio::test]
    async fn enabled_flags_are_idempotent_and_one_support_flag_is_enough() {
        let state = inspect_script(flags([true, true, true, true]), true).await;
        assert!(state.changed.is_empty());
        assert!(state.errors.is_empty());
        let mut script = flags([false, true, true, false]);
        script.extend([set(KEYS[3]), get(KEYS[3], true)]);
        let state = inspect_script(script, true).await;
        assert_eq!(state.changed, ["EnableWifiDebugging"]);
        assert!(state.enabled());
        assert!(state.errors.is_empty());
    }

    #[tokio::test]
    async fn unsupported_or_unknown_support_never_writes() {
        let state = inspect_script(flags([false, false, false, false]), true).await;
        assert_eq!(state.supported(), Some(false));
        assert!(state.changed.is_empty());
        let mut script = flags([false, false, false, false]);
        script[0].response = reply("Error", "MissingValue");
        let state = inspect_script(script, true).await;
        assert_eq!(state.supported(), None);
        assert_eq!(state.values[0], None);
        assert!(state.changed.is_empty());
        assert!(state.detail().contains("MissingValue"));
    }

    #[tokio::test]
    async fn missing_and_non_boolean_flags_are_unknown_and_never_overwritten() {
        let mut script = flags([true, true, false, false]);
        script[2].response = reply("Error", "MissingValue");
        script[3].response = reply("Value", "false");
        let state = inspect_script(script, true).await;
        assert_eq!(state.values[2..], [None, None]);
        assert!(state.disabled().is_empty());
        assert!(state.changed.is_empty());
        assert!(state.errors.len() == 2, "{state:?}");
    }

    #[tokio::test]
    async fn denied_write_reports_key_and_reason_but_other_flag_can_be_fixed() {
        let mut script = flags([true, true, false, false]);
        let mut denied = set(KEYS[2]);
        denied.response = reply("Error", "SetProhibited");
        script.extend([denied, set(KEYS[3]), get(KEYS[3], true)]);
        let state = inspect_script(script, true).await;
        assert_eq!(state.values[2..], [Some(false), Some(true)]);
        assert_eq!(state.changed, ["EnableWifiDebugging"]);
        assert!(
            state
                .detail()
                .contains("SetValue(EnableWifiConnections=true)")
        );
        assert!(state.detail().contains("SetProhibited"));
    }

    #[tokio::test]
    async fn acknowledged_write_is_not_a_fix_unless_read_back_true() {
        let mut script = flags([true, true, true, false]);
        script.extend([set(KEYS[3]), get(KEYS[3], false)]);
        let state = inspect_script(script, true).await;
        assert!(!state.enabled());
        assert!(state.changed.is_empty());
        assert!(state.detail().contains("read back false"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_stops_the_stream_instead_of_misreading_a_late_reply() {
        let (client, _silent_device) = tokio::io::duplex(1 << 16);
        let mut client = LockdownClient::new(Idevice::new(Box::new(client), conn::LABEL));
        let state = inspect_client(&mut client, true).await;
        assert_eq!(state.values, [None; 4]);
        assert!(state.changed.is_empty());
        assert_eq!(state.errors, ["GetValue(SupportsWifi): timed out"]);
    }

    #[test]
    fn usb_and_network_selection_are_scoped_to_the_target_phone() {
        let device = |udid: &str, connection_type| UsbmuxdDevice {
            udid: udid.into(),
            device_id: 1,
            connection_type,
        };
        let devices = [
            device("other", Connection::Usb),
            device("other", Connection::Network("127.0.0.1".parse().unwrap())),
            device("mine", Connection::Usb),
        ];
        assert_eq!(usb_device(&devices, "mine").unwrap().udid, "mine");
        assert!(usb_device(&devices, "missing").is_none());
        assert!(!has_network(&devices, "mine"));
        assert!(has_network(&devices, "other"));
        assert!(usb_device(&devices[1..2], "other").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_waits_for_registration_and_has_a_deadline() {
        let mut attempts = 0;
        assert_eq!(
            wait_for_network(|| {
                attempts += 1;
                std::future::ready(Ok(attempts == 3))
            })
            .await,
            Ok(true)
        );
        assert_eq!(attempts, 3);
        let started = tokio::time::Instant::now();
        assert_eq!(wait_for_network(|| async { Ok(false) }).await, Ok(false));
        assert_eq!(started.elapsed(), DISCOVERY_WAIT);
        let stalled = wait_for_network(|| std::future::pending()).await;
        assert!(stalled.unwrap_err().contains("timed out"));
        assert_eq!(
            wait_for_network(|| async { Err("service unavailable".into()) }).await,
            Err("service unavailable".into())
        );
    }
}
