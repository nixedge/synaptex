mod api;
mod smartconfig;
mod views;

use dioxus::prelude::*;

use api::SynaptexClient;
use views::{
    devices::DevicesView,
    pairing::PairingView,
    settings::SettingsView,
};

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let server_url = use_signal(|| "http://192.168.1.1:8080".to_string());
    let api_key    = use_signal(|| Option::<String>::None);
    let selected   = use_signal(|| Option::<api::CloudDevice>::None);

    // Re-read signals every render so client stays in sync with saved settings.
    let client = SynaptexClient::new(
        server_url.read().clone(),
        api_key.read().clone(),
    );

    rsx! {
        div {
            nav {
                span { "Synaptex Pairing" }
            }

            match selected.read().clone() {
                Some(dev) => rsx! {
                    PairingView {
                        client: client.clone(),
                        device: dev,
                    }
                },
                None => rsx! {
                    SettingsView { server_url, api_key }
                    DevicesView { client }
                },
            }
        }
    }
}
