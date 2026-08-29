//! Mental-model CRUD, KNN search, refresh, and single-shot reflect (CE-10).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use memgarden_store::banks;
use memgarden_store::mental_models::{self as store, MentalModel};

use crate::error::{ApiError, join_err};
use crate::json::ApiJson;
use crate::mental::reflect::{ReflectOutcome, reflect};
use crate::mental::{self, cron};
use crate::state::AppState;

/// A name is a label, not a document.
const MAX_NAME_BYTES: usize = 500;
/// The source query is fed to recall, which has its own 8KB ceiling.
const MAX_SOURCE_QUERY_BYTES: usize = 8 * 1024;
/// Caller-supplied content. Generous — a mental model is a document — but not
/// unbounded: it is embedded, echoed into every reflect prompt (until the
/// token bound sheds it) and returned on every list.
const MAX_CONTENT_BYTES: usize = 64 * 1024;
/// Page size ceiling for the list endpoint.
const MAX_LIST_LIMIT: usize = 200;
/// A 5-field cron expression. The longest sensible one — every field a
/// comma list — is a few dozen bytes; 256 is roomy and still tiny.
///
/// This bound is load-bearing, not cosmetic (review round 1, MUST FIX 1).
/// `cron::parse_field` expands each comma part into its values with no
/// per-part cap, so an unbounded field is a Vec allocation and a sort
/// proportional to the *input length × 60*, and `MentalModelResponse::new`
/// re-parses the stored expression for **every row of every read** to compute
/// `due`. Unbounded, a 2MB trigger measured 98.8 ms per parse — a 200-row page
/// would block a worker for ~20 s. Bounding the input is the fix; caching the
/// derived `due` is not, because the cost would still be paid once on write
/// and once per row on any cache miss. At 256 bytes a parse is microseconds
/// and the amplification is gone.
const MAX_TRIGGER_BYTES: usize = 256;

#[derive(Debug, Serialize)]
pub struct MentalModelResponse {
    pub id: String,
    pub bank_id: String,
    pub name: String,
    pub source_query: Option<String>,
    pub content: String,
    pub max_tokens: Option<i64>,
    /// Cron expression, or `null`.
    pub trigger: Option<String>,
    /// Whether `trigger` has fired since `last_refreshed_at`
    /// (`maintenance.py:417-425`). Always `false` without a trigger.
    ///
    /// Reported rather than acted on: this PR ships **no background refresh
    /// task**. Nothing in this system spends GPU without a caller, and CE-10
    /// has no caller yet (design note, "Diverged from legacy").
    pub due: bool,
    /// Parsed back from the JSON column; `null` if it was never written.
    pub reflect_response: Option<Value>,
    pub last_refreshed_at: Option<i64>,
    /// The highest `memory_nodes.created_at` a refresh has folded in. Distinct
    /// from `last_refreshed_at` on purpose — see 0006's comment.
    pub refresh_watermark: Option<i64>,
    pub created_at: i64,
}

/// Builds the response bodies **off the reactor** (review round 1, L8).
///
/// `due` runs `cron::prev_fire`, which walks whole days backwards until the
/// schedule matches or `MAX_LOOKBACK_DAYS` (1,464) runs out. Realistic
/// schedules match in one or two iterations, but `0 0 30 2 *` is eleven bytes,
/// parses fine, and never fires — so it walks the full lookback every time it
/// is rendered. `MAX_TRIGGER_BYTES` does not help here: input length bounds
/// the *parse*, not the *walk*.
///
/// So every handler routes its rows through this one call rather than mapping
/// them inline. One `spawn_blocking` per request, whatever the row count —
/// which is also why memoizing per distinct trigger string is the wrong lever:
/// it would help the 200-rows-one-schedule case and do nothing for the single
/// pathological row.
async fn respond(
    models: Vec<MentalModel>,
    now_ms: i64,
) -> Result<Vec<MentalModelResponse>, ApiError> {
    tokio::task::spawn_blocking(move || {
        models
            .into_iter()
            .map(|m| MentalModelResponse::new(m, now_ms))
            .collect()
    })
    .await
    .map_err(join_err)
}

/// [`respond`] for the single-row handlers.
async fn respond_one(model: MentalModel, now_ms: i64) -> Result<MentalModelResponse, ApiError> {
    let mut out = respond(vec![model], now_ms).await?;
    Ok(out.remove(0))
}

