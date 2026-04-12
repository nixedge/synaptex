/// REST handlers that proxy to the synaptex-router gRPC service.

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::bond_sync;
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
        mac:        body.mac.clone(),
        ip:         body.ip.clone(),
        kind:       body.kind.clone(),
        bond_id:    body.bond_id.clone(),
        bond_token: body.bond_token.clone(),
    }).await.map_err(|e| {
        (StatusCode::BAD_GATEWAY, format!("register_device RPC failed: {e}"))
    })?;

    // For Bond hubs: immediately discover sub-devices and register virtual plugins.
    // Use the currently observed IP for the initial connect (the device may not
    // have renewed its DHCP lease to managed_ip yet).  managed_ip is stored in
    // BondConfig for all future connections.
    if body.kind == "bond" && !body.bond_token.is_empty() {
        let connect_ip = if body.ip.is_empty() { resp.managed_ip.clone() } else { body.ip.clone() };
        let hub_mac    = body.mac.clone();
        let bond_token = body.bond_token.clone();
        let managed_ip = resp.managed_ip.clone();

        // Kick off discovery in the background; don't block the HTTP response.
        let (ip1, mac1, tok1, mgd1) = (connect_ip.clone(), hub_mac.clone(), bond_token.clone(), managed_ip.clone());
        let (t1, r1, b1) = (state.trees.clone(), state.registry.clone(), state.bus_tx.clone());
        tokio::spawn(async move {
            bond_sync::sync_hub(&ip1, &mac1, &tok1, &mgd1, t1, r1, b1).await;
        });

        // Spawn the 5-minute periodic sync for this hub.
        bond_sync::spawn_periodic_sync(
            connect_ip, hub_mac, bond_token, managed_ip,
            state.trees.clone(), state.registry.clone(), state.bus_tx.clone(),
        );
    }

    Ok(Json(RegisterDeviceResponse {
        device_id:  resp.device_id,
        managed_ip: resp.managed_ip,
    }))
}
