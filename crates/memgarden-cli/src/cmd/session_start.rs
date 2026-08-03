//! `hook session-start` — the `SessionStart` event.
//!
//! Four things, in this order, and **nothing on stdout**: ensure the bank
//! exists, mirror the session into the daemon, make sure a local state file
//! exists (rebuilding it from the mirror when it does not), and hand the
//! housekeeping to a detached child.
//!
//! Emitting nothing is a decision, not an omission (plan §Binding decisions
//! #3). `SessionStart` is one of the three events whose stdout becomes model
//! context; the recall hook fires milliseconds later and has something worth
//! saying, and two injections at the top of a session is one too many.
//!
//! # The failure posture, in one paragraph
//!
//! Nothing here may break the session. A daemon that is down costs one
//! `ECONNREFUSED` per request (microseconds on loopback) and a
//! `transport_failures` increment; a daemon that answers 4xx costs nothing at
//! all, because §Failure posture's `session-start` row is *"ignore, exit 0"* —
//! there is no cursor to protect here, so poisoning has nothing to defend.
//! Every downstream consumer self-heals: `retain` recreates a missing bank on
//! 404, and a session that never mirrored is picked up by the next session's
//! catch-up.

use memgarden_core::config::Config;
use serde::Deserialize;

use crate::http;
use crate::state::{self, SessionState};
use crate::{bank, hookio};

use super::MAX_SESSION_ID_BYTES;

/// What the daemon's `sessions` mirror is allowed to tell recovery.
///
/// **`byte_offset` is deliberately not a field**, and that is the whole point
/// of this struct existing rather than reusing the response shape.
/// `SessionResponse` (C1) carries both cursors; seeding `offset` from the
/// optimistic one skips exactly the bytes the dual cursor exists to protect,
/// because it is what some hook *POSTed* — already ahead of reality after a
/// failed job or the byte-budget 429, where the mirror advanced and the hook
/// deliberately did not.
///
/// C2a's `SessionState::recovered` names its parameter `confirmed_offset` to
/// make the right thing the easy thing, and C2a's own review recorded that
/// this is not enough: every `SessionState` field is `pub`, so C2b could still
/// assign `offset` directly. **A field that is never deserialized cannot be
/// misused**, so the wrong cursor is not merely discouraged here — it is not
/// present. serde ignores unknown fields, so it never reaches this process.
///
/// Both fields are `i64` and clamped rather than `u64`: a negative would make
/// the *whole* struct fail to parse, silently dropping `chunk_index` too, and
/// falling back to a full re-ingest over a value the daemon cannot produce.
#[derive(Debug, Deserialize)]
struct Mirror {
    #[serde(default)]
    confirmed_offset: i64,
    #[serde(default)]
    chunk_index: i64,
}

/// What the round trip to the daemon told us, split by what each outcome is
/// allowed to move (§Failure posture).
enum Mirrored {
    /// 2xx. Any success clears the breaker.
    Ok(Mirror),
    /// The daemon answered, and the answer was not 2xx. **No counter moves**:
    /// the `session-start` row is "ignore, exit 0", and `reject_failures`
    /// exists to poison a *cursor*, which this subcommand does not advance.
    Rejected,
    /// Connect refused, timeout, or a body we refuse to parse.
    /// `transport_failures += 1`; at `breaker_failures` the breaker opens.
    Transport,
    /// `daemon_url` is not a loopback `http://host:port`. A **config** fault,
    /// so it must not move `transport_failures` — a typo that opened the
    /// circuit breaker would look exactly like an outage in `hooks status`.
    Config,
}

