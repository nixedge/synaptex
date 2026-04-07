use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub code:    &'static str,
    pub message: String,
}

impl ApiError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        ApiError { code: "not_found", message: msg.into() }
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        ApiError { code: "bad_request", message: msg.into() }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        ApiError { code: "internal", message: msg.into() }
    }
    pub fn no_tuya_config() -> Self {
        ApiError {
            code: "no_tuya_config",
            message: "Tuya Cloud credentials not configured".into(),
        }
    }
    pub fn soft_reset_unsupported() -> Self {
        ApiError {
            code: "soft_reset_unsupported",
            message: "Device firmware does not support soft reset".into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.code {
            "not_found"              => StatusCode::NOT_FOUND,
            "bad_request"            => StatusCode::BAD_REQUEST,
            "soft_reset_unsupported" => StatusCode::UNPROCESSABLE_ENTITY,
            _                        => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
