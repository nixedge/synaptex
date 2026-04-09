use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use synaptex_types::device::{DeviceId, DeviceInfo};
use synaptex_tuya::{plugin::TuyaConfig, TuyaPlugin};

use crate::{
    db::{self, PluginConfig},
    rest::{
        dto::{CommandDto, DeviceDto, RegisterBody, device_dto},
        error::{ApiError, ApiResult},
        AppState,
    },
    tuya_cloud::TuyaCloudClient,
};

pub async fn list_devices(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<DeviceDto>>> {
    let infos = db::list_all_devices(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let dtos = infos.iter().map(|info| {
        let st = state.cache.get(&info.id);
        let (ip, tuya_version) = db::load_plugin_config(&state.trees, &info.id)
            .ok()
            .flatten()
            .map(|cfg| match cfg {
                PluginConfig::Tuya(t) => (Some(t.ip.to_string()), t.protocol_version),
                PluginConfig::Group(_) => (None, None),
            })
            .unwrap_or((None, None));
        device_dto(info, st, ip, tuya_version)
    }).collect();

    Ok(Json(dtos))
}

pub async fn get_device(
    State(state): State<AppState>,
    Path(mac):    Path<String>,
) -> ApiResult<Json<DeviceDto>> {
    let id = DeviceId::from_mac_str(&mac)
        .map_err(|e| ApiError::bad_request(e))?;

    let info: DeviceInfo = db::get(&state.trees.registry, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("device {mac} not found")))?;

    // Try live poll; fall back to cache if offline or not registered.
    let st = match state.registry.poll_device(&id).await {
        Ok(s)  => Some(s),
        Err(_) => state.cache.get(&id),
    };

    let (ip, tuya_version) = db::load_plugin_config(&state.trees, &id)
        .ok()
        .flatten()
        .map(|cfg| match cfg {
            PluginConfig::Tuya(t) => (Some(t.ip.to_string()), t.protocol_version),
            PluginConfig::Group(_) => (None, None),
        })
        .unwrap_or((None, None));
    Ok(Json(device_dto(&info, st, ip, tuya_version)))
}

pub async fn register_device(
    State(state): State<AppState>,
    Json(body):   Json<RegisterBody>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let id = DeviceId::from_mac_str(&body.mac)
        .map_err(|e| ApiError::bad_request(e))?;

    let ip: std::net::IpAddr = body.ip.parse()
        .map_err(|_| ApiError::bad_request("invalid IP address"))?;

    let dp_profile = body.dp_profile.unwrap_or_else(|| "bulb_b".into());

    let tuya_cfg = synaptex_tuya::TuyaDeviceConfig {
        device_id:     id,
        ip,
        port:          body.port.unwrap_or(6668),
        tuya_id:       body.tuya_id.clone(),
        local_key:     body.local_key.clone(),
        dp_profile,
        dp_map:        None,
        protocol_version: None,
    };

    let info = DeviceInfo {
        id,
        name:         body.name.clone(),
        model:        body.model.clone().unwrap_or_default(),
        protocol:     "tuya_local".into(),
        capabilities: tuya_cfg.dp_map().capabilities(),
    };

    db::register_device(&state.trees, &info)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    db::save_plugin_config(&state.trees, &id, &PluginConfig::Tuya(tuya_cfg.clone()))
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let dp_map   = tuya_cfg.dp_map();
    let plugin = TuyaPlugin::new(
        info,
        TuyaConfig {
            ip:            tuya_cfg.ip,
            port:          tuya_cfg.port,
            tuya_id:       tuya_cfg.tuya_id,
            local_key:     tuya_cfg.local_key,
            dp_map,
            protocol_version: None,
        },
        state.bus_tx.clone(),
    );
    state.registry.register(Arc::new(plugin));

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "mac": body.mac }))))
}

#[derive(Deserialize)]
pub struct PatchDeviceBody {
    pub dp_profile:    Option<String>,
    pub protocol_version: Option<String>,
}

