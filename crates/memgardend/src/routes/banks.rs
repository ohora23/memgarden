use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use memgarden_store::banks;
use memgarden_store::models::Bank;

use crate::error::{ApiError, join_err};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct BankResponse {
    pub bank_id: String,
    pub mission: Option<String>,
    pub disposition: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<Bank> for BankResponse {
    fn from(b: Bank) -> Self {
        BankResponse {
            bank_id: b.bank_id,
            mission: b.mission,
            disposition: b.disposition,
            created_at: b.created_at,
            updated_at: b.updated_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateBankRequest {
    pub bank_id: String,
    #[serde(default)]
    pub mission: Option<String>,
    #[serde(default)]
    pub disposition: Option<String>,
}

/// `None` (key absent from the JSON body) leaves the field untouched;
/// `Some(None)` (key present, value `null`) clears it; `Some(Some(v))` sets
/// it. This relies on serde's standard handling of `Option<T>`-typed
/// fields: a missing key defaults to the outer `None` regardless of what
/// `T` is, so `Option<Option<String>>` distinguishes "absent" from
/// "explicit null" without a custom deserializer.
#[derive(Debug, Deserialize)]
pub struct PatchBankRequest {
    #[serde(default)]
    pub mission: Option<Option<String>>,
    #[serde(default)]
    pub disposition: Option<Option<String>>,
}

pub async fn list_banks(
    State(state): State<AppState>,
) -> Result<Json<Vec<BankResponse>>, ApiError> {
    let db = state.db.clone();
    let found = tokio::task::spawn_blocking(move || banks::list(&db))
        .await
        .map_err(join_err)??;
    Ok(Json(found.into_iter().map(BankResponse::from).collect()))
}

pub async fn create_bank(
    State(state): State<AppState>,
    Json(body): Json<CreateBankRequest>,
) -> Result<(StatusCode, Json<BankResponse>), ApiError> {
    if body.bank_id.is_empty() || body.bank_id.len() > 200 || body.bank_id.contains('/') {
        return Err(
            memgarden_core::Error::Invalid(format!("invalid bank_id: {:?}", body.bank_id)).into(),
        );
    }
    let db = state.db.clone();
    let created = tokio::task::spawn_blocking(move || {
        banks::create(
            &db,
            &body.bank_id,
            body.mission.as_deref(),
            body.disposition.as_deref(),
        )
    })
    .await
    .map_err(join_err)??;
    Ok((StatusCode::CREATED, Json(BankResponse::from(created))))
}

pub async fn get_bank(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
) -> Result<Json<BankResponse>, ApiError> {
    let db = state.db.clone();
    let found = tokio::task::spawn_blocking(move || banks::get(&db, &bank_id))
        .await
        .map_err(join_err)??;
    found
        .map(|b| Json(BankResponse::from(b)))
        .ok_or_else(|| ApiError::not_found("bank not found"))
}

pub async fn patch_bank(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    Json(body): Json<PatchBankRequest>,
) -> Result<Json<BankResponse>, ApiError> {
    let db = state.db.clone();
    let id = bank_id.clone();
    tokio::task::spawn_blocking(move || {
        banks::update(
            &db,
            &id,
            body.mission.as_ref().map(|m| m.as_deref()),
            body.disposition.as_ref().map(|d| d.as_deref()),
        )
    })
    .await
    .map_err(join_err)??;

    let db = state.db.clone();
    let updated = tokio::task::spawn_blocking(move || banks::get(&db, &bank_id))
        .await
        .map_err(join_err)??
        .ok_or_else(|| ApiError::not_found("bank not found"))?;
    Ok(Json(BankResponse::from(updated)))
}

pub async fn delete_bank(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let db = state.db.clone();
    let changed = tokio::task::spawn_blocking(move || banks::delete(&db, &bank_id))
        .await
        .map_err(join_err)??;
    if changed == 0 {
        return Err(ApiError::not_found("bank not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}
