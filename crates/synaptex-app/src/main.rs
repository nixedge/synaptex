mod api;
mod smartconfig;
mod views;

use dioxus::prelude::*;

use api::CloudDevice;
use views::{
    devices::DevicesView,
    pairing::PairingView,
    settings::SettingsView,
};

// ─── Global app settings ──────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct AppSettings {
    pub server_url: String,
    pub api_key:    Option<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            server_url: "http://192.168.1.1:8080".to_string(),
            api_key:    None,
        }
    }
}

impl AppSettings {
    pub fn client(&self) -> crate::api::SynaptexClient {
        crate::api::SynaptexClient::new(self.server_url.clone(), self.api_key.clone())
    }
}

// ─── Root component ───────────────────────────────────────────────────────────

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let _settings: Signal<AppSettings> = use_context_provider(|| Signal::new(AppSettings::default()));
    let selected:  Signal<Option<CloudDevice>> = use_context_provider(|| Signal::new(None));

    rsx! {
        div {
            nav { span { "Synaptex Pairing" } }

            match selected.read().clone() {
                Some(dev) => rsx! { PairingView { device: dev } },
                None => rsx! {
                    SettingsView {}
                    DevicesView {}
                },
            }
        }
    }
}
