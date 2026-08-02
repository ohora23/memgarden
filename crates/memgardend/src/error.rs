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

    /// An upstream dependency (Ollama) answered but with garbage or a
    /// permanent error — 502: not our bug (500) and not worth blind retries
    /// (503).
    pub fn bad_gateway(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::BAD_GATEWAY,
            code: "upstream_error",
            message: message.into(),
        }
    }

    /// The bounded retain queue is full — 429, not 503: the daemon is
    /// healthy, the caller is simply ahead of the background worker and
    /// should back off. A full queue must never mean unbounded RAM growth.
    pub fn too_many_requests(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "queue_full",
            message: message.into(),
        }
    }

    /// A dependency (e.g. the embedding model) isn't ready yet — 503, not
    /// 500: retrying shortly is the correct client behavior (decision #1).
    pub fn unavailable(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "unavailable",
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
