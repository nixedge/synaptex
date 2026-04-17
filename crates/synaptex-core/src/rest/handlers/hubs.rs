/// REST handlers for hub registration and listing (Bond, Matter, Mysa, Other).

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::bond_sync;
use crate::db::{self, HubRegistration, MysaAccountConfig, PluginConfig};
use crate::router_client::RouterClient;
use crate::rest::AppState;
use synaptex_mysa::MysaAccount;

// ─── List hubs ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct HubDto {
    pub mac:          String,
    pub kind:         String,
    pub hub_ip:       String,
    /// Number of virtual sub-devices currently registered for this hub.
    pub device_count: usize,
}

pub async fn list_hubs(
    State(state): State<AppState>,
) -> Result<Json<Vec<HubDto>>, (StatusCode, String)> {
    let ie = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());

    // Hub registrations are the authoritative source — present even before
    // sub-device discovery completes.
    let registrations = db::list_hub_registrations(&state.trees).map_err(ie)?;

    // Count discovered sub-devices per hub.
    let configs = db::load_all_plugin_configs(&state.trees).map_err(ie)?;
    let mut bond_counts = std::collections::HashMap::<String, usize>::new();
    let mut mysa_device_count = 0usize;
    for cfg in &configs {
        match cfg {
            PluginConfig::Bond(b) => { *bond_counts.entry(b.hub_mac.clone()).or_insert(0) += 1; }
            PluginConfig::Mysa(_) => { mysa_device_count += 1; }
            _ => {}
        }
    }

    let mut dtos: Vec<HubDto> = registrations
        .into_iter()
        .map(|reg| match reg {
            HubRegistration::Bond { mac, hub_ip, .. } => HubDto {
                device_count: bond_counts.get(&mac).copied().unwrap_or(0),
                kind:         "bond".to_string(),
                hub_ip,
                mac,
            },
            HubRegistration::Mysa { username } => HubDto {
                mac:          username,
                kind:         "mysa".to_string(),
                hub_ip:       String::new(),
                device_count: mysa_device_count,
            },
            HubRegistration::Other { mac, kind, hub_ip } => HubDto {
                device_count: 0,
                mac, kind, hub_ip,
            },
        })
        .collect();

    dtos.sort_by(|a, b| a.mac.cmp(&b.mac));
    Ok(Json(dtos))
}

// ─── Register hub ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterHubBody {
    /// MAC address of the hub (AA:BB:CC:DD:EE:FF).
    /// Not required for Mysa (cloud-only; no physical hub MAC).
    #[serde(default)]
    pub mac:        String,
    /// Currently observed IP (optional; empty means unknown).
    #[serde(default)]
    pub ip:         String,
    /// Hub kind: "bond" | "mysa" | "matter" | "other".
    pub kind:       String,
    /// Bond hub serial number (bondid from GET /v2/sys/version).
    #[serde(default)]
    pub bond_id:    String,
    /// Bond local API token (BOND-Token header value).
    #[serde(default)]
    pub bond_token: String,
    /// Mysa cloud account e-mail address.
    #[serde(default)]
    pub username:   String,
    /// Mysa cloud account password.
    #[serde(default)]
    pub password:   String,
}

#[derive(Debug, Serialize)]
pub struct RegisterHubResponse {
    pub device_id:  String,
    pub managed_ip: String,
}

