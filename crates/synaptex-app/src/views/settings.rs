use dioxus::prelude::*;

/// Settings screen: configure server URL and API key.
/// Edits a local draft; Save writes back to the parent signals which
/// causes App to rebuild the SynaptexClient with the new values.
#[component]
pub fn SettingsView(
    server_url: Signal<String>,
    api_key:    Signal<Option<String>>,
) -> Element {
    let mut url_draft = use_signal(|| server_url.read().clone());
    let mut key_draft = use_signal(|| {
        api_key.read().clone().unwrap_or_default()
    });

    rsx! {
        div {
            h2 { "Settings" }

            label { "Server URL" }
            input {
                r#type: "text",
                value: "{url_draft}",
                oninput: move |e| url_draft.set(e.value()),
            }

            label { "API Key" }
            input {
                r#type: "password",
                value: "{key_draft}",
                oninput: move |e| key_draft.set(e.value()),
            }

            button {
                onclick: move |_| {
                    server_url.clone().set(url_draft.read().clone());
                    let k = key_draft.read().clone();
                    api_key.clone().set(if k.is_empty() { None } else { Some(k) });
                },
                "Save"
            }
        }
    }
}
