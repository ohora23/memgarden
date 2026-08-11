//! `GET /v1/banks/{bank_id}/events` — server-sent events for one bank (E4).
//!
//! # Why SSE and not a websocket
//!
//! The traffic is one-directional and small: the server says "these ids
//! changed" and the browser decides what to do about it. SSE is a `GET` that
//! never ends, so it inherits the origin, the `Host` check and the reverse
//! proxy story the rest of the API already has, and `EventSource` reconnects
//! on its own. A websocket would buy a channel back that nothing needs and
//! cost an upgrade path to get wrong.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::{self, Stream};
use tokio::sync::broadcast::error::RecvError;

use crate::state::AppState;

/// Long enough to sit well under the 60s idle timeout most proxies default
/// to, short enough that a dead connection is noticed while the tab is still
/// open.
const KEEPALIVE_SECS: u64 = 15;

pub async fn bank_events(
    State(state): State<AppState>,
    Path(bank_id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();

    let stream = stream::unfold((rx, bank_id), |(mut rx, bank_id)| async move {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.bank_id == bank_id => {
                    // Serialisation cannot fail for this type; a `data:` line
                    // that could not be built would be a bug, not a runtime
                    // condition, so an empty payload is never emitted.
                    let Ok(json) = serde_json::to_string(&ev) else {
                        continue;
                    };
                    return Some((
                        Ok(Event::default().event(ev.kind).data(json)),
                        (rx, bank_id),
                    ));
                }
                // Another bank's traffic. One channel serves every
                // subscriber, so this is the common case with several banks
                // in flight and must not end the stream.
                Ok(_) => continue,
                // The subscriber fell behind the ring buffer. Saying so is
                // the point: the browser cannot know what it missed, so it is
                // told to re-read rather than left believing it is current.
                Err(RecvError::Lagged(n)) => {
                    let payload = format!(r#"{{"missed":{n}}}"#);
                    return Some((
                        Ok(Event::default().event("reload").data(payload)),
                        (rx, bank_id),
                    ));
                }
                // Only on shutdown, when the sender is dropped.
                Err(RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(KEEPALIVE_SECS))
            .text("keep-alive"),
    )
}
