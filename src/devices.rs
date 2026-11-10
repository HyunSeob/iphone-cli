//! `iph devices`: connected iPhones and whether each is ready.

use std::time::Duration;

use idevice::{IdeviceService, lockdown::LockdownClient, usbmuxd::UsbmuxdConnection};
use serde_json::{Value, json};

use crate::{config::Config, device::conn, fields, reply::{Fail, Res}};

async fn lockdown_value(client: &mut LockdownClient, key: &str) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(5), client.get_value(Some(key), None))
        .await
        .ok()?
        .ok()?
        .into_string()
}

/// iOS version and model from lockdown. Read-only.
async fn describe(udid: &str) -> (Option<String>, Option<String>) {
    let Ok(route) = conn::route(udid).await else {
        return (None, None);
    };
    let connect = tokio::time::timeout(Duration::from_secs(8), LockdownClient::connect(&*route.provider)).await;
    let Ok(Ok(mut client)) = connect else {
        return (None, None);
    };
    let version = lockdown_value(&mut client, "ProductVersion").await;
    let model = lockdown_value(&mut client, "ProductType").await;
    (version, model)
}

pub async fn cmd_devices(only: Option<String>) -> Res {
    let list = conn::list()
        .await
        .map_err(|e| Fail::not_sent("usbmuxd_unreachable", format!("{e:#}")))?;
    // With a phone chosen (flag, IPH_UDID or config), the others are listed
    // but never contacted.
    let chosen = only
        .or_else(|| std::env::var("IPH_UDID").ok().filter(|u| !u.is_empty()))
        .or_else(|| Config::load().ok().and_then(|c| c.udid));
    let mut mux = UsbmuxdConnection::default().await.ok();
    let mut out: Vec<Value> = Vec::new();
    for d in &list {
        let contact = chosen.as_ref().is_none_or(|c| *c == d.udid);
        let mut entry = json!({ "udid": d.udid, "connection": d.connection });
        if contact {
            let paired = match mux.as_mut() {
                Some(m) => m.get_pair_record(&d.udid).await.is_ok(),
                None => false,
            };
            let (version, model) = if paired { describe(&d.udid).await } else { (None, None) };
            entry["ios_version"] = json!(version);
            entry["model"] = json!(model);
            entry["paired"] = json!(paired);
            entry["ready"] = json!(paired && version.is_some());
            entry["state"] = json!(if !paired {
                "not paired"
            } else if version.is_none() {
                "unreachable"
            } else {
                "ready"
            });
        } else {
            entry["state"] = json!("not selected");
            entry["ready"] = json!(false);
        }
        out.push(entry);
    }
    Ok(fields! { "count": out.len(), "selected": chosen, "devices": out })
}
