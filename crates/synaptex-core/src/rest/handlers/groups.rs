use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use synaptex_types::device::{DeviceId, DeviceInfo};

use crate::{
    db::{self, GroupConfig, PluginConfig},
    group::{self, GroupPlugin},
    rest::{
        dto::{CreateGroupBody, GroupDto, PatchGroupBody},
        error::{ApiError, ApiResult},
        AppState,
    },
};

pub async fn list_groups(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<GroupDto>>> {
    let infos = db::list_all_devices(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let mut dtos = Vec::new();
    for info in &infos {
        if info.protocol != "group" { continue; }
        let cfg: Option<PluginConfig> = db::get(&state.trees.configs, &info.id)
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let members = match cfg {
            Some(PluginConfig::Group(g)) => g.member_ids.iter().map(|id| id.to_string()).collect(),
            _ => vec![],
        };
        dtos.push(GroupDto {
            mac:     info.id.to_string(),
            name:    info.name.clone(),
            model:   info.model.clone(),
            members,
        });
    }

    Ok(Json(dtos))
}

pub async fn create_group(
    State(state): State<AppState>,
    Json(body):   Json<CreateGroupBody>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    if body.members.is_empty() {
        return Err(ApiError::bad_request("members cannot be empty"));
    }

    let member_ids: Result<Vec<DeviceId>, _> = body.members.iter()
        .map(|m| DeviceId::from_mac_str(m).map_err(|e| ApiError::bad_request(e)))
        .collect();
    let member_ids = member_ids?;

    // Capability union.
    let mut capabilities = Vec::new();
    for &mid in &member_ids {
        let info: DeviceInfo = db::get(&state.trees.registry, &mid)
            .map_err(|e| ApiError::internal(e.to_string()))?
            .ok_or_else(|| ApiError::not_found(format!("member {} not found", mid)))?;
        for cap in info.capabilities {
            if !capabilities.contains(&cap) {
                capabilities.push(cap);
            }
        }
    }

    let group_id = group::new_group_id();
    let info = DeviceInfo {
        id:           group_id,
        name:         body.name.clone(),
        model:        body.model.clone().unwrap_or_default(),
        protocol:     "group".into(),
        capabilities: capabilities.clone(),
    };

    db::register_device(&state.trees, &info)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    db::save_plugin_config(
        &state.trees,
        &group_id,
        &PluginConfig::Group(GroupConfig { device_id: group_id, member_ids: member_ids.clone() }),
    ).map_err(|e| ApiError::internal(e.to_string()))?;

    let plugin = GroupPlugin::new(
        info,
        member_ids,
        state.registry.clone(),
        state.cache.clone(),
        state.bus_tx.clone(),
    );
    state.registry.register(Arc::new(plugin));

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "mac": group_id.to_string() }))))
}

pub async fn patch_group(
    State(state): State<AppState>,
    Path(mac):    Path<String>,
    Json(body):   Json<PatchGroupBody>,
) -> ApiResult<StatusCode> {
    let group_id = DeviceId::from_mac_str(&mac)
        .map_err(|e| ApiError::bad_request(e))?;

    let mut info: DeviceInfo = db::get(&state.trees.registry, &group_id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("group {mac} not found")))?;

    if let Some(name) = body.name {
        info.name = name;
    }

    let member_ids = if let Some(members) = body.members {
        let ids: Result<Vec<DeviceId>, _> = members.iter()
            .map(|m| DeviceId::from_mac_str(m).map_err(|e| ApiError::bad_request(e)))
            .collect();
        let ids = ids?;

        let mut capabilities = Vec::new();
        for &mid in &ids {
            let minfo: DeviceInfo = db::get(&state.trees.registry, &mid)
                .map_err(|e| ApiError::internal(e.to_string()))?
                .ok_or_else(|| ApiError::not_found(format!("member {mid} not found")))?;
            for cap in minfo.capabilities {
                if !capabilities.contains(&cap) {
                    capabilities.push(cap);
                }
            }
        }
        info.capabilities = capabilities;
        ids
    } else {
        let cfg: PluginConfig = db::get(&state.trees.configs, &group_id)
            .map_err(|e| ApiError::internal(e.to_string()))?
            .ok_or_else(|| ApiError::not_found("group config not found"))?;
        match cfg {
            PluginConfig::Group(g) => g.member_ids,
            _ => return Err(ApiError::internal("expected group config")),
        }
    };

    db::register_device(&state.trees, &info)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    db::save_plugin_config(
        &state.trees,
        &group_id,
        &PluginConfig::Group(GroupConfig { device_id: group_id, member_ids: member_ids.clone() }),
    ).map_err(|e| ApiError::internal(e.to_string()))?;

    state.registry.deregister(&group_id).await;
    let plugin = GroupPlugin::new(
        info,
        member_ids,
        state.registry.clone(),
        state.cache.clone(),
        state.bus_tx.clone(),
    );
    state.registry.register(Arc::new(plugin));

    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_group(
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
