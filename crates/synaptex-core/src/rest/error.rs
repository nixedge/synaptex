use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use synaptex_types::plugin::PluginError;

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
    pub fn unavailable(msg: impl Into<String>) -> Self {
        ApiError { code: "unavailable", message: msg.into() }
    }
    pub fn unprocessable(msg: impl Into<String>) -> Self {
        ApiError { code: "unprocessable", message: msg.into() }
    }
    pub fn no_tuya_config() -> Self {
        ApiError {
            code: "no_tuya_config",
            message: "Tuya Cloud credentials not configured".into(),
        }
    }
}

impl From<PluginError> for ApiError {
    fn from(e: PluginError) -> Self {
        match e {
            PluginError::Unreachable(msg)  => ApiError::unavailable(msg),
            PluginError::UnsupportedCommand => ApiError::unprocessable("command not supported by this device"),
            other                          => ApiError::internal(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.code {
            "not_found"     => StatusCode::NOT_FOUND,
            "bad_request"   => StatusCode::BAD_REQUEST,
            "unavailable"   => StatusCode::SERVICE_UNAVAILABLE,
            "unprocessable" => StatusCode::UNPROCESSABLE_ENTITY,
            _               => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
