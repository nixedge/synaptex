use std::{
    net::Ipv4Addr,
    time::{Duration, Instant},
};

use dioxus::prelude::*;
use serde_json::json;

use crate::{
    AppSettings,
    api::{CloudDevice, RegisteredDevice},
    smartconfig::SmartConfigSession,
};

// ─── State machine ───────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum PairingStep {
    Idle,
    WifiEntry,
    Broadcasting { started_at: Instant },
    Acked { ip: Ipv4Addr, tuya_id: String },
    Registering,
    Done { mac: String },
    Error(String),
}

// ─── Component ───────────────────────────────────────────────────────────────

/// Pairing wizard for a single cloud device.
#[component]
pub fn PairingView(device: CloudDevice) -> Element {
    let settings = use_context::<Signal<AppSettings>>();
    let mut selected = use_context::<Signal<Option<CloudDevice>>>();

    let mut step  = use_signal(|| PairingStep::Idle);
    let mut ssid  = use_signal(|| String::new());
    let mut pass  = use_signal(|| String::new());

    let client    = settings.read().client();
    let device_id = device.id.clone();

    rsx! {
        div {
            button {
                r#type: "button",
                onclick: move |_| selected.set(None),
                "← Back"
            }
            h2 { "Pairing: {device.name}" }
            p { "Tuya ID: {device_id}" }

            match &*step.read() {
                PairingStep::Idle => rsx! {
                    p {
                        "Put the device in pairing mode first: hold the button until the LED \
                        flashes rapidly (4–5 times per second), then press Start."
                    }
                    button {
                        r#type: "button",
                        onclick: move |_| step.set(PairingStep::WifiEntry),
                        "Start Pairing"
                    }
                },

                PairingStep::WifiEntry => rsx! {
                    div {
                        label { "WiFi SSID" }
                        input {
                            r#type: "text",
                            value: "{ssid}",
                            oninput: move |e| ssid.set(e.value()),
                        }
                        label { "WiFi Password" }
                        input {
                            r#type: "password",
                            value: "{pass}",
                            oninput: move |e| pass.set(e.value()),
                        }
                        button {
                            r#type: "button",
                            onclick: {
                                let mut step = step.clone();
                                let ssid_val = ssid.read().clone();
                                let pass_val = pass.read().clone();
                                let client   = client.clone();
                                let did      = device_id.clone();
                                let dev_name = device.name.clone();
                                let local_key = device.local_key.clone();
                                move |_| {
                                    let mut step     = step.clone();
                                    let ssid_val     = ssid_val.clone();
                                    let pass_val     = pass_val.clone();
                                    let client       = client.clone();
                                    let did          = did.clone();
                                    let dev_name     = dev_name.clone();
                                    let local_key    = local_key.clone();
                                    let started_at   = Instant::now();
                                    step.set(PairingStep::Broadcasting { started_at });
                                    spawn(async move {
                                        let session = SmartConfigSession::new(ssid_val, pass_val);
                                        match session.run(Duration::from_secs(120)).await {
                                            Ok((ip, tuya_id)) => {
                                                let tid = if tuya_id.is_empty() { did.clone() } else { tuya_id };
                                                step.set(PairingStep::Acked { ip, tuya_id: tid.clone() });

                                                // Fetch cloud device for up-to-date name/local_key.
                                                let cloud = client.get::<crate::api::CloudDevice>(
                                                    &format!("/pairing/cloud-devices/{tid}")
                                                ).await;
                                                let (name, lk) = match cloud {
                                                    Ok(d) => (d.name, d.local_key),
                                                    Err(_) => (dev_name.clone(), local_key.clone()),
                                                };

                                                step.set(PairingStep::Registering);
                                                let body = json!({
                                                    "mac":      format!("{ip}"),
                                                    "name":     name,
                                                    "ip":       ip.to_string(),
                                                    "tuya_id":  tid,
                                                    "local_key": lk,
                                                });
                                                match client.post::<_, RegisteredDevice>("/devices", &body).await {
                                                    Ok(reg) => step.set(PairingStep::Done { mac: reg.mac }),
                                                    Err(e)  => step.set(PairingStep::Error(e.to_string())),
                                                }
                                            }
                                            Err(e) => step.set(PairingStep::Error(e.to_string())),
                                        }
                                    });
                                }
                            },
                            "Broadcast Credentials"
                        }
                    }
                },

                PairingStep::Broadcasting { started_at } => rsx! {
                    p { "Broadcasting SmartConfig… ({started_at.elapsed().as_secs()}s)" }
                },

                PairingStep::Acked { ip, tuya_id } => rsx! {
                    p { "Device ACKed from {ip} (Tuya ID: {tuya_id})" }
                    p { "Registering…" }
                },

                PairingStep::Registering => rsx! { p { "Registering device with daemon…" } },

                PairingStep::Done { mac } => rsx! {
                    p { "✓ Device paired successfully!" }
                    p { "MAC address: {mac}" }
                    button {
                        r#type: "button",
                        onclick: move |_| selected.set(None),
                        "← Back to devices"
                    }
                },

                PairingStep::Error(e) => rsx! {
                    p { "Error: {e}" }
                    button {
                        r#type: "button",
                        onclick: move |_| step.set(PairingStep::Idle),
                        "Retry"
                    }
                },
            }
        }
    }
}
