use std::{collections::HashMap, net::{IpAddr, Ipv4Addr}, sync::Arc, time::Duration};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use synaptex_types::device::{DeviceId, DeviceInfo};
use synaptex_tuya::{plugin::TuyaConfig, TuyaDeviceConfig, TuyaPlugin};

use crate::{
    db::{self, PluginConfig},
    rest::{
        dto::{
            CloudDeviceDto, ImportResultDto, ImportedDeviceDto,
            ProbeResultDto, ResetBody, ResetMode,
        },
        error::{ApiError, ApiResult},
        AppState,
    },
    tuya_cloud::{discovery, TuyaCloudClient},
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

// ─── Import ───────────────────────────────────────────────────────────────────

/// Derive a dp_profile from a Tuya category code.
fn category_to_dp_profile(category: &str) -> &'static str {
    match category {
        "cz"           => "switch",    // smart plug
        "dj"           => "bulb_b",    // smart bulb (colour)
        "fsd"          => "fan",       // fan controller
        "infrared_tv"  => "ir_type2",  // TV IR blaster
        "wnykq"        => "ir_type1",  // universal remote
        "qn"           => "switch",    // fireplace / generic switch
        _              => "switch",    // safe default
    }
}

/// Fetch all pages of cloud devices (up to 500).
async fn fetch_all_cloud_devices(
    client: &TuyaCloudClient,
) -> ApiResult<Vec<crate::tuya_cloud::CloudDevice>> {
    let mut all = Vec::new();
    let mut page = 1u32;
    loop {
        let batch = client.list_devices(page, 20).await
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let done = batch.len() < 20;
        all.extend(batch);
        if done { break; }
        page += 1;
    }
    Ok(all)
}

/// Returns true for Tuya device IDs that represent virtual/cloud-only devices
/// which have no local TCP endpoint and cannot be registered as physical devices.
///
/// Only `vdevo*` IDs are reliably virtual. The `eb*` prefix with alphanumeric
/// suffixes (e.g. `eb515acfb94961a065imdj`) are real WiFi devices using a
/// different Tuya ID scheme — do NOT exclude them.
fn is_virtual_device(id: &str) -> bool {
    id.starts_with("vdevo")
}

/// POST /api/v1/pairing/import
///
/// Fetches all Tuya Cloud devices, runs a 5-second local UDP discovery scan,
/// then registers every matched device.  For online devices not found via UDP,
/// performs a second-pass ARP probe using the IP reported by Tuya Cloud.
/// Returns a summary of results.
pub async fn import_cloud_devices(
    State(state): State<AppState>,
) -> ApiResult<Json<ImportResultDto>> {
    let client = get_client(&state)?;
    let cloud_devices = fetch_all_cloud_devices(&client).await?;

    // Run local UDP discovery.
    let discovered = discovery::discover(Duration::from_secs(5)).await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    // Build tuya_id → discovered device map.
    let discovery_map: HashMap<String, &discovery::DiscoveredDevice> =
        discovered.iter().map(|d| (d.tuya_id.clone(), d)).collect();

    // Load existing plugin configs to detect already-registered devices.
    let existing_configs = db::load_all_plugin_configs(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let registered_tuya_ids: std::collections::HashSet<String> = existing_configs
        .iter()
        .filter_map(|cfg| {
            if let PluginConfig::Tuya(t) = cfg { Some(t.tuya_id.clone()) } else { None }
        })
        .collect();

    let mut registered         = Vec::new();
    let mut already_registered = Vec::new();
    let mut not_discovered     = Vec::new();
    let mut skipped_virtual    = Vec::new();

    // Devices that are online, not registered, not found via UDP, but have a
    // cloud-reported IP — we'll try ARP-probing them in a second pass.
    for cloud_dev in cloud_devices {
        let dp_profile = category_to_dp_profile(&cloud_dev.category).to_string();

        // Already registered? Check before virtual filter so a previously
        // registered device always appears in already_registered, not skipped.
        if registered_tuya_ids.contains(&cloud_dev.id) {
            let mac = existing_configs.iter().find_map(|cfg| {
                if let PluginConfig::Tuya(t) = cfg {
                    if t.tuya_id == cloud_dev.id {
                        return Some(t.device_id.to_string());
                    }
                }
                None
            }).unwrap_or_default();
            already_registered.push(ImportedDeviceDto {
                mac,
                name:       cloud_dev.name.clone(),
                tuya_id:    cloud_dev.id.clone(),
                ip:         String::new(),
                dp_profile,
            });
            continue;
        }

        // Virtual / cloud-only device — no local TCP endpoint.
        if is_virtual_device(&cloud_dev.id) {
            skipped_virtual.push(CloudDeviceDto::from(cloud_dev));
            continue;
        }

        // Found via local UDP?
        if let Some(local) = discovery_map.get(&cloud_dev.id) {
            register_device_into_state(&state, &cloud_dev, local.ip, &local.mac,
                &dp_profile, &mut registered)?;
            continue;
        }

        // Not found locally — check the router discovery cache as fallback.
        // This covers the case where core is on a different subnet and cannot
        // broadcast-discover the device directly, but the router can.
        if let Some(router_dev) = state.router_devices.get(&cloud_dev.id) {
            tracing::info!(
                tuya_id = %cloud_dev.id,
                ip = %router_dev.ip,
                "import: using router-discovered device",
            );
            register_device_into_state(&state, &cloud_dev, router_dev.ip, &router_dev.mac,
                &dp_profile, &mut registered)?;
            continue;
        }

        // Online but not found anywhere.
        if cloud_dev.online {
            not_discovered.push(CloudDeviceDto::from(cloud_dev));
        }
    }

    Ok(Json(ImportResultDto { registered, already_registered, not_discovered, skipped_virtual }))
}

/// Register a single device into sled + plugin registry and append to `registered`.
fn register_device_into_state(
    state:      &AppState,
    cloud_dev:  &crate::tuya_cloud::CloudDevice,
    ip:         Ipv4Addr,
    mac:        &str,
    dp_profile: &str,
    registered: &mut Vec<ImportedDeviceDto>,
) -> ApiResult<()> {
    let id = DeviceId::from_mac_str(mac)
        .map_err(|e| ApiError::bad_request(e))?;

    let tuya_cfg = TuyaDeviceConfig {
        device_id:  id,
        ip:         IpAddr::V4(ip),
        port:       6668,
        tuya_id:    cloud_dev.id.clone(),
        local_key:  cloud_dev.local_key.clone(),
        dp_profile: dp_profile.to_string(),
        dp_map:     None,
    };
    let info = DeviceInfo {
        id,
        name:         cloud_dev.name.clone(),
        model:        String::new(),
        protocol:     "tuya_local".into(),
        capabilities: tuya_cfg.dp_map().capabilities(),
    };

    db::register_device(&state.trees, &info)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    db::save_plugin_config(&state.trees, &id, &PluginConfig::Tuya(tuya_cfg.clone()))
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let plugin = TuyaPlugin::new(
        info,
        TuyaConfig {
            ip:        tuya_cfg.ip,
            port:      tuya_cfg.port,
            tuya_id:   tuya_cfg.tuya_id.clone(),
            local_key: tuya_cfg.local_key.clone(),
            dp_map:    tuya_cfg.dp_map(),
        },
        state.bus_tx.clone(),
    );
    state.registry.register(Arc::new(plugin));

    registered.push(ImportedDeviceDto {
        mac:        mac.to_string(),
        name:       cloud_dev.name.clone(),
        tuya_id:    cloud_dev.id.clone(),
        ip:         ip.to_string(),
        dp_profile: dp_profile.to_string(),
    });
    Ok(())
}