impl MentalModelResponse {
    /// Cheap except for `due` — call it from [`respond`], never on the reactor.
    fn new(m: MentalModel, now_ms: i64) -> Self {
        let due = m
            .trigger
            .as_deref()
            .is_some_and(|t| cron::is_due(t, m.last_refreshed_at, now_ms));
        MentalModelResponse {
            id: m.id,
            bank_id: m.bank_id,
            name: m.name,
            source_query: m.source_query,
            content: m.content,
            max_tokens: m.max_tokens,
            trigger: m.trigger,
            due,
            // A column that fails to parse is reported as absent rather than
            // failing the read: it is an audit field, and no caller's GET
            // should 500 because a past write stored something odd.
            reflect_response: m
                .reflect_response
                .and_then(|raw| serde_json::from_str(&raw).ok()),
            last_refreshed_at: m.last_refreshed_at,
            refresh_watermark: m.refresh_watermark,
            created_at: m.created_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    /// When present, the list is the **KNN** neighbourhood of this text over
    /// `vec_mental_models`, nearest first, instead of the recency page.
    #[serde(default)]
    pub q: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub name: String,
    #[serde(default, rename = "sourceQuery")]
    pub source_query: Option<String>,
    /// Defaults to `mental::MENTAL_MODEL_PENDING_CONTENT` — the model exists
    /// before its first refresh has anything to say.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, rename = "maxTokens")]
    pub max_tokens: Option<i64>,
    /// 5-field cron expression, UTC. Rejected at write time if unparseable.
    #[serde(default)]
    pub trigger: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "sourceQuery")]
    pub source_query: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, rename = "maxTokens")]
    pub max_tokens: Option<i64>,
    #[serde(default)]
    pub trigger: Option<String>,
}

/// Deliberately empty and deliberately **required** — same CSRF shape as
/// `/consolidate` (security review LOW 5): requiring a JSON content type makes
/// the request preflighted, and the preflight is what the Host guard refuses.
#[derive(Debug, Deserialize, Default)]
pub struct RefreshRequest {}

