//! MysaAccount (shared per account) + MysaPlugin (one per device).

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};
use uuid::Uuid;

use synaptex_types::{
    capability::{Capability, DeviceCommand},
    device::DeviceId,
    plugin::{DevicePlugin, DeviceState, PluginError, PluginResult, StateBusSender},
    DeviceInfo,
};

use crate::{
    auth::{self, aws_creds_need_refresh, id_token_needs_refresh, CognitoSession},
    client::MysaHttpClient,
    mqtt::{self, WorkerCmd},
    types::{MysaConfig, MysaDeviceState},
};

// ─── MysaAccount ─────────────────────────────────────────────────────────────

/// Shared state for a single Mysa account.  One per account, shared across
/// all `MysaPlugin` instances that belong to it.
pub struct MysaAccount {
    pub(crate) username:          String,
    pub(crate) password:          String,
    pub(crate) session:           RwLock<Option<CognitoSession>>,
    pub(crate) state_cache:       DashMap<String, MysaDeviceState>,
    pub(crate) bus_tx:            StateBusSender,
    /// mysa_id → subscribed (used to build SUBSCRIBE packets on reconnect).
    pub(crate) device_ids:        DashMap<String, ()>,
    /// mysa_id → synaptex DeviceId (for emitting StateChangeEvents).
    pub(crate) mysa_to_device_id: DashMap<String, DeviceId>,
    /// Channel to the MQTT worker task.
    pub(crate) cmd_tx:            std::sync::OnceLock<mpsc::Sender<WorkerCmd>>,
}

impl MysaAccount {
    pub fn new(username: String, password: String, bus_tx: StateBusSender) -> Arc<Self> {
        Arc::new(Self {
            username,
            password,
            session:           RwLock::new(None),
            state_cache:       DashMap::new(),
            bus_tx,
            device_ids:        DashMap::new(),
            mysa_to_device_id: DashMap::new(),
            cmd_tx:            std::sync::OnceLock::new(),
        })
    }

    /// Ensure we have a valid, non-expired session; authenticate or refresh as needed.
    pub async fn ensure_auth(self: &Arc<Self>) -> Result<CognitoSession> {
        // Fast path: read-lock check.
        {
            let guard = self.session.read().await;
            if let Some(ref s) = *guard {
                if !id_token_needs_refresh(s) && !aws_creds_need_refresh(s) {
                    return Ok(s.clone());
                }
            }
        }

        // Slow path: write-lock and refresh / authenticate.
        let mut guard = self.session.write().await;
        match guard.as_mut() {
            Some(s) if !id_token_needs_refresh(s) && !aws_creds_need_refresh(s) => {
                // Another task refreshed before us.
                Ok(s.clone())
            }
            Some(s) => {
                // Token exists but needs refresh.
                auth::refresh(s).await?;
                Ok(s.clone())
            }
            None => {
                // First-time authentication.
                let s = auth::authenticate(&self.username, &self.password).await?;
                let result = s.clone();
                *guard = Some(s);
                Ok(result)
            }
        }
    }

    /// Spawn the MQTT worker task.  Must be called once after construction.
    pub fn start_mqtt_worker(self: &Arc<Self>) {
        let (tx, rx) = mpsc::channel(64);
        if self.cmd_tx.set(tx).is_err() {
            warn!("mysa: MQTT worker already started");
            return;
        }
        let account = Arc::clone(self);
        tokio::spawn(mqtt::run_mqtt_worker(account, rx));
        info!("mysa: MQTT worker started");
    }

    /// Register a device for MQTT subscriptions.
    /// If the worker is running, subscribes dynamically; otherwise it will
    /// subscribe on the next reconnect.
    pub fn add_device(self: &Arc<Self>, mysa_id: String, device_id: DeviceId) {
        self.device_ids.insert(mysa_id.clone(), ());
        self.mysa_to_device_id.insert(mysa_id.clone(), device_id);

        if let Some(tx) = self.cmd_tx.get() {
            let _ = tx.try_send(WorkerCmd::Subscribe { device_id: mysa_id });
        }
    }

    /// Get cached state for a device.
    pub fn get_state(&self, mysa_id: &str) -> Option<MysaDeviceState> {
        self.state_cache.get(mysa_id).map(|r| r.clone())
    }

