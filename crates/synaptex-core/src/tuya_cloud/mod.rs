pub mod discovery;

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, Mac};
use rand::Rng;
use reqwest::Method;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::db::TuyaCloudConfig;

// ─── Token cache ─────────────────────────────────────────────────────────────

struct TokenCache {
    access_token:  String,
    expires_at_ms: u64,
}

// ─── Client ──────────────────────────────────────────────────────────────────

pub struct TuyaCloudClient {
    client_id:     String,
    client_secret: String,
    base_url:      String,
    uid:           String,
    token:         RwLock<Option<TokenCache>>,
    http:          reqwest::Client,
}

impl TuyaCloudClient {
    pub fn new(cfg: &TuyaCloudConfig) -> Self {
        Self {
            client_id:     cfg.client_id.clone(),
            client_secret: cfg.client_secret.clone(),
            base_url:      cfg.region.base_url().to_string(),
            uid:           cfg.uid.clone(),
            token:         RwLock::new(None),
            http:          reqwest::Client::new(),
        }
    }

    /// Constructor used only during config registration — uid is not yet known.
    /// Only call `get_device` / `get_uid_for_device` on a client built this way.
    pub fn for_uid_resolution(client_id: &str, client_secret: &str, base_url: &str) -> Self {
        Self {
            client_id:     client_id.to_string(),
            client_secret: client_secret.to_string(),
            base_url:      base_url.to_string(),
            uid:           String::new(),
            token:         RwLock::new(None),
            http:          reqwest::Client::new(),
        }
    }

    // ── Signing ──────────────────────────────────────────────────────────────

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn nonce() -> String {
        let bytes: [u8; 16] = rand::thread_rng().gen();
        hex::encode(bytes)
    }

    fn body_hash(body: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(body);
        hex::encode(hasher.finalize())
    }

    fn hmac_sign(secret: &str, msg: &str) -> String {
        type HmacSha256 = Hmac<sha2::Sha256>;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .expect("HMAC key length is always valid");
        mac.update(msg.as_bytes());
        hex::encode(mac.finalize().into_bytes()).to_uppercase()
    }

    /// Build the sign string.
    ///
    /// Tuya format (confirmed working):
    ///   {client_id}{access_token}{t}{nonce}{METHOD}\n{sha256(body)}\n\n{path_and_query}
    ///
    /// For the token endpoint, pass `access_token = ""`.
    /// Note: no separator between nonce and the HTTP method — the StringToSign
    /// is concatenated directly after nonce (the \n chars are *inside* StringToSign).
    fn sign_str(
        &self,
        access_token:   &str,
        ts:             u64,
        nonce:          &str,
        method:         &Method,
        path_and_query: &str,
        body_bytes:     &[u8],
    ) -> String {
        let string_to_sign = format!(
            "{}\n{}\n\n{}",
            method.as_str(),
            Self::body_hash(body_bytes),
            path_and_query,
        );
        format!("{}{}{}{}{}", self.client_id, access_token, ts, nonce, string_to_sign)
    }

