//! `GET /v1/banks/{bank_id}/graph` (CE-7, PR B5) — the data source the
//! GV-1..3 graph viewer needs ("그래프 뷰어는 CE-7 이후 착수 가능", PRD).

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use memgarden_core::types::FactType;
use memgarden_store::{banks, graph};

use crate::error::{ApiError, join_err};
use crate::state::AppState;

/// Hard ceiling on `limit`: the viewer renders a force-directed layout and
/// anything past a few thousand nodes is unreadable before it is slow.
const MAX_LIMIT: usize = 2000;
const DEFAULT_LIMIT: usize = 200;

#[derive(Debug, Deserialize)]
pub struct GraphQuery {
    pub limit: Option<usize>,
    /// Comma-separated fact types, e.g. `types=world,observation`. Absent
    /// means all types.
    pub types: Option<String>,
    /// Critic Revision R15: filters on the `session:{id}` tag B3 writes.
    pub session: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GraphNodeResponse {
    pub id: i64,
    pub uuid: String,
    #[serde(rename = "type")]
    pub fact_type: String,
    pub text: String,
    pub mentioned_at: Option<i64>,
    pub entities: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct GraphLinkResponse {
    pub from: i64,
    pub to: i64,
    #[serde(rename = "type")]
    pub link_type: String,
    pub weight: f64,
}

#[derive(Debug, Serialize)]
pub struct GraphResponse {
    pub nodes: Vec<GraphNodeResponse>,
    pub links: Vec<GraphLinkResponse>,
}

pub async fn get_graph(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<GraphResponse>, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(memgarden_core::Error::Invalid(format!(
            "limit must be 1..={MAX_LIMIT}"
        ))
        .into());
    }
    let fact_types: Vec<FactType> = match &q.types {
        Some(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<FactType>())
            .collect::<Result<_, _>>()?,
        None => vec![],
    };

    let db = state.db.clone();
    let bank = bank_id.clone();
    let exists = tokio::task::spawn_blocking(move || banks::get(&db, &bank))
        .await
        .map_err(join_err)??;
    if exists.is_none() {
        return Err(ApiError::not_found("bank not found"));
    }

    let db = state.db.clone();
    let (nodes, links) = tokio::task::spawn_blocking(move || {
        graph::graph_view(&db, &bank_id, limit, &fact_types, q.session.as_deref())
    })
    .await
    .map_err(join_err)??;

    Ok(Json(GraphResponse {
        nodes: nodes
            .into_iter()
            .map(|n| GraphNodeResponse {
                id: n.id,
                uuid: n.uuid,
                fact_type: n.fact_type,
                text: n.text,
                mentioned_at: n.mentioned_at,
                entities: n.entities,
            })
            .collect(),
        links: links
            .into_iter()
            .map(|l| GraphLinkResponse {
                from: l.from_node_id,
                to: l.to_node_id,
                link_type: l.link_type,
                weight: l.weight,
            })
            .collect(),
    }))
}
