use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
    Json,
};

use super::error::ApiError;
use crate::db;

/// Axum middleware that enforces Bearer token auth when an API key is stored.
/// If no key is configured the request passes through (dev/open mode).
pub async fn bearer_auth(
    State(trees): State<std::sync::Arc<crate::db::Trees>>,
    request:      Request,
    next:         Next,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let configured_key = db::get_api_key(&trees).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::internal(e.to_string())),
        )
    })?;

    // No key configured → open mode.
    let Some(expected) = configured_key else {
        return Ok(next.run(request).await);
    };

    // Key configured → require Authorization: Bearer <token>.
    let provided = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
            Ok(next.run(request).await)
        }
        _ => Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiError { code: "unauthorized", message: "invalid or missing Bearer token".into() }),
        )),
    }
}

/// Constant-time byte-slice comparison to prevent timing attacks.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