    /// Publish a setpoint/mode command via the MQTT worker, falling back to HTTP.
    pub async fn publish_command(
        self: &Arc<Self>,
        mysa_id:   &str,
        sp:        Option<f32>,
        mode:      Option<u8>,
    ) -> Result<()> {
        let session = self.ensure_auth().await?;

        // Build the MsgType 44 envelope.
        let mut cmd_obj = serde_json::Map::new();
        if let Some(mode) = mode {
            cmd_obj.insert("heatingMode".into(), serde_json::json!(mode));
        }
        if let Some(sp) = sp {
            cmd_obj.insert("setPoint".into(), serde_json::json!(sp));
        }

        let payload_json = serde_json::json!({
            "msgId":   Uuid::new_v4().to_string(),
            "msgType": 44,
            "userId":  session.identity_id,
            "devId":   mysa_id,
            "body":    { "cmd": cmd_obj }
        });
        let payload = serde_json::to_vec(&payload_json)?;
        let topic   = format!("/v1/dev/{mysa_id}/in");

        // Try MQTT first; fall back to HTTP REST.
        if let Some(tx) = self.cmd_tx.get() {
            let queued = tx.try_send(WorkerCmd::Publish {
                topic:   topic.clone(),
                payload: payload.clone(),
                qos:     0,
            });
            if queued.is_ok() {
                return Ok(());
            }
        }

        // HTTP fallback.
        let http = MysaHttpClient::new();
        let http_body = serde_json::json!({ "cmd": cmd_obj });
        http.post_command(&session.id_token, mysa_id, &http_body).await
    }
}

// ─── MysaPlugin ──────────────────────────────────────────────────────────────

pub struct MysaPlugin {
    info:    DeviceInfo,
    cfg:     MysaConfig,
    account: Arc<MysaAccount>,
}

impl MysaPlugin {
    pub fn new(info: DeviceInfo, cfg: MysaConfig, account: Arc<MysaAccount>) -> Self {
        Self { info, cfg, account }
    }
}

#[async_trait]
impl DevicePlugin for MysaPlugin {
    fn device_id(&self)    -> &DeviceId     { &self.info.id }
    fn name(&self)         -> &str          { &self.info.name }
    fn protocol(&self)     -> &str          { "mysa_cloud" }
    fn capabilities(&self) -> &[Capability] { &self.info.capabilities }
    fn is_connected(&self) -> bool          {
        // Consider online if we have a fresh session and a cached state.
        self.account.get_state(&self.cfg.mysa_id).is_some()
    }

    async fn connect(&self) -> PluginResult<()> {
        debug!(device = %self.info.id, mysa_id = %self.cfg.mysa_id, "mysa: connecting");
        self.account.ensure_auth().await
            .map_err(|e| PluginError::Unreachable(e.to_string()))?;
        self.account.add_device(self.cfg.mysa_id.clone(), self.info.id);
        info!(device = %self.info.id, mysa_id = %self.cfg.mysa_id, "mysa: connected");
        Ok(())
    }

    async fn disconnect(&self) {
        // Nothing to do — shared MQTT connection is managed by MysaAccount.
    }

    async fn poll_state(&self) -> PluginResult<DeviceState> {
        // Use cache if available.
        if let Some(s) = self.account.get_state(&self.cfg.mysa_id) {
            return Ok(device_state_from_mysa(self.info.id, &s, true));
        }

        // HTTP fallback.
        let session = self.account.ensure_auth().await
            .map_err(|e| PluginError::Unreachable(e.to_string()))?;

        let http  = MysaHttpClient::new();
        let ids   = [self.cfg.mysa_id.as_str()];
        let batch = http.get_state_batch(&session.id_token, &ids)
            .await
            .map_err(|e| PluginError::Unreachable(e.to_string()))?;

        let raw = batch.get(&self.cfg.mysa_id)
            .cloned()
            .unwrap_or_default();

        let temp_c = (raw.temperature.unwrap_or(20.0) * 10.0).round() as u16;
        let sp_c   = (raw.set_point.unwrap_or(21.0)  * 10.0).round() as u16;
        let mode   = raw.heating_mode.unwrap_or(0);

        let cached = MysaDeviceState {
            temp_current: temp_c,
            temp_set:     sp_c,
            mode,
        };
        self.account.state_cache.insert(self.cfg.mysa_id.clone(), cached.clone());
        Ok(device_state_from_mysa(self.info.id, &cached, true))
    }

    async fn execute_command(&self, cmd: DeviceCommand) -> PluginResult<()> {
        let (sp, mode): (Option<f32>, Option<u8>) = match cmd {
            DeviceCommand::SetPower(true)       => (None, Some(3)),
            DeviceCommand::SetPower(false)      => (None, Some(0)),
            DeviceCommand::SetTargetTemp(tenths) => {
                let celsius = tenths as f32 / 10.0;
                (Some(celsius), None)
            }
            _ => return Err(PluginError::UnsupportedCommand),
        };

        self.account
            .publish_command(&self.cfg.mysa_id, sp, mode)
            .await
            .map_err(|e| PluginError::Unreachable(e.to_string()))
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn device_state_from_mysa(id: DeviceId, s: &MysaDeviceState, online: bool) -> DeviceState {
    DeviceState {
        device_id:        id,
        online,
        updated_at_ms:    now_ms(),
        power:            Some(s.mode != 0),
        brightness:       None,
        color_temp_k:     None,
        rgb:              None,
        mode:             None,
        switches:         HashMap::new(),
        fan_speed:        None,
        temp_current:     Some(s.temp_current),
        temp_set:         Some(s.temp_set),
        temp_calibration: None,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
