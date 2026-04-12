/// REST handlers that proxy to the synaptex-router gRPC service.

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::router_client::RouterClient;
use crate::rest::AppState;

// ─── Register device ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterDeviceBody {
    /// MAC address of the device (AA:BB:CC:DD:EE:FF).
    pub mac:        String,
    /// Currently observed IP (optional; empty means unknown).
    #[serde(default)]
    pub ip:         String,
    /// Device kind: "bond" | "matter" | "other".
    pub kind:       String,
    /// Bond hub serial number (bondid from GET /v2/sys/version).
    #[serde(default)]
    pub bond_id:    String,
    /// Bond local API token (BOND-Token header value).
    #[serde(default)]
    pub bond_token: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterDeviceResponse {
    pub device_id:  String,
    pub managed_ip: String,
}

pub async fn register_device(
    State(state): State<AppState>,
    Json(body):   Json<RegisterDeviceBody>,
) -> Result<Json<RegisterDeviceResponse>, (StatusCode, String)> {
    let cfg = state.router_client_cfg.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE,
         "router integration not configured (--router-url / --router-cert)".to_string())
    })?;

    let mut client = RouterClient::connect(cfg.clone()).await.map_err(|e| {
        (StatusCode::BAD_GATEWAY, format!("router connection failed: {e}"))
    })?;

    let resp = client.register_device(synaptex_router_proto::RegisterDeviceRequest {
        mac:        body.mac,
        ip:         body.ip,
        kind:       body.kind,
        bond_id:    body.bond_id,
        bond_token: body.bond_token,
    }).await.map_err(|e| {
        (StatusCode::BAD_GATEWAY, format!("register_device RPC failed: {e}"))
    })?;

    Ok(Json(RegisterDeviceResponse {
        device_id:  resp.device_id,
        managed_ip: resp.managed_ip,
    }))
}