#[derive(Debug, Deserialize)]
pub struct ReflectRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /v1/banks/{bank_id}/mental-models` — the recency page, or the KNN
/// neighbourhood of `?q=`.
///
/// **`trigger` and `due` are advisory.** MemGarden validates a cron
/// expression, stores it, and reports on every read whether it has fired since
/// `last_refreshed_at` — but **nothing in the daemon acts on it**: CE-10 ships
/// no background refresh task, so `due` stays `true` until a client calls
/// `POST …/{mm_id}/refresh` itself. A scheduler is a caller's job for now (see
/// `docs/parity-gaps.md`). Stated here rather than only in a Rust doc comment
/// because an HTTP client sees `"due": true` and would otherwise reasonably
/// assume something is going to act on it.
pub async fn list_mental_models(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<MentalModelResponse>>, ApiError> {
    require_bank(&state, &bank_id).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, MAX_LIST_LIMIT);
    let offset = q.offset.unwrap_or(0);
    let now = memgarden_core::now_ms();

    // The search text is embedded, so it is bounded like every other string
    // that reaches the model.
    if q.q
        .as_deref()
        .is_some_and(|s| s.len() > MAX_SOURCE_QUERY_BYTES)
    {
        return Err(memgarden_core::Error::Invalid(format!(
            "q too long (max {MAX_SOURCE_QUERY_BYTES} bytes)"
        ))
        .into());
    }
    // KNN mode is unpaged, so an `offset` alongside `q` is refused rather than
    // ignored (review round 1, L3). Same defect class as the wrapping OFFSET:
    // a paging parameter that is accepted and then quietly does nothing hands
    // the caller page 1 with no signal. A comment cannot reach an HTTP client.
    if q.q.is_some() && q.offset.is_some_and(|o| o > 0) {
        return Err(memgarden_core::Error::Invalid(
            "offset is not supported with q: KNN search returns the nearest \
             `limit` models and is unpaged"
                .to_string(),
        )
        .into());
    }
    let found = match q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        // KNN search. 503 rather than a silent recency page when the embedder
        // is not ready: a caller who asked for nearest-neighbour ordering and
        // got creation order would have no way to tell.
        Some(text) => {
            let embedding = mental::embed_one(&state, text.to_string())
                .await
                .ok_or_else(|| ApiError::unavailable("embedding model not ready"))?;
            let (db, bank) = (state.db.clone(), bank_id.clone());
            tokio::task::spawn_blocking(move || {
                let hits = store::knn(&db, &bank, &embedding, limit)?;
                // ponytail: one `get` per hit — N+1, with N bounded by
                // `limit` (<= MAX_LIST_LIMIT = 200). Sequential inside one
                // blocking task, so it is slow at the ceiling, never a pool
                // deadlock. Upgrade path when something actually asks for 200
                // mental models: a batched `get_many` in the shape of
                // `search::hydrate` / `nodes::created_after` — one statement,
                // `id IN (SELECT value FROM json_each(?))`.
                hits.into_iter()
                    .filter_map(|(id, _)| store::get(&db, &bank, &id).transpose())
                    .collect::<memgarden_core::error::Result<Vec<_>>>()
            })
            .await
            .map_err(join_err)??
        }
        None => {
            let (db, bank) = (state.db.clone(), bank_id.clone());
            tokio::task::spawn_blocking(move || store::list(&db, &bank, limit, offset))
                .await
                .map_err(join_err)??
        }
    };
    Ok(Json(respond(found, now).await?))
}

pub async fn create_mental_model(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    ApiJson(body): ApiJson<CreateRequest>,
) -> Result<(StatusCode, Json<MentalModelResponse>), ApiError> {
    require_bank(&state, &bank_id).await?;
    validate(
        Some(&body.name),
        body.source_query.as_deref(),
        body.content.as_deref(),
        body.max_tokens,
        body.trigger.as_deref(),
    )?;
    let created = mental::create(
        &state,
        &bank_id,
        body.name.trim(),
        body.source_query.as_deref(),
        body.content.as_deref(),
        body.max_tokens,
        body.trigger.as_deref(),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(respond_one(created, memgarden_core::now_ms()).await?),
    ))
}

pub async fn get_mental_model(
    State(state): State<AppState>,
    Path((bank_id, mm_id)): Path<(String, String)>,
) -> Result<Json<MentalModelResponse>, ApiError> {
    require_bank(&state, &bank_id).await?;
    let found = mental::load(&state, &bank_id, &mm_id).await?;
    Ok(Json(respond_one(found, memgarden_core::now_ms()).await?))
}

/// Sets the fields present in the body. A field cannot be *cleared* through
/// this route (see `mental_models::Patch`); nothing in CE-10 needs to.
pub async fn patch_mental_model(
    State(state): State<AppState>,
    Path((bank_id, mm_id)): Path<(String, String)>,
    ApiJson(body): ApiJson<PatchRequest>,
) -> Result<Json<MentalModelResponse>, ApiError> {
    require_bank(&state, &bank_id).await?;
    validate(
        body.name.as_deref(),
        body.source_query.as_deref(),
        body.content.as_deref(),
        body.max_tokens,
        body.trigger.as_deref(),
    )?;
    let updated = mental::patch(
        &state,
        &bank_id,
        &mm_id,
        &mental::Fields {
            name: body.name.as_deref().map(str::trim),
            source_query: body.source_query.as_deref(),
            content: body.content.as_deref(),
            max_tokens: body.max_tokens,
            trigger: body.trigger.as_deref(),
        },
    )
    .await?;
    Ok(Json(respond_one(updated, memgarden_core::now_ms()).await?))
}

pub async fn delete_mental_model(
    State(state): State<AppState>,
    Path((bank_id, mm_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_bank(&state, &bank_id).await?;
    let (db, bank, id) = (state.db.clone(), bank_id.clone(), mm_id.clone());
    let changed = tokio::task::spawn_blocking(move || store::delete(&db, &bank, &id))
        .await
        .map_err(join_err)??;
    if changed == 0 {
        return Err(ApiError::not_found(format!(
            "mental model not found: {mm_id}"
        )));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Runs one refresh **synchronously** — same reasoning as `/consolidate`: the
/// caller asked for it and wants to know what it produced, but it makes an LLM
/// call, so the client needs a matching timeout.
///
/// 502 when the model produced empty content; the previous content is
/// preserved (`memory_engine.py:11724-11743`).
pub async fn refresh_mental_model(
    State(state): State<AppState>,
    Path((bank_id, mm_id)): Path<(String, String)>,
    ApiJson(_): ApiJson<RefreshRequest>,
) -> Result<Json<MentalModelResponse>, ApiError> {
    require_bank(&state, &bank_id).await?;
    let refreshed = mental::refresh(&state, &bank_id, &mm_id).await?;
    Ok(Json(
        respond_one(refreshed, memgarden_core::now_ms()).await?,
    ))
}

/// `POST /v1/banks/{bank_id}/reflect` — one recall plus one LLM call.
pub async fn reflect_bank(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    ApiJson(body): ApiJson<ReflectRequest>,
) -> Result<Json<ReflectOutcome>, ApiError> {
    require_bank(&state, &bank_id).await?;
    if body.query.len() > MAX_SOURCE_QUERY_BYTES {
        return Err(memgarden_core::Error::Invalid(format!(
            "query too long: {} bytes (max {MAX_SOURCE_QUERY_BYTES})",
            body.query.len()
        ))
        .into());
    }
    let limit = body
        .limit
        .unwrap_or(state.cfg.recall.limit)
        .clamp(1, MAX_LIST_LIMIT);
    Ok(Json(reflect(&state, &bank_id, body.query, limit).await?))
}

/// One validator for both write routes: whatever is present must be sane.
///
/// The cron check is the important one — an unparseable trigger is refused
/// here rather than becoming a `due` flag that is silently always false.
fn validate(
    name: Option<&str>,
    source_query: Option<&str>,
    content: Option<&str>,
    max_tokens: Option<i64>,
    trigger: Option<&str>,
) -> Result<(), ApiError> {
    let invalid = |m: String| ApiError::from(memgarden_core::Error::Invalid(m));
    if let Some(name) = name {
        if name.trim().is_empty() {
            return Err(invalid("name must not be empty".to_string()));
        }
        if name.len() > MAX_NAME_BYTES {
            return Err(invalid(format!(
                "name too long: {} bytes (max {MAX_NAME_BYTES})",
                name.len()
            )));
        }
    }
    if let Some(q) = source_query
        && q.len() > MAX_SOURCE_QUERY_BYTES
    {
        return Err(invalid(format!(
            "sourceQuery too long: {} bytes (max {MAX_SOURCE_QUERY_BYTES})",
            q.len()
        )));
    }
    if let Some(c) = content {
        if c.len() > MAX_CONTENT_BYTES {
            return Err(invalid(format!(
                "content too long: {} bytes (max {MAX_CONTENT_BYTES})",
                c.len()
            )));
        }
        // Empty content is the exact value `refresh` refuses to write
        // (`mental::refresh` outcome 2) and the reason
        // `MENTAL_MODEL_PENDING_CONTENT` exists — a caller must not be able to
        // put the document into that state through the front door either
        // (review round 1, L5).
        if c.trim().is_empty() {
            return Err(invalid(
                "content must not be empty (omit it to keep the current text)".to_string(),
            ));
        }
    }
    if let Some(t) = max_tokens
        && !(1..=i64::from(mental::REFRESH_REPLY_MAX_TOKENS)).contains(&t)
    {
        return Err(invalid(format!(
            "maxTokens must be 1..={}: {t}",
            mental::REFRESH_REPLY_MAX_TOKENS
        )));
    }
    if let Some(t) = trigger {
        // Length first: `Cron::parse` is the expensive half, and the whole
        // point of the bound is that it never runs on a huge input.
        if t.len() > MAX_TRIGGER_BYTES {
            return Err(invalid(format!(
                "trigger too long: {} bytes (max {MAX_TRIGGER_BYTES})",
                t.len()
            )));
        }
        if t == cron::AFTER_CONSOLIDATION {
            return Ok(());
        }
        if let Err(e) = cron::Cron::parse(t) {
            return Err(invalid(format!("invalid trigger: {e}")));
        }
    }
    Ok(())
}

async fn require_bank(state: &AppState, bank_id: &str) -> Result<(), ApiError> {
    let db = state.db.clone();
    let bank = bank_id.to_string();
    let found = tokio::task::spawn_blocking(move || banks::get(&db, &bank))
        .await
        .map_err(join_err)??;
    if found.is_none() {
        return Err(ApiError::not_found(format!("bank not found: {bank_id}")));
    }
    Ok(())
}
