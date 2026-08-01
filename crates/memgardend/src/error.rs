use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use memgarden_core::Error;

/// JSON error envelope: `{"error": {"code": "...", "message": "..."}}`.
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": { "code": self.code, "message": self.message } }));
        (self.status, body).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        match e {
            Error::NotFound(message) => ApiError {
                status: StatusCode::NOT_FOUND,
                code: "not_found",
                message,
            },
            Error::Invalid(message) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid",
                message,
            },
            Error::Conflict(message) => ApiError {
                status: StatusCode::CONFLICT,
                code: "conflict",
                message,
            },
            Error::Storage(message) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "storage",
                message,
            },
            Error::Config(message) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "config",
                message,
            },
            Error::Io { path, source } => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "io",
                message: format!("{}: {source}", path.display()),
            },
        }
    }
}

/// A `tokio::task::JoinError` from a `spawn_blocking` call (e.g. a panic in
/// the blocking closure) surfaces as a 500.
pub fn join_err(e: tokio::task::JoinError) -> ApiError {
    ApiError::internal(format!("task join error: {e}"))
}
