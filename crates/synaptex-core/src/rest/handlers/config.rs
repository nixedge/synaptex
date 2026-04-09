use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use rand::Rng;

use crate::{
    db::{self, TuyaCloudConfig, TuyaRegion},
    rest::{
        dto::{ConfigDto, SetApiKeyBody, SetTuyaCloudBody, TuyaCloudInfoDto},
        error::{ApiError, ApiResult},
        AppState,
    },
    tuya_cloud::TuyaCloudClient,
};

pub async fn get_config(
    State(state): State<AppState>,
) -> ApiResult<Json<ConfigDto>> {
    let tuya = db::get_tuya_cloud_config(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let api_key = db::get_api_key(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(ConfigDto {
        tuya_cloud: tuya.map(|c| TuyaCloudInfoDto {
            client_id: c.client_id,
            region:    region_str(&c.region).into(),
        }),
        api_key_set: api_key.is_some(),
    }))
}

pub async fn put_tuya_cloud(
    State(state): State<AppState>,
    Json(body):   Json<SetTuyaCloudBody>,
) -> ApiResult<StatusCode> {
    let region = parse_region(&body.region)
        .ok_or_else(|| ApiError::bad_request("invalid region; use us|eu|cn|in"))?;

    // Resolve owner UID from the seed device now so list_devices never needs to.
    let base_url = region.base_url();
    let resolver = TuyaCloudClient::for_uid_resolution(&body.client_id, &body.client_secret, base_url);
    let uid = resolver.get_uid_for_device(&body.seed_device_id).await
        .map_err(|e| ApiError::bad_request(
            format!("could not resolve UID from seed device: {e}")
        ))?;

    let cfg = TuyaCloudConfig {
        client_id:     body.client_id,
        client_secret: body.client_secret,
        region,
        uid,
    };
    db::save_tuya_cloud_config(&state.trees, &cfg)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Always unauthenticated — used for bootstrapping the API key.
pub async fn put_api_key(
    State(state): State<AppState>,
    Json(body):   Json<SetApiKeyBody>,
) -> impl IntoResponse {
    let key = match body.key {
        Some(k) if !k.is_empty() => k,
        _ => {
            let bytes: [u8; 16] = rand::thread_rng().gen();
            hex::encode(bytes)
        }
    };
    if let Err(e) = db::save_api_key(&state.trees, &key) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ).into_response();
    }
    Json(serde_json::json!({ "key": key })).into_response()
}

pub async fn delete_api_key(
    State(state): State<AppState>,
) -> ApiResult<StatusCode> {
    db::remove_api_key(&state.trees)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

fn region_str(r: &TuyaRegion) -> &'static str {
    match r {
        TuyaRegion::Us => "us",
        TuyaRegion::Eu => "eu",
        TuyaRegion::Cn => "cn",
        TuyaRegion::In => "in",
    }
}

fn parse_region(s: &str) -> Option<TuyaRegion> {
    match s {
        "us" => Some(TuyaRegion::Us),
        "eu" => Some(TuyaRegion::Eu),
        "cn" => Some(TuyaRegion::Cn),
        "in" => Some(TuyaRegion::In),
        _    => None,
    }
}
