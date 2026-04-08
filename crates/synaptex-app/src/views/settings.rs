use dioxus::prelude::*;

use crate::AppSettings;

#[component]
pub fn SettingsView() -> Element {
    let mut settings = use_context::<Signal<AppSettings>>();

    let mut url_draft = use_signal(|| settings.read().server_url.clone());
    let mut key_draft = use_signal(|| {
        settings.read().api_key.clone().unwrap_or_default()
    });
    let mut saved = use_signal(|| false);

    rsx! {
        div {
            h2 { "Settings" }

            label { "Server URL" }
            input {
                r#type: "text",
                value: "{url_draft}",
                oninput: move |e| {
                    url_draft.set(e.value());
                    saved.set(false);
                },
            }

            label { "API Key" }
            input {
                r#type: "password",
                value: "{key_draft}",
                oninput: move |e| {
                    key_draft.set(e.value());
                    saved.set(false);
                },
            }

            button {
                r#type: "button",
                onclick: move |_| {
                    let url = url_draft.read().clone();
                    let key = key_draft.read().clone();
                    settings.write().server_url = url;
                    settings.write().api_key = if key.is_empty() { None } else { Some(key) };
                    saved.set(true);
                },
                "Save"
            }

            if *saved.read() {
                span { " ✓ Saved" }
            }
        }
    }
}
