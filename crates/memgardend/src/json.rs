//! `ApiJson<T>` — `axum::Json` with MemGarden's error envelope.
//!
//! Review LOW (PR #8): a request missing a required field came back as
//! axum's own 422 with a `text/plain` body, so a client that parses
//! `{"error":{"code","message"}}` — which every other failure on every route
//! returns — got an unparseable response for the most common mistake there
//! is. Malformed input is a 400 `invalid`, like the hand-written validation
//! checks it sits next to.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;

use crate::error::ApiError;

pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    axum::Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(ApiJson(value)),
            // `body_text()` keeps serde's own message ("missing field
            // `is_initial` at line 1 column 42"), which is the only part of
            // the rejection a caller can act on.
            //
            // The rejection's own status is preserved — 413 for a body over
            // the route's limit, 415 for the wrong content type are real,
            // distinct conditions — with the single exception of axum's 422
            // for a well-formed-JSON-but-wrong-shape body, which MemGarden
            // reports as 400 `invalid` like every other validation failure.
            Err(rejection) => {
                let status = rejection.status();
                Err(if status == StatusCode::UNPROCESSABLE_ENTITY {
                    memgarden_core::Error::Invalid(rejection.body_text()).into()
                } else {
                    ApiError::new(status, "invalid", rejection.body_text())
                })
            }
        }
    }
}
