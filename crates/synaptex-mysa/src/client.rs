//! Mysa REST client — device enumeration and state polling.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use reqwest::Client;

use crate::types::{DeviceListWrapper, MysaDeviceInfo, MysaRawState};

const BASE_URL: &str = "https://mysa-backend.mysa.cloud";
const USER_AGENT: &str = "okhttp/4.11.0";

pub struct MysaHttpClient {
    http: Client,
}

impl MysaHttpClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .user_agent(USER_AGENT)
                .build()
                .expect("build reqwest client"),
        }
    }

    /// `GET /api/v1/devices` — list all devices on the account.
    pub async fn list_devices(&self, id_token: &str) -> Result<Vec<MysaDeviceInfo>> {
        let resp = self.http
            .get(format!("{BASE_URL}/api/v1/devices"))
            .header("Authorization", id_token)
            .send()
            .await
            .context("GET /api/v1/devices")?;

        let status = resp.status();
        let text   = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("list_devices failed ({status}): {text}");
        }

        // Try array first, then wrapped object.
        if let Ok(arr) = serde_json::from_str::<Vec<MysaDeviceInfo>>(&text) {
            return Ok(arr);
        }
        let wrapped: DeviceListWrapper = serde_json::from_str(&text)
            .with_context(|| format!("parse list_devices response: {text}"))?;
        Ok(wrapped.data.unwrap_or_default())
    }

    /// `GET /api/v1/state/batch` — fetch state for multiple devices at once.
    pub async fn get_state_batch(
        &self,
        id_token: &str,
        ids:      &[&str],
    ) -> Result<HashMap<String, MysaRawState>> {
        let body = serde_json::json!({ "deviceIds": ids });
        let resp = self.http
            .get(format!("{BASE_URL}/api/v1/state/batch"))
            .header("Authorization", id_token)
            .json(&body)
            .send()
            .await
            .context("GET /api/v1/state/batch")?;

        let status = resp.status();
        let text   = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("get_state_batch failed ({status}): {text}");
        }

        serde_json::from_str(&text)
            .with_context(|| format!("parse get_state_batch response: {text}"))
    }

    /// `POST /api/v1/state/{device_id}/update` — push a command to the cloud.
    pub async fn post_command(
        &self,
        id_token:  &str,
        device_id: &str,
        payload:   &serde_json::Value,
    ) -> Result<()> {
        let resp = self.http
            .post(format!("{BASE_URL}/api/v1/state/{device_id}/update"))
            .header("Authorization", id_token)
            .json(payload)
            .send()
            .await
            .context("POST /api/v1/state/{device_id}/update")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("post_command failed ({status}): {text}");
        }
        Ok(())
    }
}

impl Default for MysaHttpClient {
    fn default() -> Self {
        Self::new()
    }
}