pub fn run() {
    // stdin first, and before the config read: it is the cheapest rejection,
    // and it is what makes `empty_and_malformed_stdin_exit_zero` cover
    // `hookio` rather than only the exit code.
    let Some(input) = hookio::read_stdin() else {
        return;
    };
    let Some(cfg) = super::enabled_config() else {
        return;
    };
    if input.session_id.is_empty() || input.session_id.len() > MAX_SESSION_ID_BYTES {
        super::debug(&cfg.hooks, "session_start: unusable session_id");
        return;
    }

    let project_dir = std::env::var("CLAUDE_PROJECT_DIR").ok();
    let bank_id = bank::derive(&cfg.hooks, input.project_dir(project_dir.as_deref()));
    let mirrored = mirror(&cfg, &bank_id, &input);

    let dir = cfg.hooks.state_dir.as_path();
    // Locked because a `resume` `SessionStart` can land while the previous
    // session's `async: true` `Stop` is still writing. Advisory, so it
    // serializes MemGarden against MemGarden and nothing else — which is
    // exactly the race that exists.
    let _ = state::with_lock(dir, &input.session_id, || {
        let mut st = state::load(dir, &input.session_id).unwrap_or_else(|| match &mirrored {
            // The wiped-state-dir recovery. `confirmed_offset` is by
            // construction behind anything unresolved, so there is no in-flight
            // job to reconcile and re-sending from it is at-least-once: the
            // daemon's `doc_key` content hash answers `duplicate`.
            Mirrored::Ok(m) => SessionState::recovered(
                &input.session_id,
                &bank_id,
                m.confirmed_offset.max(0) as u64,
                m.chunk_index.max(0) as u64,
            ),
            _ => SessionState::new(&input.session_id, &bank_id),
        });
        // Refreshed on every start, including a `resume`: Claude Code has been
        // observed to move a transcript between versions, and catch-up reads
        // this field with no payload to fall back on.
        //
        // `bank_id` is deliberately NOT refreshed. A session's cursor belongs
        // to the bank its bytes were posted to; re-deriving it mid-session
        // (a `resume` from a different cwd, an edited `directory_bank_map`)
        // would leave the offset pointing into another bank's document.
        input.transcript_path.clone_into(&mut st.transcript_path);
        // Stored for C4b's two detached callers, which have no payload and
        // would otherwise post the retain with a `null` `cwd` — producing
        // absolute `file:` tags for the same files the live hook tagged
        // relatively. Only when non-empty, so an absent field never clobbers
        // a good stored one.
        if !input.cwd.is_empty() {
            input.cwd.clone_into(&mut st.cwd);
        }
        match mirrored {
            Mirrored::Ok(_) => {
                st.transport_failures = 0;
                st.breaker_open_until_ms = 0;
            }
            Mirrored::Transport => {
                st.transport_failures = st.transport_failures.saturating_add(1);
            }
            Mirrored::Rejected | Mirrored::Config => {}
        }
        if let Err(e) = state::store(dir, &st) {
            super::debug(
                &cfg.hooks,
                &format!("session_start: state write failed: {e}"),
            );
        }
    });

    // Detached, and last: whatever it costs is not on the session's clock.
    if let Ok(exe) = std::env::current_exe() {
        super::spawn_detached(&exe, &["hook", "catchup", &input.session_id]);
    }
}

