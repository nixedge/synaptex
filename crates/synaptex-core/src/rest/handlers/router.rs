use axum::{extract::State, Json};
use serde::Serialize;

use crate::rest::AppState;

#[derive(Serialize)]
pub struct RouterDeviceDto {
    pub tuya_id:    String,
    pub ip:         String,
    pub managed_ip: Option<String>,
    pub mac:        String,
    pub version:    String,
}

/// GET /api/v1/router/devices
///
/// Returns all Tuya devices currently known to the router discovery cache.
/// Empty when synaptex-router is not configured or no devices have been seen yet.
pub async fn list_router_devices(
    State(state): State<AppState>,
) -> Json<Vec<RouterDeviceDto>> {
    let mut devices: Vec<RouterDeviceDto> = state.router_devices
        .iter()
        .map(|entry| RouterDeviceDto {
            tuya_id:    entry.key().clone(),
            ip:         entry.value().ip.to_string(),
            managed_ip: entry.value().managed_ip.map(|a| a.to_string()),
            mac:        entry.value().mac.clone(),
            version:    entry.value().version.clone(),
        })
        .collect();
    devices.sort_by(|a, b| a.mac.cmp(&b.mac));
    Json(devices)
}
