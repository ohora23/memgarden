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

#[derive(Debug, Deserialize)]
pub struct PatchBankRequest {
    #[serde(default)]
    pub mission: Option<String>,
    #[serde(default)]
    pub disposition: Option<String>,
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
            body.mission.as_deref(),
            body.disposition.as_deref(),
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
    let id = bank_id.clone();
    let existing = tokio::task::spawn_blocking(move || banks::get(&db, &id))
        .await
        .map_err(join_err)??;
    if existing.is_none() {
        return Err(ApiError::not_found("bank not found"));
    }

    let db = state.db.clone();
    tokio::task::spawn_blocking(move || banks::delete(&db, &bank_id))
        .await
        .map_err(join_err)??;
    Ok(StatusCode::NO_CONTENT)
}
