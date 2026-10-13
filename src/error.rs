use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error, Serialize)]
#[error("{message}")]
pub struct Error {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }
    pub fn auth() -> Self {
        Self::new(
            "reauth_required",
            "Your Moodle login has expired. Run moocp login to reconnect.",
        )
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("unsupported", message)
    }
    pub fn local(error: impl std::fmt::Display) -> Self {
        Self::new("local_storage", error.to_string())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Self::local(value)
    }
}
impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::local(value)
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::new(
            "invalid_response",
            "Could not decode the expected JSON structure.",
        )
    }
}
