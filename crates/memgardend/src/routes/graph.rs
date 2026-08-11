//! `GET /v1/banks/{bank_id}/graph` (CE-7, PR B5) — the data source the
//! GV-1..3 graph viewer needs ("그래프 뷰어는 CE-7 이후 착수 가능", PRD) — and
//! `GET /v1/banks/{bank_id}/nodes/{id}` (E1), which is what the viewer calls
//! when the user clicks one of those nodes.

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

/// Node text is truncated to a label. A node's `text` can run to tens of
/// kilobytes, so `limit` bounds the node *count* but not the response size —
/// 2000 nodes could be 60MB+ before serialization. The viewer draws a label
/// and fetches the full text on click; `uuid` and `id` are both in the
/// payload for that.
const MAX_LABEL_CHARS: usize = 160;

fn label(text: String) -> String {
    if text.chars().count() <= MAX_LABEL_CHARS {
        return text;
    }
    text.chars()
        .take(MAX_LABEL_CHARS - 1)
        .chain(['…'])
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct GraphQuery {
    pub limit: Option<usize>,
    /// Comma-separated fact types, e.g. `types=world,observation`. Absent
    /// means all types.
    pub types: Option<String>,
    /// Critic Revision R15: filters on the `session:{id}` tag B3 writes.
    pub session: Option<String>,
    /// Inclusive `event_date` bounds in epoch ms (E3's date filter). Applied
    /// in SQL rather than by the caller: `limit` takes the newest ids, so a
    /// range narrowed after the fact could never reach a memory older than
    /// the newest `limit` of them.
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Comma-separated node ids. When present the newest-`limit` selection is
    /// replaced by exactly these, which is how the explorer asks for the
    /// edges *among* the nodes it already has on screen — `nodes/{id}` only
    /// ever answers for one node's own neighbours, so a graph built from it
    /// alone is a star.
    pub ids: Option<String>,
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
        return Err(
            memgarden_core::Error::Invalid(format!("limit must be 1..={MAX_LIMIT}")).into(),
        );
    }
    // An id list is a selection, not a way around the cap on how much one
    // response may carry, so it is bounded by the same ceiling as `limit`.
    let mut ids: Vec<i64> = q
        .ids
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.truncate(MAX_LIMIT);

    let mut fact_types: Vec<FactType> = match &q.types {
        Some(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<FactType>())
            .collect::<Result<_, _>>()?,
        None => vec![],
    };
    // The domain is three values; `?types=world,world,world` must not become
    // three JSON array entries the SQL then scans for.
    fact_types.sort_unstable_by_key(|t| t.as_str());
    fact_types.dedup();

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
        graph::graph_view(
            &db,
            &bank_id,
            limit,
            &graph::GraphFilter {
                fact_types: &fact_types,
                session: q.session.as_deref(),
                since: q.since,
                until: q.until,
                ids: &ids,
            },
        )
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
                text: label(n.text),
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

// ---------------------------------------------------------------------------
// E1 — one node in full
// ---------------------------------------------------------------------------

/// A related node, drawn as one row in the detail panel. `text` is a label,
/// truncated exactly as `/graph`'s is — these are list rows, not the subject.
#[derive(Debug, Serialize)]
pub struct RelatedNodeResponse {
    pub id: i64,
    pub uuid: String,
    #[serde(rename = "type")]
    pub fact_type: String,
    pub label: String,
    /// Present only for a neighbour, absent for provenance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct NodeDetailResponse {
    pub id: i64,
    pub uuid: String,
    #[serde(rename = "type")]
    pub fact_type: String,
    /// **Not truncated.** This endpoint exists because `/graph` truncates.
    pub text: String,
    pub context: Option<String>,
    pub event_date: Option<i64>,
    pub occurred_start: Option<i64>,
    pub occurred_end: Option<i64>,
    pub created_at: i64,
    pub proof_count: i64,
    pub tags: Vec<String>,
    pub entities: Vec<String>,
    pub sources: Vec<RelatedNodeResponse>,
    pub cited_by: Vec<RelatedNodeResponse>,
    /// Keyed by link type (`semantic`, `temporal`, `caused_by`, …). A type
    /// with no edges is absent rather than empty, so the panel can render
    /// what is there without knowing the full domain.
    pub neighbors: std::collections::BTreeMap<String, Vec<RelatedNodeResponse>>,
}

fn related(n: memgarden_store::graph::NodeSummary, weight: Option<f64>) -> RelatedNodeResponse {
    RelatedNodeResponse {
        id: n.id,
        uuid: n.uuid,
        fact_type: n.fact_type,
        label: label(n.text),
        weight,
    }
}

/// `GET /v1/banks/{bank_id}/nodes/{id}`.
///
/// 404 both when the node does not exist and when it belongs to another bank,
/// deliberately: `bank_id` comes from the URL, and distinguishing the two
/// would let a caller enumerate ids across banks.
pub async fn get_node(
    State(state): State<AppState>,
    Path((bank_id, node_id)): Path<(String, i64)>,
) -> Result<Json<NodeDetailResponse>, ApiError> {
    let db = state.db.clone();
    let bank = bank_id.clone();
    let exists = tokio::task::spawn_blocking(move || banks::get(&db, &bank))
        .await
        .map_err(join_err)??;
    if exists.is_none() {
        return Err(ApiError::not_found("bank not found"));
    }

    let db = state.db.clone();
    let detail = tokio::task::spawn_blocking(move || graph::node_detail(&db, &bank_id, node_id))
        .await
        .map_err(join_err)??;
    let Some(d) = detail else {
        return Err(ApiError::not_found("node not found"));
    };

    let mut neighbors: std::collections::BTreeMap<String, Vec<RelatedNodeResponse>> =
        std::collections::BTreeMap::new();
    for adj in d.neighbors {
        neighbors
            .entry(adj.link_type)
            .or_default()
            .push(related(adj.node, Some(adj.weight)));
    }

    Ok(Json(NodeDetailResponse {
        id: d.node.id,
        uuid: d.node.uuid,
        fact_type: d.node.fact_type,
        text: d.node.text,
        context: d.context,
        event_date: d.event_date,
        occurred_start: d.occurred_start,
        occurred_end: d.occurred_end,
        created_at: d.created_at,
        proof_count: d.proof_count,
        tags: d.tags,
        entities: d.entities,
        sources: d.sources.into_iter().map(|n| related(n, None)).collect(),
        cited_by: d.cited_by.into_iter().map(|n| related(n, None)).collect(),
        neighbors,
    }))
}
