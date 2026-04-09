use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Show current daemon configuration.
    Show,

    /// Set or generate the REST API key.
    SetApiKey {
        /// Explicit key to use; omit to auto-generate a random 32-hex key.
        #[arg(long)]
        key: Option<String>,
    },

    /// Clear the REST API key, reverting to open/dev mode.
    UnsetApiKey,

    /// Configure Tuya Cloud credentials.
    SetTuyaCloud {
        #[arg(long)]
        client_id: String,
        #[arg(long)]
        client_secret: String,
        /// Region: us | eu | cn | in
        #[arg(long)]
        region: String,
        /// Any device ID in your Tuya account (used once to resolve your owner UID).
        #[arg(long)]
        seed_device_id: String,
    },
}

pub async fn run(cmd: ConfigCmd, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();

    macro_rules! auth {
        ($req:expr) => {
            if let Some(key) = api_key {
                $req.header("Authorization", format!("Bearer {key}"))
            } else {
                $req
            }
        };
    }

    match cmd {
        ConfigCmd::Show => {
            let resp = auth!(client.get(format!("{http_url}/api/v1/config")))
                .send()
                .await
                .context("GET /api/v1/config")?;
            let text = resp.text().await?;
            let val: serde_json::Value = serde_json::from_str(&text)
                .unwrap_or(serde_json::Value::String(text.clone()));
            println!("{}", serde_json::to_string_pretty(&val)?);
        }

        ConfigCmd::SetApiKey { key } => {
            let body = match key {
                Some(k) => json!({ "key": k }),
                None    => json!({}),
            };
            let resp = auth!(client.put(format!("{http_url}/api/v1/config/api-key")).json(&body))
                .send()
                .await
                .context("PUT /api/v1/config/api-key")?;
            let text = resp.text().await?;
            let val: serde_json::Value = serde_json::from_str(&text)
                .unwrap_or(serde_json::Value::String(text.clone()));
            println!("{}", serde_json::to_string_pretty(&val)?);
        }

        ConfigCmd::UnsetApiKey => {
            let resp = auth!(client.delete(format!("{http_url}/api/v1/config/api-key")))
                .send()
                .await
                .context("DELETE /api/v1/config/api-key")?;
            if resp.status().is_success() {
                println!("API key cleared — daemon is now in open/dev mode.");
            } else {
                let text = resp.text().await?;
                anyhow::bail!("server error: {text}");
            }
        }

        ConfigCmd::SetTuyaCloud { client_id, client_secret, region, seed_device_id } => {
            let body = json!({
                "client_id":      client_id,
                "client_secret":  client_secret,
                "region":         region,
                "seed_device_id": seed_device_id,
            });
            let resp = auth!(client.put(format!("{http_url}/api/v1/config/tuya-cloud")).json(&body))
                .send()
                .await
                .context("PUT /api/v1/config/tuya-cloud")?;
            if resp.status().is_success() {
                println!("Tuya Cloud credentials saved.");
            } else {
                let text = resp.text().await?;
                anyhow::bail!("server error: {text}");
            }
        }
    }

    Ok(())
}