    fn make_headers(
        &self,
        access_token: &str,
        ts:           u64,
        nonce:        &str,
        signature:    &str,
    ) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("client_id",   self.client_id.parse().unwrap());
        headers.insert("sign",        signature.parse().unwrap());
        headers.insert("t",           ts.to_string().parse().unwrap());
        headers.insert("nonce",       nonce.parse().unwrap());
        headers.insert("sign_method", "HMAC-SHA256".parse().unwrap());
        if !access_token.is_empty() {
            headers.insert("access_token", access_token.parse().unwrap());
        }
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        headers
    }

    // ── Token management ─────────────────────────────────────────────────────

    async fn get_access_token(&self) -> Result<String> {
        {
            let guard = self.token.read().await;
            if let Some(ref c) = *guard {
                if Self::now_ms() + 60_000 < c.expires_at_ms {
                    return Ok(c.access_token.clone());
                }
            }
        }

        let ts    = Self::now_ms();
        let nonce = Self::nonce();
        let path  = "/v1.0/token?grant_type=1";

        let msg = self.sign_str("", ts, &nonce, &Method::GET, path, b"");
        let sig = Self::hmac_sign(&self.client_secret, &msg);
        let headers = self.make_headers("", ts, &nonce, &sig);

        let url = format!("{}{}", self.base_url, path);
        let raw = self.http.get(&url).headers(headers).send().await?.text().await?;
        let resp: TuyaResponse<TokenResult> = serde_json::from_str(&raw)?;

        if !resp.success {
            bail!("Tuya token fetch failed: code={} msg={}", resp.code, resp.msg);
        }
        let result = resp.result.ok_or_else(|| anyhow!("empty token result"))?;

        let expires_at_ms = Self::now_ms() + (result.expire_time as u64) * 1000;
        let token = result.access_token.clone();
        *self.token.write().await = Some(TokenCache {
            access_token: result.access_token,
            expires_at_ms,
        });
        Ok(token)
    }

    // ── Generic signed request ────────────────────────────────────────────────

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path:   &str,
        body:   Option<&impl Serialize>,
    ) -> Result<T> {
        let access_token = self.get_access_token().await?;
        let ts    = Self::now_ms();
        let nonce = Self::nonce();

        let body_bytes = match body {
            Some(b) => serde_json::to_vec(b)?,
            None    => Vec::new(),
        };

        let msg = self.sign_str(&access_token, ts, &nonce, &method, path, &body_bytes);
        let sig = Self::hmac_sign(&self.client_secret, &msg);
        let headers = self.make_headers(&access_token, ts, &nonce, &sig);

        let url = format!("{}{}", self.base_url, path);
        let mut req = self.http.request(method, &url).headers(headers);
        if !body_bytes.is_empty() {
            req = req.body(body_bytes);
        }

        let resp: TuyaResponse<T> = req.send().await?.json().await?;
        if !resp.success {
            bail!("Tuya API error on {path}: code={} msg={}", resp.code, resp.msg);
        }
        resp.result.ok_or_else(|| anyhow!("empty result for {path}"))
    }

    // ── Public API ────────────────────────────────────────────────────────────

    pub async fn list_devices(&self, page: u32, page_size: u32) -> Result<Vec<CloudDevice>> {
        let path = format!("/v1.0/users/{}/devices?page_no={page}&page_size={page_size}", self.uid);
        let raw: Vec<CloudDeviceRaw> = self.request(Method::GET, &path, None::<&()>).await?;
        Ok(raw.into_iter().map(CloudDevice::from).collect())
    }

    /// Resolve the account owner UID from any device in the account.
    /// Used at config-save time; the result is stored in `TuyaCloudConfig::uid`.
    pub async fn get_uid_for_device(&self, device_id: &str) -> Result<String> {
        let path = format!("/v1.0/devices/{device_id}");
        let raw: CloudDeviceRaw = self.request(Method::GET, &path, None::<&()>).await?;
        Ok(raw.uid)
    }

    pub async fn get_device(&self, device_id: &str) -> Result<CloudDevice> {
        let path = format!("/v1.0/devices/{device_id}");
        let raw: CloudDeviceRaw = self.request(Method::GET, &path, None::<&()>).await?;
        Ok(CloudDevice::from(raw))
    }

    /// Fetch the DP function / status schema for a device.
    /// Returns `DeviceSpecs` with all DP IDs and their raw value descriptors.
    pub async fn get_device_specs(&self, device_id: &str) -> Result<DeviceSpecs> {
        let path = format!("/v1.0/devices/{device_id}/specifications");
        let raw: DeviceSpecsRaw = self.request(Method::GET, &path, None::<&()>).await?;
        let dp_ids = raw.functions.iter()
            .chain(raw.status.iter())
            .map(|s| s.dp_id)
            .collect();
        Ok(DeviceSpecs { dp_ids })
    }

    /// Cloud de-registration — removes device from cloud account without
    /// necessarily physically resetting the device hardware.
    pub async fn factory_reset(&self, device_id: &str) -> Result<()> {
        let path = format!("/v1.0/devices/{device_id}");
        self.request::<serde_json::Value>(Method::DELETE, &path, None::<&()>).await?;
        Ok(())
    }
}

// ─── Device specifications ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DpSpecRaw {
    dp_id: u16,
}

#[derive(Deserialize)]
struct DeviceSpecsRaw {
    #[serde(default)]
    functions: Vec<DpSpecRaw>,
    #[serde(default)]
    status:    Vec<DpSpecRaw>,
}

/// Processed device DP specifications — ready for profile detection.
pub struct DeviceSpecs {
    /// Union of all DP IDs from `functions` and `status`.
    pub dp_ids: std::collections::HashSet<u16>,
}

// ─── Wire types ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TuyaResponse<T> {
    success: bool,
    #[serde(default)]
    code:    i64,
    #[serde(default)]
    msg:     String,
    result:  Option<T>,
}

#[derive(Deserialize)]
struct TokenResult {
    access_token: String,
    expire_time:  u32,
}

#[derive(Deserialize)]
struct CloudDeviceRaw {
    pub id:         String,
    pub name:       String,
    pub category:   String,
    pub product_id: String,
    pub online:     bool,
    #[serde(default)]
    pub uid:        String,
    #[serde(default)]
    pub local_key:  String,
    #[serde(default)]
    pub firmware_version: Option<String>,
}

pub struct CloudDevice {
    pub id:         String,
    pub name:       String,
    pub category:   String,
    pub product_id: String,
    pub online:     bool,
    pub local_key:  String,
    pub firmware:   Option<String>,
}

impl From<CloudDeviceRaw> for CloudDevice {
    fn from(r: CloudDeviceRaw) -> Self {
        CloudDevice {
            id:         r.id,
            name:       r.name,
            category:   r.category,
            product_id: r.product_id,
            online:     r.online,
            local_key:  r.local_key,
            firmware:   r.firmware_version,
        }
    }
}