/// `POST /v1/banks` then `POST /v1/banks/{bank}/sessions`.
///
/// Two round trips every session start, in that order, because the sessions
/// row has a foreign key to the bank. The alternative — POST the session and
/// create the bank only on 404 — saves one loopback round trip in the steady
/// state and costs a retry path; at once per session that is not a trade worth
/// the branch.
fn mirror(cfg: &Config, bank_id: &str, input: &hookio::HookInput) -> Mirrored {
    let target = match super::target(&cfg.hooks) {
        Ok(t) => t,
        Err(http::HttpError::Url(m)) => {
            super::debug(&cfg.hooks, &format!("session_start: {m}"));
            return Mirrored::Config;
        }
        // An unreadable `<data>/daemon.token` is transport-class, not config —
        // see `cmd::target`.
        Err(e) => {
            super::debug(&cfg.hooks, &format!("session_start: {e}"));
            return Mirrored::Transport;
        }
    };
    let timeouts = super::interactive_timeouts(&cfg.hooks);

    // 409 means it already exists, which is the expected answer on every
    // session but the first and is not worth a pre-flight GET to avoid. The
    // result is ignored entirely: if this failed for a real reason, the
    // sessions POST below fails too and reports it.
    //
    // **No `mission` is sent.** The plan's C2b line says to post
    // `[profile] bank_mission`, but `routes/banks.rs::create_bank` already
    // applies the daemon's own `[profile] bank_mission` to a bank created
    // without one. Sending ours would be a client overriding a server policy
    // with a value read from a config the server may not share — and the
    // divergence table's stated reason for not memoizing missions client-side
    // is precisely that "the daemon already owns mission precedence".
    let _ = http::post(
        &target,
        "/v1/banks",
        serde_json::json!({ "bank_id": bank_id })
            .to_string()
            .as_bytes(),
        &timeouts,
    );

    // `null` for an absent field rather than `""`: the daemon reads
    // `Option<String>` and leaves the column untouched for both, but an empty
    // string would clobber a `cwd` a previous start recorded correctly.
    let body = serde_json::json!({
        "session_id": input.session_id,
        "source": none_if_empty(&input.source),
        "cwd": none_if_empty(&input.cwd),
        "transcript_path": none_if_empty(&input.transcript_path),
    })
    .to_string();
    let path = format!("/v1/banks/{}/sessions", http::encode_path_segment(bank_id));

    match http::post(&target, &path, body.as_bytes(), &timeouts) {
        // The upsert answers with the merged row, so **this response is the
        // recovery source**. The plan specifies a separate
        // `GET .../sessions/{sid}` for it; that GET can only return what this
        // POST just returned, and skipping it removes a round trip, a 404 arm
        // and a second timeout budget from the SessionStart path.
        Ok(r) if r.is_success() => match serde_json::from_slice::<Mirror>(&r.body) {
            Ok(m) => Mirrored::Ok(m),
            // A 2xx we cannot read is a transport-class failure: the daemon
            // answering something we do not understand is the same class of
            // problem as it not answering.
            Err(e) => {
                super::debug(
                    &cfg.hooks,
                    &format!("session_start: unparseable mirror: {e}"),
                );
                Mirrored::Transport
            }
        },
        Ok(r) => {
            super::debug(
                &cfg.hooks,
                &format!("session_start: mirror rejected with {}", r.status),
            );
            Mirrored::Rejected
        }
        Err(http::HttpError::Url(m)) => {
            super::debug(&cfg.hooks, &format!("session_start: {m}"));
            Mirrored::Config
        }
        Err(e) => {
            super::debug(&cfg.hooks, &format!("session_start: mirror failed: {e}"));
            Mirrored::Transport
        }
    }
}

fn none_if_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The enforcement C2a's review asked for, asserted rather than described:
    /// a real `SessionResponse` body carries both cursors, and the optimistic
    /// one does not survive deserialization into this process.
    #[test]
    fn the_mirror_struct_cannot_carry_the_optimistic_cursor() {
        let body = serde_json::json!({
            "bank_id": "claude-code::demo",
            "session_id": "s1",
            "byte_offset": 99999,
            "confirmed_offset": 65536,
            "inflight_bytes": 34463,
            "chunk_index": 2,
            "turns": 20,
        })
        .to_string();
        let m: Mirror = serde_json::from_slice(body.as_bytes()).unwrap();
        assert_eq!(m.confirmed_offset, 65536);
        assert_eq!(m.chunk_index, 2);

        // The mutation this pins: adding `byte_offset` back as a field, or
        // renaming `confirmed_offset` to it. A body carrying **only** the
        // optimistic cursor must recover nothing, because there is nowhere for
        // it to land — 99999 here would be 34463 bytes of transcript skipped.
        let optimistic_only: Mirror = serde_json::from_slice(br#"{"byte_offset":99999}"#).unwrap();
        assert_eq!(optimistic_only.confirmed_offset, 0);
    }

    /// A body missing both cursors is a fresh row, not a failure — and it must
    /// read as offset 0 rather than as "do not recover".
    #[test]
    fn an_absent_cursor_reads_as_zero_and_a_negative_one_does_not_poison_the_chunk() {
        let m: Mirror = serde_json::from_slice(b"{}").unwrap();
        assert_eq!((m.confirmed_offset, m.chunk_index), (0, 0));

        let m: Mirror =
            serde_json::from_slice(br#"{"confirmed_offset":-1,"chunk_index":3}"#).unwrap();
        assert_eq!(m.confirmed_offset.max(0), 0);
        assert_eq!(m.chunk_index, 3, "a bad cursor must not drop the chunk");
    }

    #[test]
    fn empty_payload_fields_are_sent_as_null_rather_than_as_an_empty_string() {
        assert_eq!(none_if_empty(""), None);
        assert_eq!(none_if_empty("startup"), Some("startup"));
    }
}
