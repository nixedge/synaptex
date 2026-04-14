use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use synaptex_types::device::{DeviceId, DeviceInfo};
use synaptex_tuya::{plugin::TuyaConfig, TuyaPlugin};

use crate::{
    db::{self, PluginConfig},
    rest::{
        dto::{CommandDto, DeviceDto, RegisterBody, device_dto},
        error::{ApiError, ApiResult},
        AppState,
    },
    router_client::RouterClient,
    tuya_cloud::TuyaCloudClient,
};

/// Build per-member `DeviceDto`s from cache for a group.
fn member_dtos(member_ids: &[synaptex_types::device::DeviceId], state: &AppState) -> Vec<DeviceDto> {
    member_ids.iter().filter_map(|mid| {
        let mut info: DeviceInfo = db::get(&state.trees.registry, mid).ok()??;
        let st  = state.cache.get(mid);
        let cfg = db::load_plugin_config(&state.trees, mid).ok().flatten();
        let (ip, ver) = cfg.as_ref()
            .map(|cfg| match cfg {
                PluginConfig::Tuya(t) => (Some(t.ip.to_string()), t.protocol_version.clone()),
                PluginConfig::Bond(b) => (Some(b.hub_ip.clone()), None),
                PluginConfig::Group(_) => (None, None),
            })
            .unwrap_or((None, None));
        if let Some(PluginConfig::Tuya(ref t)) = cfg {
            info.capabilities = t.dp_map().capabilities();
        }
        Some(device_dto(&info, st, ip, ver))
    }).collect()
}

pub async fn list_devices(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<DeviceDto>>> {
    let infos = db::list_all_devices(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let dtos = infos.iter().map(|info| {
        let st  = state.cache.get(&info.id);
        let cfg = db::load_plugin_config(&state.trees, &info.id).ok().flatten();
        let (ip, tuya_version) = cfg.as_ref()
            .map(|cfg| match cfg {
                PluginConfig::Tuya(t) => (Some(t.ip.to_string()), t.protocol_version.clone()),
                PluginConfig::Bond(b) => (Some(b.hub_ip.clone()), None),
                PluginConfig::Group(_) => (None, None),
            })
            .unwrap_or((None, None));
        let mut info = info.clone();
        if let Some(PluginConfig::Tuya(ref t)) = cfg {
            info.capabilities = t.dp_map().capabilities();
        }
        device_dto(&info, st, ip, tuya_version)
    }).collect();

    Ok(Json(dtos))
}

pub async fn get_device(
    State(state): State<AppState>,
    Path(mac):    Path<String>,
) -> ApiResult<Json<DeviceDto>> {
    let id = DeviceId::from_mac_str(&mac)
        .map_err(|e| ApiError::bad_request(e))?;

    let mut info: DeviceInfo = db::get(&state.trees.registry, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("device {mac} not found")))?;

    // Try live poll; fall back to cache if offline or not registered.
    let st = match state.registry.poll_device(&id).await {
        Ok(s)  => Some(s),
        Err(_) => state.cache.get(&id),
    };

    let cfg = db::load_plugin_config(&state.trees, &id)
        .ok()
        .flatten();

    let (ip, tuya_version) = cfg.as_ref()
        .map(|cfg| match cfg {
            PluginConfig::Tuya(t) => (Some(t.ip.to_string()), t.protocol_version.clone()),
            PluginConfig::Bond(b) => (Some(b.hub_ip.clone()), None),
            PluginConfig::Group(_) => (None, None),
        })
        .unwrap_or((None, None));

    let members = match &cfg {
        Some(PluginConfig::Group(g)) => Some(member_dtos(&g.member_ids, &state)),
        _ => None,
    };

    if let Some(PluginConfig::Tuya(ref t)) = cfg {
        info.capabilities = t.dp_map().capabilities();
    }
    let mut dto = device_dto(&info, st, ip, tuya_version);
    dto.members = members;
    Ok(Json(dto))
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
        PluginConfig::Bond(b) => serde_json::json!({
            "hub_mac":        b.hub_mac,
            "hub_ip":         b.hub_ip,
            "bond_device_id": b.bond_device_id,
            "device_type":    b.device_type,
            "name":           b.name,
            "actions":        b.actions,
            "protocol":       "bond_local",
        }),
    };

    Ok(Json(out))
}

