use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::{
    db,
    rest::{
        dto::{CloudDeviceDto, ProbeResultDto, ResetBody, ResetMode},
        error::{ApiError, ApiResult},
        AppState,
    },
    tuya_cloud::TuyaCloudClient,
};

fn get_client(state: &AppState) -> ApiResult<TuyaCloudClient> {
    let cfg = db::get_tuya_cloud_config(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(ApiError::no_tuya_config)?;
    Ok(TuyaCloudClient::new(&cfg))
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_page")]
    page: u32,
    #[serde(default = "default_page_size")]
    size: u32,
}

fn default_page() -> u32 { 1 }
fn default_page_size() -> u32 { 20 }

pub async fn list_cloud_devices(
    State(state): State<AppState>,
    Query(q):     Query<ListQuery>,
) -> ApiResult<Json<Vec<CloudDeviceDto>>> {
    let client  = get_client(&state)?;
    let devices = client.list_devices(q.page, q.size).await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(devices.into_iter().map(CloudDeviceDto::from).collect()))
}

pub async fn get_cloud_device(
    State(state): State<AppState>,
    Path(tuya_id): Path<String>,
) -> ApiResult<Json<CloudDeviceDto>> {
    let client = get_client(&state)?;
    let device = client.get_device(&tuya_id).await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(CloudDeviceDto::from(device)))
}

pub async fn probe_device(
    State(state):  State<AppState>,
    Path(tuya_id): Path<String>,
) -> ApiResult<Json<ProbeResultDto>> {
    // Check per-device cache first.
    let device_cached = db::get_probe_result(&state.trees, &tuya_id)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if let Some(supported) = device_cached {
        return Ok(Json(ProbeResultDto { supported: Some(supported), cached: true }));
    }

    let client = get_client(&state)?;

    // Get product_id to check product-level cache.
    let device = client.get_device(&tuya_id).await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if let Some(supported) = db::get_probe_result(&state.trees, &device.product_id)
        .map_err(|e| ApiError::internal(e.to_string()))?
    {
        return Ok(Json(ProbeResultDto { supported: Some(supported), cached: true }));
    }

    // Probe = attempt soft reset; cache result by product_id.
    let supported = client.soft_reset(&tuya_id).await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if supported {
        tracing::warn!(
            tuya_id = %tuya_id,
            "probe triggered an actual soft reset — device is now in pairing mode"
        );
    }

    db::save_probe_result(&state.trees, &device.product_id, supported)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    // Also cache by device ID for quick future lookups.
    db::save_probe_result(&state.trees, &tuya_id, supported)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(ProbeResultDto { supported: Some(supported), cached: false }))
}

pub async fn reset_device(
    State(state):  State<AppState>,
    Path(tuya_id): Path<String>,
    Json(body):    Json<ResetBody>,
) -> ApiResult<StatusCode> {
    let client = get_client(&state)?;

    match body.mode {
        ResetMode::Soft => {
            let ok = client.soft_reset(&tuya_id).await
                .map_err(|e| ApiError::internal(e.to_string()))?;
            if !ok {
                return Err(ApiError::soft_reset_unsupported());
            }
        }
        ResetMode::Full => {
            client.factory_reset(&tuya_id).await
                .map_err(|e| ApiError::internal(e.to_string()))?;
        }
    }

    Ok(StatusCode::NO_CONTENT)
}
