use thiserror::Error;

#[derive(Debug, Error)]
pub enum TuyaError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("cipher error: {0}")]
    Cipher(String),

    #[error("protocol framing error: {0}")]
    Protocol(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("device offline or connection refused")]
    Offline,
}

impl From<TuyaError> for synaptex_types::plugin::PluginError {
    fn from(e: TuyaError) -> Self {
        match e {
            TuyaError::Io(e)           => Self::Io(e),
            TuyaError::Cipher(s)       => Self::Cipher(s),
            TuyaError::Protocol(s)     => Self::Protocol(s),
            TuyaError::Json(e)         => Self::Protocol(e.to_string()),
            TuyaError::Offline         => Self::Unreachable("device offline".into()),
        }
    }
}
