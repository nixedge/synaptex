use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use crate::types::{BondDeviceInfo, BondDeviceState};

/// HTTP client for the Bond local API v2.
pub struct BondClient {
    base_url: String,
    token:    String,
    client:   Client,
}

impl BondClient {
    pub fn new(ip: &str, token: &str) -> Self {
        Self {
            base_url: format!("http://{ip}/v2"),
            token:    token.to_string(),
            client:   Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
        }
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client
            .get(&url)
            .header("BOND-Token", &self.token)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("GET {url}: HTTP {}", resp.status());
        }
        resp.json::<Value>().await.context("parse JSON")
    }

    /// Verify connectivity and return the bridge's bond ID.
    /// Uses `GET /sys/version` which requires no auth (useful as a ping).
    pub async fn verify(&self) -> Result<String> {
        let url = format!("{}/sys/version", self.base_url);
        let v: Value = self.client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .json()
            .await
            .context("parse version")?;
        Ok(v["bondid"].as_str().unwrap_or("").to_string())
    }

    /// Return all Bond device IDs from `GET /devices`.
    /// Bond returns a hash-map whose keys are device IDs; "_" is metadata.
    pub async fn list_device_ids(&self) -> Result<Vec<String>> {
        let map = self.get_json("/devices").await?;
        let ids = map.as_object()
            .map(|obj| {
                obj.keys()
                    .filter(|k| !k.starts_with('_'))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Ok(ids)
    }

    /// Fetch metadata for a single Bond device.
    pub async fn get_device(&self, id: &str) -> Result<BondDeviceInfo> {
        let v = self.get_json(&format!("/devices/{id}")).await?;
        let name        = v["name"].as_str().unwrap_or(id).to_string();
        let device_type = v["type"].as_str().unwrap_or("GX").to_string();
        let actions: Vec<String> = v["actions"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // max_speed lives under properties.max_speed (CF devices).
        let max_speed = v["properties"]["max_speed"].as_u64().unwrap_or(3).max(1) as u8;
        Ok(BondDeviceInfo { id: id.to_string(), name, device_type, actions, max_speed })
    }

    /// Fetch the current state of a Bond device.
    pub async fn get_device_state(&self, id: &str) -> Result<BondDeviceState> {
        let v = self.get_json(&format!("/devices/{id}/state")).await?;
        Ok(BondDeviceState {
            power: v["power"].as_u64().unwrap_or(0) as u8,
            speed: v["speed"].as_u64().unwrap_or(0) as u8,
            light: v["light"].as_u64().unwrap_or(0) as u8,
        })
    }

    /// Execute a Bond action on a device.
    /// `argument` is an optional integer (e.g. fan speed 1–6 for SetSpeed).
    pub async fn execute_action(
        &self,
        device_id: &str,
        action:    &str,
        argument:  Option<u32>,
    ) -> Result<()> {
        let url = format!("{}/devices/{device_id}/actions/{action}", self.base_url);
        let body = match argument {
            Some(arg) => serde_json::json!({ "argument": arg }),
            None      => serde_json::json!({}),
        };
        let resp = self.client
            .put(&url)
            .header("BOND-Token", &self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("PUT {url}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("PUT {url}: HTTP {}", resp.status());
        }
        Ok(())
    }
}