// ─── Register managed (cloud / observe-only) device ──────────────────────────

#[derive(Deserialize)]
pub struct RegisterManagedBody {
    /// MAC address "AA:BB:CC:DD:EE:FF".
    pub mac:  String,
    /// Human-readable name, e.g. "Bedroom Thermostat".
    pub name: String,
    /// Device kind: "mysa" | "roku" | "sense" | ...
    pub kind: String,
    /// Currently observed IP (optional).
    #[serde(default)]
    pub ip:         String,
    /// Pin to a specific managed IP instead of auto-allocating (optional).
    #[serde(default)]
    pub managed_ip: String,
}

#[derive(Serialize)]
pub struct RegisterManagedResponse {
    mac:        String,
    managed_ip: String,
}

/// Register a device that has no local protocol support (cloud-controlled or
/// observe-only).  Core writes a `DeviceInfo` to the registry so the device
/// appears in `GET /api/v1/devices`, and the router allocates a managed IP.
pub async fn register_managed_device(
    State(state): State<AppState>,
    Json(body):   Json<RegisterManagedBody>,
) -> ApiResult<(StatusCode, Json<RegisterManagedResponse>)> {
    let id = DeviceId::from_mac_str(&body.mac)
        .map_err(|e| ApiError::bad_request(e))?;

    let cfg = state.router_client_cfg.as_ref().ok_or_else(|| {
        ApiError::unavailable("router integration not configured (--router-url / --router-cert)")
    })?;

    let mut client = RouterClient::connect(cfg.clone()).await
        .map_err(|e| ApiError::internal(format!("router connection failed: {e}")))?;

    let resp = client.register_device(synaptex_router_proto::RegisterDeviceRequest {
        mac:        body.mac.clone(),
        ip:         body.ip.clone(),
        kind:       body.kind.clone(),
        bond_id:    String::new(),
        bond_token: String::new(),
        managed_ip: body.managed_ip.clone(),
    }).await.map_err(|e| ApiError::internal(format!("register_device RPC failed: {e}")))?;

    // Write DeviceInfo to the registry tree — no PluginConfig, so no plugin is
    // loaded at startup.  Device appears in listings with state: null until a
    // cloud plugin is added.
    let info = DeviceInfo {
        id,
        name:         body.name.clone(),
        model:        String::new(),
        protocol:     body.kind.clone(),
        capabilities: vec![],
    };
    db::register_device(&state.trees, &info)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    tracing::info!(mac = %body.mac, kind = %body.kind, managed_ip = %resp.managed_ip,
        "managed device registered");

    Ok((StatusCode::CREATED, Json(RegisterManagedResponse {
        mac:        body.mac,
        managed_ip: resp.managed_ip,
    })))
}

pub async fn device_command(
    State(state): State<AppState>,
    Path(mac):    Path<String>,
    Json(dto):    Json<CommandDto>,
) -> ApiResult<axum::response::Response> {
    let id = DeviceId::from_mac_str(&mac)
        .map_err(|e| ApiError::bad_request(e))?;

    use synaptex_types::capability::DeviceCommand;
    let cmd = DeviceCommand::try_from(dto)
        .map_err(ApiError::bad_request)?;

    state.registry.execute_command(&id, cmd).await
        .map_err(ApiError::from)?;

    // For groups return per-member state so callers can see each device's result.
    let cfg = db::load_plugin_config(&state.trees, &id).ok().flatten();
    if let Some(PluginConfig::Group(g)) = cfg {
        let info: DeviceInfo = db::get(&state.trees.registry, &id)
            .map_err(|e| ApiError::internal(e.to_string()))?
            .ok_or_else(|| ApiError::not_found(format!("device {mac} not found")))?;
        let st = state.cache.get(&id);
        let mut dto = device_dto(&info, st, None, None);
        dto.members = Some(member_dtos(&g.member_ids, &state));
        return Ok((StatusCode::OK, Json(dto)).into_response());
    }

    Ok(StatusCode::NO_CONTENT.into_response())
}
