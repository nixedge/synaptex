use anyhow::{bail, Result};
use reqwest::Client;
use serde::{de::DeserializeOwned, Serialize};

/// REST client for the synaptex-core HTTP API.
#[derive(Clone)]
pub struct SynaptexClient {
    pub base_url: String,
    pub api_key:  Option<String>,
    http:         Client,
}

// reqwest::Client doesn't implement PartialEq; compare by config fields only.
impl PartialEq for SynaptexClient {
    fn eq(&self, other: &Self) -> bool {
        self.base_url == other.base_url && self.api_key == other.api_key
    }
}

impl SynaptexClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            http: Client::new(),
        }
    }

    fn auth_header(&self) -> Option<String> {
        self.api_key.as_ref().map(|k| format!("Bearer {k}"))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}/api/v1{path}", self.base_url);
        let mut req = self.http.get(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("GET {path} failed ({status}): {text}");
        }
        Ok(resp.json().await?)
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = format!("{}/api/v1{path}", self.base_url);
        let mut req = self.http.post(&url).json(body);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("POST {path} failed ({status}): {text}");
        }
        Ok(resp.json().await?)
    }

    pub async fn post_no_body(&self, path: &str) -> Result<()> {
        let url = format!("{}/api/v1{path}", self.base_url);
        let mut req = self.http.post(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("POST {path} failed ({status}): {text}");
        }
        Ok(())
    }
}

// ─── DTO types (mirrors crates/synaptex-core/src/rest/dto.rs) ────────────────

#[derive(serde::Deserialize, Clone, Debug, PartialEq)]
pub struct CloudDevice {
    pub id:         String,
    pub name:       String,
    pub category:   String,
    pub product_id: String,
    pub online:     bool,
    pub firmware:   Option<String>,
    pub local_key:  String,
}

#[derive(serde::Deserialize, Clone, Debug, PartialEq)]
pub struct ProbeResult {
    pub supported: Option<bool>,
    pub cached:    bool,
}

#[derive(serde::Deserialize, Clone, Debug)]
pub struct RegisteredDevice {
    pub mac: String,
}