pub async fn register_hub(
    State(state): State<AppState>,
    Json(body):   Json<RegisterHubBody>,
) -> Result<Json<RegisterHubResponse>, (StatusCode, String)> {
    // ── Mysa cloud accounts need no router integration ────────────────────────
    if body.kind == "mysa" {
        return register_mysa_hub(state, body).await;
    }

    let cfg = state.router_client_cfg.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE,
         "router integration not configured (--router-url / --router-cert)".to_string())
    })?;

    let mut client = RouterClient::connect(cfg.clone()).await.map_err(|e| {
        (StatusCode::BAD_GATEWAY, format!("router connection failed: {e}"))
    })?;

    let resp = client.register_device(synaptex_router_proto::RegisterDeviceRequest {
        mac:        body.mac.clone(),
        ip:         body.ip.clone(),
        kind:       body.kind.clone(),
        bond_id:    body.bond_id.clone(),
        bond_token: body.bond_token.clone(),
        managed_ip: String::new(),
    }).await.map_err(|e| {
        (StatusCode::BAD_GATEWAY, format!("register_device RPC failed: {e}"))
    })?;

    // Persist the hub registration so core can rediscover sub-devices on restart.
    let hub_reg = if body.kind == "bond" {
        HubRegistration::Bond {
            mac:        body.mac.clone(),
            hub_ip:     resp.managed_ip.clone(),
            bond_token: body.bond_token.clone(),
            bond_id:    body.bond_id.clone(),
        }
    } else {
        HubRegistration::Other {
            mac:    body.mac.clone(),
            kind:   body.kind.clone(),
            hub_ip: resp.managed_ip.clone(),
        }
    };
    if let Err(e) = db::save_hub_registration(&state.trees, &hub_reg) {
        tracing::warn!(mac = %body.mac, "failed to save hub registration: {e}");
    }

    // For Bond hubs: immediately discover sub-devices and register virtual plugins.
    if body.kind == "bond" && !body.bond_token.is_empty() {
        let connect_ip = if body.ip.is_empty() { resp.managed_ip.clone() } else { body.ip.clone() };
        let hub_mac    = body.mac.clone();
        let bond_token = body.bond_token.clone();
        let managed_ip = resp.managed_ip.clone();

        let (ip1, mac1, tok1, mgd1) = (connect_ip.clone(), hub_mac.clone(), bond_token.clone(), managed_ip.clone());
        let (t1, r1, b1) = (state.trees.clone(), state.registry.clone(), state.bus_tx.clone());
        tokio::spawn(async move {
            bond_sync::sync_hub(&ip1, &mac1, &tok1, &mgd1, t1, r1, b1).await;
        });

        bond_sync::spawn_periodic_sync(
            connect_ip, hub_mac, bond_token, managed_ip,
            state.trees.clone(), state.registry.clone(), state.bus_tx.clone(),
        );
    }

    Ok(Json(RegisterHubResponse {
        device_id:  resp.device_id,
        managed_ip: resp.managed_ip,
    }))
}

// ─── Mysa cloud hub registration ─────────────────────────────────────────────

async fn register_mysa_hub(
    state: AppState,
    body:  RegisterHubBody,
) -> Result<Json<RegisterHubResponse>, (StatusCode, String)> {
    let ie = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let bad = |msg: &str| (StatusCode::BAD_REQUEST, msg.to_string());

    if body.username.is_empty() || body.password.is_empty() {
        return Err(bad("username and password are required for kind=mysa"));
    }

    // Validate credentials by attempting a fresh authentication.
    let account = MysaAccount::new(body.username.clone(), body.password.clone(), state.bus_tx.clone());
    account.ensure_auth().await.map_err(|e| {
        (StatusCode::UNAUTHORIZED, format!("Mysa authentication failed: {e}"))
    })?;

    // Persist credentials and a hub registration.
    db::save_mysa_account_config(&state.trees, &MysaAccountConfig {
        username: body.username.clone(),
        password: body.password.clone(),
    }).map_err(ie)?;

    if let Err(e) = db::save_hub_registration(&state.trees, &HubRegistration::Mysa {
        username: body.username.clone(),
    }) {
        tracing::warn!(username = %body.username, "failed to save mysa hub registration: {e}");
    }

    // Start the MQTT worker and kick off background device sync.
    account.start_mqtt_worker();
    let (t, r) = (state.trees.clone(), state.registry.clone());
    tokio::spawn(async move {
        crate::mysa_sync::sync_account(account, t, r).await;
    });

    tracing::info!(username = %body.username, "mysa: account registered");
    Ok(Json(RegisterHubResponse {
        device_id:  body.username.clone(),
        managed_ip: String::new(),
    }))
}
