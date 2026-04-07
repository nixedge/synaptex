use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;

use crate::{
    db::{self, Routine},
    rest::{
        dto::{RoutineBody, RoutineDto, RoutineStepDto, routine_to_dto},
        error::{ApiError, ApiResult},
        AppState,
    },
};

pub async fn list_routines(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<RoutineDto>>> {
    let routines = db::list_routines(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(routines.iter().map(routine_to_dto).collect()))
}

pub async fn get_routine(
    State(state): State<AppState>,
    Path(id):     Path<String>,
) -> ApiResult<Json<RoutineDto>> {
    let routine = db::get_routine(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("routine {id} not found")))?;
    Ok(Json(routine_to_dto(&routine)))
}

pub async fn create_routine(
    State(state): State<AppState>,
    Json(body):   Json<RoutineBody>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let steps = convert_steps(body.steps)?;
    let id     = Uuid::new_v4().to_string();
    let routine = Routine {
        id:       id.clone(),
        name:     body.name,
        schedule: body.schedule,
        steps,
    };
    let has_schedule = routine.schedule.is_some();

    db::save_routine(&state.trees, &routine)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if has_schedule {
        state.routine_runner
            .start_cron(routine, state.registry.clone(), state.trees.clone())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

pub async fn put_routine(
    State(state): State<AppState>,
    Path(id):     Path<String>,
    Json(body):   Json<RoutineBody>,
) -> ApiResult<StatusCode> {
    let _ = db::get_routine(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("routine {id} not found")))?;

    let steps   = convert_steps(body.steps)?;
    let routine = Routine {
        id:       id.clone(),
        name:     body.name,
        schedule: body.schedule.clone(),
        steps,
    };

    state.routine_runner.stop_cron(&id);
    db::save_routine(&state.trees, &routine)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if routine.schedule.is_some() {
        state.routine_runner
            .start_cron(routine, state.registry.clone(), state.trees.clone())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }

    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_routine(
    State(state): State<AppState>,
    Path(id):     Path<String>,
) -> ApiResult<StatusCode> {
    state.routine_runner.stop_cron(&id);
    state.routine_runner.cancel(&id);
    db::remove_routine(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn trigger_routine(
    State(state): State<AppState>,
    Path(id):     Path<String>,
) -> ApiResult<StatusCode> {
    let routine = db::get_routine(&state.trees, &id)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found(format!("routine {id} not found")))?;
    state.routine_runner.trigger(routine, state.registry.clone(), state.trees.clone());
    Ok(StatusCode::NO_CONTENT)
}

pub async fn cancel_routine(
    State(state): State<AppState>,
    Path(id):     Path<String>,
) -> ApiResult<StatusCode> {
    state.routine_runner.cancel(&id);
    Ok(StatusCode::NO_CONTENT)
}

fn convert_steps(
    steps: Vec<RoutineStepDto>,
) -> ApiResult<Vec<crate::db::RoutineStep>> {
    steps.into_iter()
        .map(|s| crate::db::RoutineStep::try_from(s)
            .map_err(|e| ApiError::bad_request(e)))
        .collect()
}
