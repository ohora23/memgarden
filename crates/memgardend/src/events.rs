//! E4 — what changed, announced to whoever is watching.
//!
//! The explorer is a window onto a bank that keeps growing behind it: the
//! hooks retain while you read, and until now the screen only ever showed the
//! state it was opened in. AC-4 asks for a retain to be visible within five
//! seconds without a reload.
//!
//! # Why a broadcast channel and not a table
//!
//! This is a notification, not a record. The database already holds what
//! happened, and a subscriber that misses an event can ask for the truth at
//! any time — so nothing here needs to survive a restart, be delivered in
//! order across reconnects, or be acknowledged. `tokio::sync::broadcast` is
//! exactly that shape: bounded, lossy under lag, and free when nobody is
//! listening.
//!
//! The capacity is small on purpose. A subscriber slow enough to fall
//! `CAPACITY` behind is one that should re-read rather than replay, and
//! `RecvError::Lagged` is how it finds out — the SSE handler turns that into
//! a `reload` event rather than pretending the gap did not happen.

use serde::Serialize;
use tokio::sync::broadcast;

/// Deep enough to absorb one retain's chunks landing back to back, shallow
/// enough that a stalled browser tab cannot hold megabytes of ids.
pub const CAPACITY: usize = 64;

/// One thing that happened, scoped to a bank.
///
/// Ids rather than contents: the receiver decides whether it cares, and asks
/// for what it needs through the ordinary endpoints. That keeps this type
/// from becoming a second, weaker copy of `GET .../nodes/{id}` that has to be
/// kept in step with it.
#[derive(Debug, Clone, Serialize)]
pub struct GraphEvent {
    pub bank_id: String,
    /// `nodes` today. Named rather than implied so a later kind — links,
    /// consolidation — does not have to break the shape.
    pub kind: &'static str,
    pub ids: Vec<i64>,
}

/// The publish side, cloned into `AppState`.
pub type Publisher = broadcast::Sender<GraphEvent>;

pub fn channel() -> Publisher {
    broadcast::channel(CAPACITY).0
}

/// Fire-and-forget. `send` fails only when there are no receivers, which is
/// the normal case — nobody has the UI open — so the error is dropped rather
/// than logged, or a daemon with no browser attached would narrate every
/// retain into its log.
pub fn publish(tx: &Publisher, bank_id: &str, kind: &'static str, ids: Vec<i64>) {
    if ids.is_empty() {
        return;
    }
    let _ = tx.send(GraphEvent {
        bank_id: bank_id.to_string(),
        kind,
        ids,
    });
}