pub async fn patch_device(
    State(state): State<AppState>,
    Path(mac):    Path<String>,
    Json(body):   Json<PatchDeviceBody>,
) -> ApiResult<StatusCode> {
    let id = DeviceId::from_mac_str(&mac)
        .map_err(|e| ApiError::bad_request(e))?;

    if body.dp_profile.is_none() && body.protocol_version.is_none() {
        return Ok(StatusCode::NO_CONTENT);
    }

    let mut tuya_cfg = match db::load_plugin_config(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
    {
        Some(PluginConfig::Tuya(cfg)) => cfg,
        _ => return Err(ApiError::bad_request("device has no Tuya config")),
    };

    if let Some(dp_profile) = body.dp_profile {
        tuya_cfg.dp_profile = dp_profile;
        tuya_cfg.dp_map     = None;  // clear any override so profile takes effect
    }
    if let Some(hint) = body.protocol_version {
        tuya_cfg.protocol_version = if hint.is_empty() { None } else { Some(hint) };
    }

    let dp_map = tuya_cfg.dp_map();

    // Update capabilities in stored DeviceInfo.
    let mut info: DeviceInfo = db::get(&state.trees.registry, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("device {mac} not found")))?;
    info.capabilities = dp_map.capabilities();

    db::register_device(&state.trees, &info)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    db::save_plugin_config(&state.trees, &id, &PluginConfig::Tuya(tuya_cfg.clone()))
        .map_err(|e| ApiError::internal(e.to_string()))?;

    // Reload the live plugin with the updated config.
    state.registry.deregister(&id).await;
    let plugin = TuyaPlugin::new(
        info,
        TuyaConfig {
            ip:            tuya_cfg.ip,
            port:          tuya_cfg.port,
            tuya_id:       tuya_cfg.tuya_id,
            local_key:     tuya_cfg.local_key,
            dp_map,
            protocol_version: tuya_cfg.protocol_version,
        },
        state.bus_tx.clone(),
    );
    state.registry.register(Arc::new(plugin));

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct UnregisterQuery {
    #[serde(default)]
    factory_reset: bool,
}

pub async fn unregister_device(
    State(state): State<AppState>,
    Path(mac):    Path<String>,
    Query(q):     Query<UnregisterQuery>,
) -> ApiResult<StatusCode> {
    let id = DeviceId::from_mac_str(&mac)
        .map_err(|e| ApiError::bad_request(e))?;

    if q.factory_reset {
        // Look up the tuya_id from the stored plugin config.
        let tuya_id = match db::load_plugin_config(&state.trees, &id)
            .map_err(|e| ApiError::internal(e.to_string()))?
        {
            Some(PluginConfig::Tuya(cfg)) => cfg.tuya_id,
            _ => return Err(ApiError::bad_request("device has no Tuya config")),
        };

        let cloud_cfg = db::get_tuya_cloud_config(&state.trees)
            .map_err(|e| ApiError::internal(e.to_string()))?
            .ok_or_else(ApiError::no_tuya_config)?;

        TuyaCloudClient::new(&cloud_cfg)
            .factory_reset(&tuya_id)
            .await
            .map_err(|e| ApiError::internal(format!("Tuya Cloud factory reset failed: {e}")))?;
    }

    state.registry.deregister(&id).await;
    db::remove_device(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    db::remove_plugin_config(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Returns raw Tuya credentials for the device. Only available in dev mode (no API key set).
pub async fn device_debug_config(
    State(state): State<AppState>,
    Path(mac):    Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // Refuse if an API key is configured.
    if db::get_api_key(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .is_some()
    {
        return Err(ApiError { code: "forbidden", message: "debug-config is only available in dev mode (no API key set)".into() });
    }

    let id = DeviceId::from_mac_str(&mac)
        .map_err(|e| ApiError::bad_request(e))?;

    let cfg = db::load_plugin_config(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("no config for device {mac}")))?;

    let out = match cfg {
        PluginConfig::Tuya(t) => serde_json::json!({
            "mac":           mac,
            "tuya_id":       t.tuya_id,
            "local_key":     t.local_key,
            "ip":            t.ip.to_string(),
            "port":          t.port,
            "dp_profile":    t.dp_profile,
            "protocol":      "tuya_local",
            "protocol_version": t.protocol_version,
        }),
        PluginConfig::Group(g) => serde_json::json!({ "members": g.member_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>() }),
    };

    Ok(Json(out))
}

pub async fn device_command(
    State(state): State<AppState>,
    Path(mac):    Path<String>,
    Json(dto):    Json<CommandDto>,
) -> ApiResult<StatusCode> {
    let id = DeviceId::from_mac_str(&mac)
        .map_err(|e| ApiError::bad_request(e))?;

    use synaptex_types::capability::DeviceCommand;
    let cmd = DeviceCommand::try_from(dto)
        .map_err(ApiError::bad_request)?;

    state.registry.execute_command(&id, cmd).await
        .map_err(ApiError::from)?;

    Ok(StatusCode::NO_CONTENT)
}
