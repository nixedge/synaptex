use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use synaptex_types::device::{DeviceId, DeviceInfo};
use synaptex_tuya::{plugin::TuyaConfig, TuyaPlugin};

use crate::{
    db::{self, PluginConfig},
    rest::{
        dto::{CommandDto, DeviceDto, RegisterBody, device_dto},
        error::{ApiError, ApiResult},
        AppState,
    },
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
                PluginConfig::Tuya(t) => (Some(t.ip.to_string()), t.protocol_hint),
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

    let st = state.cache.get(&id);
    let (ip, tuya_version) = db::load_plugin_config(&state.trees, &id)
        .ok()
        .flatten()
        .map(|cfg| match cfg {
            PluginConfig::Tuya(t) => (Some(t.ip.to_string()), t.protocol_hint),
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
        protocol_hint: None,
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
            protocol_hint: None,
        },
        state.bus_tx.clone(),
    );
    state.registry.register(Arc::new(plugin));

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "mac": body.mac }))))
}

pub async fn unregister_device(
    State(state): State<AppState>,
    Path(mac):    Path<String>,
) -> ApiResult<StatusCode> {
    let id = DeviceId::from_mac_str(&mac)
        .map_err(|e| ApiError::bad_request(e))?;

    state.registry.deregister(&id).await;
    db::remove_device(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    db::remove_plugin_config(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
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
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}
