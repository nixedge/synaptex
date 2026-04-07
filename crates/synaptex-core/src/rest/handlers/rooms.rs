use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use synaptex_types::device::{DeviceId, DeviceInfo};
use uuid::Uuid;

use crate::{
    db::{self, Room},
    rest::{
        dto::{CommandDto, CreateRoomBody, PatchRoomBody, RoomDto},
        error::{ApiError, ApiResult},
        AppState,
    },
    room,
};

fn room_to_dto(r: Room) -> RoomDto {
    RoomDto {
        id:      r.id,
        name:    r.name,
        devices: r.device_ids.iter().map(|id| id.to_string()).collect(),
    }
}

pub async fn list_rooms(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<RoomDto>>> {
    let rooms = db::list_rooms(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(rooms.into_iter().map(room_to_dto).collect()))
}

pub async fn get_room(
    State(state): State<AppState>,
    Path(id):     Path<String>,
) -> ApiResult<Json<RoomDto>> {
    let room = db::get_room(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("room {id} not found")))?;
    Ok(Json(room_to_dto(room)))
}

pub async fn create_room(
    State(state): State<AppState>,
    Json(body):   Json<CreateRoomBody>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let device_ids: Result<Vec<DeviceId>, _> = body.devices.iter()
        .map(|m| DeviceId::from_mac_str(m).map_err(|e| ApiError::bad_request(e)))
        .collect();
    let device_ids = device_ids?;

    for &did in &device_ids {
        let exists = db::get::<DeviceInfo>(&state.trees.registry, &did)
            .map_err(|e| ApiError::internal(e.to_string()))?
            .is_some();
        if !exists {
            return Err(ApiError::not_found(format!("device {did} not found")));
        }
    }

    let room_id = Uuid::new_v4().to_string();
    let room = Room { id: room_id.clone(), name: body.name, device_ids };
    db::save_room(&state.trees, &room)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": room_id }))))
}

pub async fn patch_room(
    State(state): State<AppState>,
    Path(id):     Path<String>,
    Json(body):   Json<PatchRoomBody>,
) -> ApiResult<StatusCode> {
    let mut room = db::get_room(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("room {id} not found")))?;

    if let Some(name) = body.name {
        room.name = name;
    }

    if let Some(devices) = body.devices {
        let device_ids: Result<Vec<DeviceId>, _> = devices.iter()
            .map(|m| DeviceId::from_mac_str(m).map_err(|e| ApiError::bad_request(e)))
            .collect();
        let device_ids = device_ids?;

        for &did in &device_ids {
            let exists = db::get::<DeviceInfo>(&state.trees.registry, &did)
                .map_err(|e| ApiError::internal(e.to_string()))?
                .is_some();
            if !exists {
                return Err(ApiError::not_found(format!("device {did} not found")));
            }
        }
        room.device_ids = device_ids;
    }

    db::save_room(&state.trees, &room)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_room(
    State(state): State<AppState>,
    Path(id):     Path<String>,
) -> ApiResult<StatusCode> {
    db::remove_room(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn room_command(
    State(state): State<AppState>,
    Path(id):     Path<String>,
    Json(dto):    Json<CommandDto>,
) -> ApiResult<StatusCode> {
    use synaptex_types::capability::DeviceCommand;

    let room = db::get_room(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("room {id} not found")))?;

    let cmd = DeviceCommand::try_from(dto)
        .map_err(ApiError::bad_request)?;

    room::execute_room_command(&room, cmd, &state.registry)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}
