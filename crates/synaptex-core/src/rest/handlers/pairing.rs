use std::{net::{IpAddr, Ipv4Addr}, sync::Arc};

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use synaptex_types::device::{DeviceId, DeviceInfo};
use synaptex_tuya::{plugin::TuyaConfig, TuyaDeviceConfig, TuyaPlugin};

use crate::{
    db::{self, PluginConfig},
    rest::{
        dto::{CloudDeviceDto, ImportResultDto, ImportedDeviceDto},
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

    // Load existing plugin configs to detect already-registered devices.
    let existing_configs = db::load_all_plugin_configs(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let registered_tuya_ids: std::collections::HashSet<String> = existing_configs
        .iter()
        .filter_map(|cfg| {
            if let PluginConfig::Tuya(t) = cfg { Some(t.tuya_id.clone()) } else { None }
        })
        .collect();

    let mut registered           = Vec::new();
    let mut updated_registration = Vec::new();
    let mut already_registered   = Vec::new();
    let mut not_discovered       = Vec::new();
    let mut skipped_virtual      = Vec::new();

    // Devices that are online, not registered, not found via UDP, but have a
    // cloud-reported IP — we'll try ARP-probing them in a second pass.
    for cloud_dev in cloud_devices {
        let dp_profile = category_to_dp_profile(&cloud_dev.category).to_string();

        // Already registered? Sync name + local_key from cloud, then skip re-registration.
        if registered_tuya_ids.contains(&cloud_dev.id) {
            let existing_cfg = existing_configs.iter().find_map(|cfg| {
                if let PluginConfig::Tuya(t) = cfg {
                    if t.tuya_id == cloud_dev.id { return Some(t.clone()); }
                }
                None
            });

            if let Some(mut cfg) = existing_cfg {
                let id = cfg.device_id;
                let mut key_changed  = false;
                let mut name_changed = false;

                // Sync name in registry (list_devices reads sled, so this is immediately live).
                if let Ok(Some(mut info)) = db::get::<DeviceInfo>(&state.trees.registry, &id) {
                    if info.name != cloud_dev.name {
                        tracing::info!(
                            tuya_id = %cloud_dev.id,
                            old_name = %info.name,
                            new_name = %cloud_dev.name,
                            "import: syncing device name from cloud",
                        );
                        info.name = cloud_dev.name.clone();
                        match db::register_device(&state.trees, &info) {
                            Ok(()) => name_changed = true,
                            Err(e) => tracing::warn!(tuya_id = %cloud_dev.id, "import: failed to sync name: {e}"),
                        }
                    }
                }

                // Sync local_key — update sled and flag for plugin reconnect.
                tracing::debug!(
                    tuya_id     = %cloud_dev.id,
                    key_empty   = cloud_dev.local_key.is_empty(),
                    key_changed = (cfg.local_key != cloud_dev.local_key),
                    "import: local_key check for already-registered device",
                );
                if !cloud_dev.local_key.is_empty() && cfg.local_key != cloud_dev.local_key {
                    tracing::info!(tuya_id = %cloud_dev.id, "import: syncing local_key from cloud");
                    cfg.local_key = cloud_dev.local_key.clone();
                    match db::save_plugin_config(&state.trees, &id, &PluginConfig::Tuya(cfg.clone())) {
                        Ok(()) => key_changed = true,
                        Err(e) => tracing::warn!(tuya_id = %cloud_dev.id, "import: failed to sync local_key: {e}"),
                    }
                }

                // If the local_key changed, rebuild the plugin so it reconnects with the new key.
                if key_changed {
                    // Re-read the current DeviceInfo (may have just had its name updated).
                    if let Ok(Some(info)) = db::get::<DeviceInfo>(&state.trees.registry, &id) {
                        let dp_map = cfg.dp_map();
                        let new_plugin = TuyaPlugin::new(
                            info,
                            TuyaConfig {
                                ip:            cfg.ip,
                                port:          cfg.port,
                                tuya_id:       cfg.tuya_id.clone(),
                                local_key:     cfg.local_key.clone(),
                                dp_map,
                                protocol_version: cfg.protocol_version.clone(),
                            },
                            state.bus_tx.clone(),
                        );
                        state.registry.deregister(&id).await;
                        state.registry.register(Arc::new(new_plugin));
                    }
                }

                let dto = ImportedDeviceDto {
                    mac:     id.to_string(),
                    name:    cloud_dev.name.clone(),
                    tuya_id: cloud_dev.id.clone(),
                    ip:      String::new(),
                    dp_profile,
                };
                if key_changed || name_changed {
                    updated_registration.push(dto);
                } else {
                    already_registered.push(dto);
                }
            }
            continue;
        }

        // Virtual / cloud-only device — no local TCP endpoint.
        if is_virtual_device(&cloud_dev.id) {
            skipped_virtual.push(CloudDeviceDto::from(cloud_dev));
            continue;
        }

        // Look up this device in the router's continuously-updated discovery map.
        if let Some(router_dev) = state.router_devices.get(&cloud_dev.id) {
            let hint = (!router_dev.version.is_empty()).then(|| router_dev.version.clone());
            tracing::info!(
                tuya_id = %cloud_dev.id,
                ip      = %router_dev.ip,
                version = ?hint,
                "import: using router-discovered device",
            );
            register_device_into_state(&state, &cloud_dev, router_dev.ip, &router_dev.mac,
                &dp_profile, hint, &mut registered)?;
            continue;
        }

        // Online but not found anywhere.
        if cloud_dev.online {
            not_discovered.push(CloudDeviceDto::from(cloud_dev));
        }
    }

    Ok(Json(ImportResultDto { registered, updated_registration, already_registered, not_discovered, skipped_virtual }))
}

/// Register a single device into sled + plugin registry and append to `registered`.
fn register_device_into_state(
    state:          &AppState,
    cloud_dev:      &crate::tuya_cloud::CloudDevice,
    ip:             Ipv4Addr,
    mac:            &str,
    dp_profile:     &str,
    protocol_version:  Option<String>,
    registered:     &mut Vec<ImportedDeviceDto>,
) -> ApiResult<()> {
    let id = DeviceId::from_mac_str(mac)
        .map_err(|e| ApiError::bad_request(e))?;

    let tuya_cfg = TuyaDeviceConfig {
        device_id:     id,
        ip:            IpAddr::V4(ip),
        port:          6668,
        tuya_id:       cloud_dev.id.clone(),
        local_key:     cloud_dev.local_key.clone(),
        dp_profile:    dp_profile.to_string(),
        dp_map:        None,
        protocol_version: protocol_version.clone(),
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
            ip:            tuya_cfg.ip,
            port:          tuya_cfg.port,
            tuya_id:       tuya_cfg.tuya_id.clone(),
            local_key:     tuya_cfg.local_key.clone(),
            dp_map:        tuya_cfg.dp_map(),
            protocol_version,
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
