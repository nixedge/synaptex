use dioxus::prelude::*;

use crate::{AppSettings, api::CloudDevice};

/// Cloud device list screen — calls GET /pairing/cloud-devices.
#[component]
pub fn DevicesView() -> Element {
    let settings = use_context::<Signal<AppSettings>>();
    let mut selected = use_context::<Signal<Option<CloudDevice>>>();

    let devices = use_resource(move || {
        let client = settings.read().client();
        async move { client.get::<Vec<CloudDevice>>("/pairing/cloud-devices").await }
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
                            {
                                let dev = dev.clone();
                                let status = if dev.online { "online" } else { "offline" };
                                rsx! {
                                    li { key: "{dev.id}",
                                        button {
                                            r#type: "button",
                                            onclick: move |_| selected.set(Some(dev.clone())),
                                            "{dev.name} ({dev.id}) — {status}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}
