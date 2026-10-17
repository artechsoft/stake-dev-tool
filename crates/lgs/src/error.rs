use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("missing field: {0}")]
    MissingField(&'static str),
    #[error("session not found")]
    SessionNotFound,
    #[error("insufficient balance")]
    InsufficientBalance,
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("mode \"{mode}\" not found for game \"{game}\"")]
    ModeNotFound { game: String, mode: String },
    #[error("event {0} not found in weights")]
    EventNotFound(u32),
    #[error("file not found: {0}")]
    FileNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("zstd error: {0}")]
    Zstd(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
}

/// Matches the RGS error body, `{ "error": "ERR_*", "message": "..." }`, so a
/// front end's error handling behaves here as it will in production.
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            AppError::InsufficientBalance => (StatusCode::BAD_REQUEST, "ERR_IPB"),
            AppError::MissingField(_)
            | AppError::InvalidRequest(_)
            | AppError::SessionNotFound
            | AppError::ModeNotFound { .. } => (StatusCode::BAD_REQUEST, "ERR_VAL"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "ERR_GEN"),
        };
        // The cause used to travel in the response body ONLY: server-side all
        // that remained was `tower_http`'s bare "response failed", which made a
        // failing books load invisible unless someone read the client's network
        // tab. Log it here, the one place the cause is still in hand.
        if status.is_server_error() {
            tracing::error!(error = %self, "LGS request failed");
        } else {
            tracing::debug!(error = %self, "LGS request rejected");
        }
        let body = Json(ErrorBody {
            error: code,
            message: self.to_string(),
        });
        (status, body).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
