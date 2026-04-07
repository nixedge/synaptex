use dioxus::prelude::*;

use crate::api::{CloudDevice, SynaptexClient};

/// Cloud device list screen — calls GET /pairing/cloud-devices.
#[component]
pub fn DevicesView(client: SynaptexClient) -> Element {
    let devices = use_resource(move || {
        let c = client.clone();
        async move { c.get::<Vec<CloudDevice>>("/pairing/cloud-devices").await }
    });

    rsx! {
        div {
            h2 { "Cloud Devices" }
            match devices.read().as_ref() {
                None => rsx! { p { "Loading…" } },
                Some(Err(e)) => rsx! { p { "Error: {e}" } },
                Some(Ok(list)) => rsx! {
                    ul {
                        for dev in list.iter() {
                            li {
                                key: "{dev.id}",
                                {
                                    let status = if dev.online { "online" } else { "offline" };
                                    format!("{} ({}) — {}", dev.name, dev.id, status)
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}
