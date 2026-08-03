//! `hook retain` — the `Stop` event, and the cursor state machine behind it.
//!
//! **This is the only hook that can lose memory.** `recall` fails open and the
//! turn proceeds with nothing injected; `session-start` fails and the next
//! session's catch-up covers it. Here the failure mode is a cursor that
//! advanced past bytes nothing ingested, and the transcript is the only spool
//! (plan §Binding decisions #9) — so bytes skipped here are gone.
//!
//! # Why there are two cursors, and what this file does with them
//!
//! `POST …/retain` answers **202 = queued, not ingested**. The worker can
//! still fail a chunk (`retain/mod.rs:317-337`), mark the job `Failed`
//! (`:341-347`), and — deliberately — *withhold the content hash* so that
//! re-POSTing the same bytes starts a fresh job rather than being dismissed as
//! a duplicate (`:349-353`). That recovery path is designed and, without the
//! protocol in this file, unreachable: a hook that commits its cursor on the
//! 202 never sends those bytes again, and the daemon's careful arrangement to
//! accept them has no caller.
//!
//! So, per plan §Binding decisions #8:
//!
//! * on accept, the cursor advances **and** a `pending = {job_id, offset_from,
//!   offset_to, chunk_before}` record is written;
//! * on the **next** invocation, `GET /v1/retain/{job_id}` settles it —
//!   `done` clears it, `failed` rolls `offset`/`chunk` back to `offset_from`/
//!   `chunk_before`, anything else leaves it alone and skips the turn.
//!
//! One in-flight job per session, and that bound is load-bearing rather than
//! lazy: `sessions.confirmed_offset` is a single high-water mark, so a job B
//! covering 5000..9000 that finishes before job A covering 0..5000 fails would
//! confirm straight over A's gap and make the loss invisible.
//!
//! // ponytail: one in-flight job per session. A queue — and a
//! // `confirmed_offset` that is a set of intervals rather than a mark — if
//! // retain ever fires per-turn instead of every `retain_every_n_turns`.
//!
//! # Three callers, one state machine
//!
//! [`advance`] is everything from the throttle check to the accept table, and
//! it is called under [`state::with_lock`] by all three:
//!
//! | caller | force | turns |
//! |---|---|---|
//! | `hook retain` (`Stop`) | no — the turn gate decides | `+1` |
//! | `hook retain --force` (the `session-end` child) | yes | unchanged |
//! | `hook catchup` (C2b's child) | yes | unchanged |
//!
//! `turns` counts `Stop` invocations, so neither detached caller increments
//! it: `SessionEnd` is not a `Stop`, and catch-up is not even the session's
//! own process.

use std::path::Path;

use memgarden_core::config::Config;
use memgarden_core::now_ms;
use serde::Deserialize;

use crate::http::{self, HttpError};
use crate::state::{self, Pending, SessionState};
use crate::transcript::{self, Delta};
use crate::{bank, hookio};

/// What the daemon's `RetainResponse` is allowed to tell this hook.
///
/// Same discipline as C2b's `Mirror` and C3's `Reply`: every field defaulted,
/// unknown fields ignored, so a daemon that grows a field cannot turn an
/// accept into a transport failure and stall a cursor over a JSON shape.
///
/// **Both fields are `Option`, and that is not decoration.**
/// `#[serde(default)]` covers a field that is *absent*; it does **not** cover
/// one that is explicitly `null`, which is a type error against `String`. The
/// daemon's `RetainResponse` declares `job_id: Option<String>` and therefore
/// serializes `"job_id": null` on **every `duplicate` and every `skipped`** —
/// so the `String` version of this struct failed to parse exactly the two
/// answers §Failure posture's accept table added, turned them into transport
/// failures, and left the cursor wedged on the response that was supposed to
/// unwedge it. Caught by
/// `duplicate_and_skipped_both_advance_without_a_pending_job`.
#[derive(Debug, Default, Deserialize)]
struct RetainReply {
    /// `accepted` | `duplicate` | `skipped`.
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    job_id: Option<String>,
}

/// What `GET /v1/retain/{job_id}` is allowed to tell the reconciler.
///
/// `status` only. The row also carries `chunks_failed`, which is tempting and
/// wrong to read: a job with one failed chunk out of four is still `done`, and
/// the daemon has already decided what that means for `confirmed_offset`
/// (`retain/mod.rs`'s clean-run block). A hook that second-guessed it would
/// roll back bytes the daemon had confirmed.
#[derive(Debug, Default, Deserialize)]
struct JobReply {
    /// `Option` for `RetainReply`'s reason: an explicit `null` is not what
    /// `#[serde(default)]` covers, and a status this hook cannot parse must
    /// read as "not settled yet", never as a parse failure that then reads as
    /// a transport failure.
    #[serde(default)]
    status: Option<String>,
}

/// What one round trip is allowed to move.
///
/// The split is §Failure posture's, and the two failure counters are the
/// reason it is not one enum arm: `transport_failures` drives the circuit
/// breaker and must never be reachable from an answer the daemon gave, while
/// `reject_failures` drives poisoning and must never be reachable from an
/// outage.
#[derive(Debug)]
enum Outcome {
    /// `202` with a job id. Advance **and** record `pending`.
    Accepted(String),
    /// `200 {"status":"duplicate"}` or `200 {"status":"skipped"}`. Advance,
    /// no `pending`: nothing is outstanding for these bytes.
    Settled,
    /// `202` **without** a job id — a shape the real daemon does not produce.
    ///
    /// The cursor does **not** advance, and no counter moves. Advancing would
    /// be silent loss the moment that job failed, and there is no job id to
    /// discover that it did. Not advancing is at-least-once and self-
    /// terminating: the job either finishes and stamps the content hash, so
    /// the next attempt is answered `duplicate` and advances, or it fails and
    /// the next attempt is the re-send we wanted.
    Unreconcilable,
    /// Connect refused, timeout, unparseable body, `429`, or a 5xx that is not
    /// `503`. `transport_failures += 1`; at `breaker_failures` the breaker
    /// opens.
    Transport,
    /// A durable client-side rejection: a 4xx that is neither `429` (queue
    /// full, retry) nor `404` (bank missing, self-healed). `reject_failures
    /// += 1`; at `max_reject_failures` the session is poisoned.
    Rejected(u16),
    /// `503` — models loading, mid-migration. **Neither counter moves**
    /// (§Failure posture): it is a correct answer and it is fast, and a 9 s
    /// model load must not blind the session for a 60 s cooldown.
    NotReady,
    /// `daemon_url` is not a loopback `http://host:port`. A **config** fault,
    /// so it must not move `transport_failures` — a typo that opened the
    /// circuit breaker would look exactly like an outage in `hooks status`.
    Config,
}

pub fn run(args: &[String]) {
    // argv is either `hook retain` (the `Stop` hook, payload on stdin) or the
    // detached child's
    //   `hook retain --force --session <sid> --end-reason <reason>`.
    //
    // **Fixed slots, and the two untrusted values only ever occupy value
    // positions.** C2b's review demonstrated the alternative against the real
    // binary: scanning argv for `--dry-run` let a *session id* of `--dry-run`
    // empty the exclusion set, turning a filter off by its own subject. Here
    // the untrusted values are the session id (stdin) and the end reason
    // (stdin), and both would be sitting next to a `--force` that decides
    // whether the turn gate applies. Neither gets to choose a slot.
    let force = args.get(2).map(String::as_str) == Some("--force");
    let session_arg = flag_value(args, 3, "--session");
    let end_reason = flag_value(args, 5, "--end-reason").unwrap_or("");

    // The child is spawned with `Stdio::null()` on all three streams, so it
    // has no payload at all — everything it knows comes from argv and from the
    // state file. That is why `--session` exists: plan §C4b spells the child
    // `hook retain --force --end-reason <reason>`, which names no session, and
    // a `SessionEnd` child that cannot identify its session cannot retain it.
    let (session_id, input) = if force {
        match session_arg {
            Some(id) => (id.to_string(), None),
            None => return,
        }
    } else {
        let Some(input) = hookio::read_stdin() else {
            return;
        };
        (input.session_id.clone(), Some(input))
    };

    let Some(cfg) = super::enabled_config() else {
        return;
    };
    if session_id.is_empty() || session_id.len() > super::MAX_SESSION_ID_BYTES {
        super::debug(&cfg.hooks, "retain: unusable session_id");
        return;
    }

    let dir = cfg.hooks.state_dir.as_path();
    let now = now_ms();
    let bank_id = state::with_lock(dir, &session_id, || {
        let mut st = match state::load(dir, &session_id) {
            Some(st) => st,
            None => {
                // §Failure posture: a missing or corrupt state file is
                // "start over" — offset 0, `is_initial = true`, and the
                // daemon's backfill cap bounds the payload.
                //
                // The forced child has no payload, so it has no `cwd` to
                // derive a bank from and nothing to start over *as*. Its
                // session is one `session-start` never recorded, which the
                // next session's catch-up cannot help with either; there is
                // nothing here to do but exit.
                let input = input.as_ref()?;
                let project_dir = std::env::var("CLAUDE_PROJECT_DIR").ok();
                let derived = bank::derive(&cfg.hooks, input.project_dir(project_dir.as_deref()));
                SessionState::new(&session_id, &derived)
            }
        };

        if let Some(input) = input.as_ref() {
            // Refreshed from the payload, and only when non-empty: these are
            // the detached children's only handles on the session, and
            // overwriting a good stored value with an absent one would cost
            // catch-up the file it was going to read.
            //
            // `bank_id` is deliberately **not** refreshed — C2b's rule: a
            // session's cursor belongs to the bank its bytes were posted to.
            if !input.transcript_path.is_empty() {
                input.transcript_path.clone_into(&mut st.transcript_path);
            }
            if !input.cwd.is_empty() {
                input.cwd.clone_into(&mut st.cwd);
            }
            st.turns = st.turns.saturating_add(1);
            st.turns_since_retain = st.turns_since_retain.saturating_add(1);
        }

        advance(&cfg, &mut st, force, now);

        if let Err(e) = state::store(dir, &st) {
            super::debug(&cfg.hooks, &format!("retain: state write failed: {e}"));
        }
        Some(st.bank_id)
    });

    // The `SessionEnd` half, outside the lock: the session row's `end_reason`
    // and `ended_at`. Last, because the retain above is the part that can lose
    // something and this one is a label on a row.
    if force && let Ok(Some(bank_id)) = bank_id {
        report_end(&cfg, &bank_id, &session_id, end_reason, now);
    }
}

/// `args[at]` is exactly `name` and `args[at + 1]` exists.
///
/// A fixed slot, not a scan: see [`run`]. A flag in the wrong position is not
/// silently accepted somewhere else, it is simply absent.
fn flag_value<'a>(args: &'a [String], at: usize, name: &str) -> Option<&'a str> {
    (args.get(at).map(String::as_str) == Some(name))
        .then(|| args.get(at + 1))?
        .map(String::as_str)
}

/// The whole state machine: throttles, reconciliation, the turn gate, the
/// read, the POST, and the accept table.
///
/// The caller holds the session's lock, has already loaded `st`, and stores it
/// afterwards **whatever this returns** — every early exit here still has
/// counters or a cursor worth persisting.
///
/// `force` bypasses the turn gate and nothing else. In particular it does not
/// bypass the breaker or the poison throttle: a `SessionEnd` against a daemon
/// that has durably rejected this session ten times is not the moment to try
/// an eleventh time inside the hour.
pub fn advance(cfg: &Config, st: &mut SessionState, force: bool, now_ms: i64) {
    // ---- 1. Throttles, before anything opens a socket.
    //
    // **Ahead of the reconcile, which plan §C4b puts first.** The plan's order
    // makes the breaker unreachable from the one path that most needs it: a
    // *hung* daemon answers the reconcile `GET` with `recall_timeout_ms` of
    // silence on every `Stop`, and a breaker checked afterwards never gets to
    // skip a socket it has already opened. The breaker's whole measured value
    // (C3: 1536 ms per prompt without one) is that it is checked *before* the
    // connect.
    if super::breaker_open(st, &cfg.hooks, now_ms) {
        super::debug(&cfg.hooks, "retain: breaker open, skipping");
        return;
    }
    if super::poisoned_within_throttle(st, cfg.hooks.poison_retry_secs, now_ms) {
        super::debug(&cfg.hooks, "retain: poisoned, inside the retry window");
        return;
    }

    // ---- 2. Reconcile the previous accept.
    //
    // Costs nothing on 9 of every 10 `Stop`s, because `pending` is `None`
    // except in the window between an accept and its settlement.
    if !reconcile(cfg, st) {
        return;
    }

    // ---- 3. The turn gate. This is where 9 of every 10 `Stop`s stop.
    if !force && st.turns_since_retain < cfg.hooks.retain_every_n_turns {
        return;
    }

    // ---- 4. `stat`, and the guard that this is a transcript at all.
    let Some(size) = regular_file_len(&st.transcript_path) else {
        super::debug(&cfg.hooks, "retain: no readable regular transcript");
        return;
    };
    // §Binding decisions #6: a file that shrank was genuinely rewritten, so
    // start over. It only half-covers — a rewrite to an equal or greater
    // length leaves `size >= offset` while the offset points mid-content — and
    // it costs one comparison, which is why it is here anyway.
    if size < st.offset {
        st.offset = 0;
    }
    if size <= st.offset {
        return;
    }

    // ---- 5. The delta.
    let from = st.offset;
    let delta = transcript::read_delta(
        Path::new(&st.transcript_path),
        from,
        cfg.hooks.max_post_bytes,
    );
    // Counted before the send decision, because the lines have been read
    // either way and `compactions` is cumulative on the wire. It is a lower
    // bound by construction: a rollback re-counts the boundaries in the
    // re-sent delta, which is why the daemon merges the column with `MAX`.
    st.compactions = st.compactions.saturating_add(delta.compactions);

    if delta.messages.is_empty() {
        // We consumed only lines the reader skips. **Advance anyway**: not
        // advancing re-scans them on every retain turn for the rest of the
        // session, and the daemon would answer `skipped` if we sent them.
        st.offset = delta.consumed_to;
        // And restart the cadence, which the plan does not say and which is
        // what keeps "one delta read per `retain_every_n_turns` `Stop`s" true.
        // Leaving it high makes every subsequent `Stop` pass the gate and
        // re-read the tail — the 0.30 ms gated path becoming a 0.32 ms read
        // on every turn, for a session that has nothing to say.
        st.turns_since_retain = 0;
        return;
    }

    // ---- 6. The POST, and the accept table.
    let outcome = post_delta(cfg, st, &delta, from, now_ms);
    apply(cfg, st, &outcome, from, delta.consumed_to, now_ms);
}

/// Settles `pending`. Returns whether the caller may proceed to a new POST.
///
/// Not proceeding is the point of three of the five arms: stacking a second
/// unconfirmed job on an unconfirmed cursor is exactly what makes
/// `confirmed_offset` unable to describe which bytes are missing.
fn reconcile(cfg: &Config, st: &mut SessionState) -> bool {
    let Some(pending) = st.pending.clone() else {
        return true;
    };
    match job_status(cfg, &pending.job_id) {
        JobOutcome::Done => {
            st.pending = None;
            true
        }
        // The whole reason C1 carries two cursors. `offset_from` and
        // `chunk_before` are restored together: the re-send has to reuse the
        // same `document_id`, or the failed delta's provenance row is
        // orphaned and the next one overwrites a chunk that never landed.
        //
        // `compactions` is deliberately **not** rolled back — see `advance`.
        JobOutcome::Failed => {
            super::debug(
                &cfg.hooks,
                &format!(
                    "retain: job {} failed, rolling back to {}",
                    pending.job_id, pending.offset_from
                ),
            );
            st.offset = pending.offset_from;
            st.chunk = pending.chunk_before;
            st.pending = None;
            // Proceeds to the **turn gate**, not straight to a re-send. A
            // rolled-back delta that re-POSTed immediately would, under the
            // chunk-failure storm §Open questions 6 predicts for a shadow
            // run, put an expensive `prepare()` on every single `Stop`. The
            // gate re-sends within `retain_every_n_turns`, and `session-end`
            // forces it regardless — neither of which can lose the bytes,
            // because the transcript is still the spool.
            true
        }
        JobOutcome::Unsettled => false,
        JobOutcome::Unreachable => {
            // A round trip that failed is a transport failure like any other:
            // without this, a down daemon during reconciliation would never
            // open the breaker and every `Stop` would pay a fresh connect.
            count_transport_failure(cfg, st, now_ms());
            false
        }
    }
}

enum JobOutcome {
    Done,
    Failed,
    /// `pending`, `running`, or a status word we do not recognise.
    Unsettled,
    /// The daemon could not be reached or would not answer usefully.
    Unreachable,
}

fn job_status(cfg: &Config, job_id: &str) -> JobOutcome {
    let Ok(target) = super::target(&cfg.hooks) else {
        return JobOutcome::Unreachable;
    };
    // The first path segment in this crate that comes from **the daemon**
    // rather than from config or stdin — C2a's guard in `http::request`
    // predicted this caller by name. Encoded for the same reason bank ids are:
    // a raw space or CR in a request line is request splitting, not a 400.
    let path = format!("/v1/retain/{}", http::encode_path_segment(job_id));
    // The interactive budget, not the retain one: this is a single-row read,
    // and it runs on gated turns where 5 s of a hung daemon would be five
    // seconds of a `Stop`.
    match http::get(&target, &path, &super::interactive_timeouts(&cfg.hooks)) {
        Ok(r) if r.is_success() => {
            let reply: JobReply = serde_json::from_slice(&r.body).unwrap_or_default();
            match reply.status.unwrap_or_default().as_str() {
                "done" => JobOutcome::Done,
                "failed" => JobOutcome::Failed,
                _ => JobOutcome::Unsettled,
            }
        }
        // **The job row is gone.** Nothing will ever settle it, so treating it
        // as unsettled wedges this session's cursor for the rest of its life.
        // `failed` is the safe reading: re-sending is at-least-once and the
        // content-hash dedup answers `duplicate` if the job did in fact
        // finish. The plan's reconcile has no arm for this.
        Ok(r) if r.status == 404 => JobOutcome::Failed,
        Ok(r) => {
            super::debug(
                &cfg.hooks,
                &format!("retain: job lookup answered {}", r.status),
            );
            JobOutcome::Unreachable
        }
        Err(e) => {
            super::debug(&cfg.hooks, &format!("retain: job lookup failed: {e}"));
            JobOutcome::Unreachable
        }
    }
}

/// `POST /v1/banks/{bank}/retain`, with the one self-heal the plan allows:
/// a `404` creates the bank and retries **once**.
fn post_delta(cfg: &Config, st: &SessionState, delta: &Delta, from: u64, now_ms: i64) -> Outcome {
    let target = match super::target(&cfg.hooks) {
        Ok(t) => t,
        Err(HttpError::Url(m)) => {
            super::debug(&cfg.hooks, &format!("retain: {m}"));
            return Outcome::Config;
        }
        // An unreadable `<data>/daemon.token` is transport-class, not config:
        // see `cmd::target`.
        Err(e) => {
            super::debug(&cfg.hooks, &format!("retain: {e}"));
            return Outcome::Transport;
        }
    };

    // §Binding decisions #7. `chunk 0` -> the bare `session_id`; `chunk N > 0`
    // -> `session_id-cN` (`retain.py:154`).
    //
    // **Legacy's stated reason for the suffix does not apply here and must not
    // be repeated.** `retain.py:110-116` says reusing a `document_id`
    // "replaces the server-side document"; that is true of legacy's server.
    // Ours UPDATEs and keeps the row id (`documents.rs:55-70`) and
    // `routes/retain.rs:395-409` rebuilds `documents.metadata` from scratch
    // every time — so without the suffix, each delta's `message_count` and
    // `files_modified` **overwrite the previous delta's**. The suffix protects
    // per-delta provenance, not facts.
    let document_id = if st.chunk == 0 {
        st.session_id.clone()
    } else {
        format!("{}-c{}", st.session_id, st.chunk)
    };

    let body = serde_json::json!({
        "messages": delta.messages,
        "session_id": st.session_id,
        "cwd": none_if_empty(&st.cwd),
        // The session's **first** retain, which is what gates the daemon's
        // backfill cap. `from`, not `st.offset` after the fact, and not a
        // hardcoded `false` on the catch-up path — see the design note.
        "is_initial": from == 0,
        "document_id": document_id,
        "event_date": now_ms,
        // The optimistic cursor, mirrored: `Delta::consumed_to`, a line
        // boundary, never the file size.
        "byte_offset": delta.consumed_to,
        // All four mirror fields are cumulative absolutes from our own state
        // file, merged `MAX` server-side — not per-request increments.
        "turn": st.turns,
        "chunk": st.chunk,
        // Plural, and cumulative. Plan §C4b spells this `compaction` and omits
        // `chunk` entirely; the daemon's `RetainRequest` has neither of those
        // shapes, and a field name it does not recognise is silently dropped.
        "compactions": st.compactions,
        "metadata": {
            // The delta's span in the transcript, which is what pairs with
            // `truncated`: the two together say "we covered these bytes, and
            // the oversize fallback dropped some of them".
            "transcript_bytes": delta.consumed_to.saturating_sub(from),
            // Set when the oversize fallback dropped leading messages. The
            // daemon never sees those bytes, so `retain_cap_saving`
            // under-reports for this row and this flag is how a reader knows.
            "truncated": delta.truncated,
        },
    })
    .to_string();
    let path = format!(
        "/v1/banks/{}/retain",
        http::encode_path_segment(&st.bank_id)
    );
    let timeouts = super::retain_timeouts(&cfg.hooks);

    let first = classify(cfg, http::post(&target, &path, body.as_bytes(), &timeouts));
    if !matches!(first, Outcome::Rejected(404)) {
        return first;
    }

    // The bank is missing — the one 4xx that self-heals. `session-start`
    // creates it, but a state dir that outlived a database, or a session that
    // started while the daemon was down, both land here. Created without a
    // `mission`: `routes/banks.rs::create_bank` applies the daemon's own
    // `[profile] bank_mission`, and the daemon owns mission precedence (C2b).
    super::debug(&cfg.hooks, "retain: bank missing, creating it and retrying");
    let _ = http::post(
        &target,
        "/v1/banks",
        serde_json::json!({ "bank_id": st.bank_id })
            .to_string()
            .as_bytes(),
        &super::interactive_timeouts(&cfg.hooks),
    );
    // **Exactly once.** A second 404 after a create is a durable rejection,
    // and retrying it every turn is how a self-heal becomes a hot loop.
    classify(cfg, http::post(&target, &path, body.as_bytes(), &timeouts))
}

/// The accept table of §Failure posture, as a `match`.
fn classify(cfg: &Config, result: Result<http::Response, HttpError>) -> Outcome {
    let response = match result {
        Ok(r) => r,
        Err(HttpError::Url(m)) => {
            super::debug(&cfg.hooks, &format!("retain: {m}"));
            return Outcome::Config;
        }
        Err(e) => {
            super::debug(&cfg.hooks, &format!("retain: post failed: {e}"));
            return Outcome::Transport;
        }
    };
    let reply = |r: &http::Response| serde_json::from_slice::<RetainReply>(&r.body).ok();
    match response.status {
        // Queued, not ingested. The job id is what makes it reconcilable, and
        // a 202 without one is deliberately not an advance.
        202 => match reply(&response)
            .and_then(|r| r.job_id)
            .filter(|id| !id.is_empty())
        {
            Some(job_id) => Outcome::Accepted(job_id),
            None => {
                super::debug(&cfg.hooks, "retain: 202 carried no job id; not advancing");
                Outcome::Unreconcilable
            }
        },
        200 => match reply(&response).and_then(|r| r.status) {
            // The daemon already has these exact bytes.
            Some(s) if s == "duplicate" => Outcome::Settled,
            // "Nothing here worth keeping" is precisely when advancing is
            // right. `plan_ingest` returns `None` for an empty role-filtered
            // set or a <10-character transcript
            // (`retain/transcript.rs:127-134`), which is ordinary with
            // `include_tool_calls = false`. Not accepting it re-sent the same
            // delta forever and poisoned the session after ten tries —
            // losing every *subsequent* real delta, not just the skippable one.
            Some(s) if s == "skipped" => Outcome::Settled,
            other => {
                super::debug(
                    &cfg.hooks,
                    &format!("retain: unreadable 200 ({other:?}); not advancing"),
                );
                Outcome::Transport
            }
        },
        // Queue full, or the byte budget exhausted. Transport-class: retry
        // next turn, and never poison — the daemon is busy, not offended.
        429 => Outcome::Transport,
        // Models loading, mid-migration, or the retain worker not yet running.
        // Neither counter moves.
        503 => Outcome::NotReady,
        // Handled by `post_delta`'s single retry, and durable if it recurs.
        status if (400..500).contains(&status) => Outcome::Rejected(status),
        // A 5xx that is not 503 is a daemon fault, not a durable client-side
        // rejection — `reject_failures` is defined as the latter, so a 500
        // must not be able to poison a session.
        status => {
            super::debug(&cfg.hooks, &format!("retain: server error {status}"));
            Outcome::Transport
        }
    }
}

/// Applies one outcome to the cursor and the counters.
fn apply(
    cfg: &Config,
    st: &mut SessionState,
    outcome: &Outcome,
    from: u64,
    consumed_to: u64,
    now_ms: i64,
) {
    match outcome {
        Outcome::Accepted(job_id) => {
            // `chunk_before` is captured **before** the increment: it is what
            // a `failed` job rolls back to, and rolling back to the
            // incremented value would change the `document_id` on the re-send.
            st.pending = Some(Pending {
                job_id: job_id.clone(),
                offset_from: from,
                offset_to: consumed_to,
                chunk_before: st.chunk,
            });
            accept(st, consumed_to);
        }
        Outcome::Settled => {
            st.pending = None;
            accept(st, consumed_to);
        }
        // The cursor stays put and nothing is counted: see the variant.
        Outcome::Unreconcilable => {}
        Outcome::Transport => count_transport_failure(cfg, st, now_ms),
        Outcome::Rejected(status) => {
            super::debug(&cfg.hooks, &format!("retain: rejected with {status}"));
            st.reject_failures = st.reject_failures.saturating_add(1);
            if st.reject_failures >= cfg.hooks.max_reject_failures {
                // A slow-retry state, not a latch. `hooks status
                // --clear-poison <sid>` (C5) is the manual exit; any success
                // is the automatic one.
                st.poisoned_at = Some(now_ms);
            }
        }
        Outcome::NotReady | Outcome::Config => {}
    }
}

/// An accepted delta: advance, count the chunk, restart the cadence, and clear
/// every failure state.
///
/// `chunk` increments on **every** accept, including `duplicate` and
/// `skipped`. Not incrementing on those would make the next delta reuse this
/// one's `document_id`, which is the provenance overwrite §Binding decisions
/// #7 exists to prevent — and the daemon has already committed a row under it.
fn accept(st: &mut SessionState, consumed_to: u64) {
    st.offset = consumed_to;
    st.chunk = st.chunk.saturating_add(1);
    st.turns_since_retain = 0;
    // Any answer at all clears the breaker; an *accepted* answer also clears
    // poisoning, which is what makes poisoning a slow-retry state rather than
    // a latch.
    st.transport_failures = 0;
    st.reject_failures = 0;
    st.breaker_open_until_ms = 0;
    st.poisoned_at = None;
}

fn count_transport_failure(cfg: &Config, st: &mut SessionState, now_ms: i64) {
    st.transport_failures = st.transport_failures.saturating_add(1);
    if st.transport_failures >= cfg.hooks.breaker_failures {
        st.breaker_open_until_ms = now_ms.saturating_add(super::breaker_cooldown_ms(&cfg.hooks));
    }
}

/// The transcript's size, and the guard that it is a transcript at all.
///
/// **Validated at the read, not at store time.** C2b decided this deliberately:
/// `retain` reads the *payload's* `transcript_path` on every `Stop`, so a
/// store-time guard would cover the once-per-session path and leave the
/// once-per-ten-turns path wide open. One guard, where every reader passes.
///
/// It checks a **property, not a spelling**. A `.jsonl` allowlist constrains a
/// vocabulary Claude Code owns and would break on the next rename; "is this a
/// regular file" is the thing that actually matters, and it refuses a
/// directory, a device, a socket and a fifo.
///
/// `std::fs::metadata` rather than open-then-`fstat`, which is what the
/// obvious reading of "check the handle" would be: **opening a fifo blocks
/// until a writer appears**, so the check has to settle the file type
/// *before* anything opens it, and `O_NONBLOCK` needs `libc`, which this
/// crate's CI-enforced dependency closure refuses. It also does double duty —
/// `advance` needs the size for the `size < offset` reset anyway, so this is
/// one syscall doing both jobs rather than two doing one each.
///
/// // ponytail: the residual is a swap between this `stat` and `read_delta`'s
/// // `open`, which needs write access to the transcript's own directory — at
/// // which point the attacker can simply write the transcript. `O_NOFOLLOW |
/// // O_NONBLOCK` on the open plus an `fstat` on the handle is the airtight
/// // version, and it costs `libc`.
fn regular_file_len(path: &str) -> Option<u64> {
    if path.is_empty() {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    meta.is_file().then_some(meta.len())
}

/// `POST /v1/banks/{bank}/sessions` carrying only `end_reason` and `ended_at`.
///
/// The daemon merges cumulative fields with `MAX` and takes the last write for
/// `end_reason`, so sending nothing else is not an omission — it is what keeps
/// this call from being able to move a cursor.
fn report_end(cfg: &Config, bank_id: &str, session_id: &str, reason: &str, now_ms: i64) {
    let Ok(target) = super::target(&cfg.hooks) else {
        return;
    };
    let body = serde_json::json!({
        "session_id": session_id,
        // `null` rather than `""`: the daemon reads `Option<String>` and
        // leaves the column alone for `null`, while an empty string would
        // clobber a reason a previous `SessionEnd` recorded correctly.
        "end_reason": none_if_empty(reason),
        "ended_at": now_ms,
    })
    .to_string();
    let path = format!("/v1/banks/{}/sessions", http::encode_path_segment(bank_id));
    if let Err(e) = http::post(
        &target,
        &path,
        body.as_bytes(),
        &super::interactive_timeouts(&cfg.hooks),
    ) {
        super::debug(
            &cfg.hooks,
            &format!("retain: session end update failed: {e}"),
        );
    }
}

fn none_if_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| (*s).to_string()).collect()
    }

    /// The C2b trap, on C4b's argv: an untrusted value in a flag position
    /// silently changes a decision. `--force` is read from a **fixed slot**,
    /// so an `end_reason` or a `session_id` spelled `--force` cannot reach it.
    #[test]
    fn a_flag_is_only_a_flag_in_its_own_slot() {
        let child = args(&[
            "hook",
            "retain",
            "--force",
            "--session",
            "s1",
            "--end-reason",
            "clear",
        ]);
        assert_eq!(child.get(2).map(String::as_str), Some("--force"));
        assert_eq!(flag_value(&child, 3, "--session"), Some("s1"));
        assert_eq!(flag_value(&child, 5, "--end-reason"), Some("clear"));

        // A session id that is itself a flag name occupies the value slot and
        // is read as a value — the exclusion-filter defect C2b demonstrated,
        // in its C4b shape.
        let hostile = args(&[
            "hook",
            "retain",
            "--force",
            "--session",
            "--force",
            "--end-reason",
            "--session",
        ]);
        assert_eq!(flag_value(&hostile, 3, "--session"), Some("--force"));
        assert_eq!(flag_value(&hostile, 5, "--end-reason"), Some("--session"));

        // A flag in the wrong slot is absent, not found elsewhere.
        let scattered = args(&["hook", "retain", "--session", "s1", "--force"]);
        assert_eq!(scattered.get(2).map(String::as_str), Some("--session"));
        assert_eq!(flag_value(&scattered, 3, "--session"), None);
        // The plain `Stop` invocation has neither.
        let plain = args(&["hook", "retain"]);
        assert_eq!(plain.get(2), None);
        assert_eq!(flag_value(&plain, 3, "--session"), None);
        // A flag with no value is not a flag with an empty value.
        let dangling = args(&["hook", "retain", "--force", "--session"]);
        assert_eq!(flag_value(&dangling, 3, "--session"), None);
    }

    /// The property, not the spelling. A `.jsonl` name proves nothing and a
    /// regular file with any name is readable.
    #[test]
    fn only_a_regular_file_is_a_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("transcript.jsonl");
        std::fs::write(&real, b"0123456789").unwrap();
        assert_eq!(regular_file_len(&real.to_string_lossy()), Some(10));

        // A name Claude Code does not use is still a regular file.
        let odd = dir.path().join("t.log");
        std::fs::write(&odd, b"xy").unwrap();
        assert_eq!(regular_file_len(&odd.to_string_lossy()), Some(2));

        // A directory named like a transcript is not one.
        let masquerade = dir.path().join("dir.jsonl");
        std::fs::create_dir(&masquerade).unwrap();
        assert_eq!(regular_file_len(&masquerade.to_string_lossy()), None);

        assert_eq!(regular_file_len(""), None);
        assert_eq!(
            regular_file_len(&dir.path().join("gone.jsonl").to_string_lossy()),
            None
        );
    }

    /// A fifo is the case the check has to settle **before** anything opens
    /// it: `File::open` on a fifo with no writer blocks forever, in a hook
    /// whose contract is that it always exits.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_being_opened() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("transcript.jsonl");
        // `mkfifo` via the shell: `libc` is not in this crate's dependency
        // budget, and the test needs the node, not the binding.
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            return; // no mkfifo on this box; the directory case still covers the type check
        }
        // Returns, rather than hanging the test runner forever.
        assert_eq!(regular_file_len(&fifo.to_string_lossy()), None);
    }

    /// `document_id` is the bare session id on chunk 0 and suffixed after.
    /// Asserted through the real formatting, because the mutation that
    /// matters — suffixing chunk 0 too, or dropping the suffix — is a
    /// one-character edit that no cursor assertion can see.
    #[test]
    fn the_document_id_is_bare_on_chunk_zero_and_suffixed_after() {
        let mut st = SessionState::new("sess-1", "b1");
        let id = |st: &SessionState| {
            if st.chunk == 0 {
                st.session_id.clone()
            } else {
                format!("{}-c{}", st.session_id, st.chunk)
            }
        };
        assert_eq!(id(&st), "sess-1");
        st.chunk = 1;
        assert_eq!(id(&st), "sess-1-c1");
        st.chunk = 42;
        assert_eq!(id(&st), "sess-1-c42");
    }

    /// Every accept clears every failure state, and every accept moves the
    /// chunk. A mutant that clears only `transport_failures` leaves a poisoned
    /// session poisoned through a successful retain.
    #[test]
    fn an_accept_advances_the_chunk_and_clears_every_failure_state() {
        let mut st = SessionState::new("s1", "b1");
        st.chunk = 3;
        st.offset = 100;
        st.turns_since_retain = 10;
        st.transport_failures = 2;
        st.reject_failures = 9;
        st.breaker_open_until_ms = 12_345;
        st.poisoned_at = Some(678);

        accept(&mut st, 4096);

        assert_eq!(st.offset, 4096);
        assert_eq!(st.chunk, 4);
        assert_eq!(st.turns_since_retain, 0);
        assert_eq!(st.transport_failures, 0);
        assert_eq!(st.reject_failures, 0);
        assert_eq!(st.breaker_open_until_ms, 0);
        assert_eq!(st.poisoned_at, None);
    }

    /// A reply missing every field is still a reply, and an unknown status is
    /// not silently an accept.
    #[test]
    fn the_reply_shapes_default_rather_than_fail() {
        let reply: RetainReply = serde_json::from_slice(b"{}").unwrap();
        assert_eq!((reply.status, reply.job_id), (None, None));

        // **The shape the daemon really sends on `duplicate` and `skipped`.**
        // `#[serde(default)]` does not cover an explicit `null`, so a `String`
        // field here failed to parse the two answers the accept table exists
        // for — and a failed parse is a transport failure, i.e. the cursor
        // stayed wedged on the response designed to unwedge it.
        let reply: RetainReply = serde_json::from_slice(
            br#"{"status":"duplicate","job_id":null,"document_id":7,
                 "raw_tokens":0,"capped_tokens":0,"saved_tokens":0,"saving_ratio":0.0}"#,
        )
        .expect("an explicit null must parse");
        assert_eq!(reply.status.as_deref(), Some("duplicate"));
        assert_eq!(reply.job_id, None);

        // The real 202 body, including fields this hook does not read.
        let reply: RetainReply = serde_json::from_slice(
            br#"{"status":"accepted","job_id":"019-abc","document_id":7,
                 "raw_tokens":900,"capped_tokens":300,"saved_tokens":600,
                 "saving_ratio":0.667}"#,
        )
        .unwrap();
        assert_eq!(reply.status.as_deref(), Some("accepted"));
        assert_eq!(reply.job_id.as_deref(), Some("019-abc"));

        let job: JobReply = serde_json::from_slice(
            br#"{"job_id":"x","status":"failed","chunks_failed":1,"error":"ollama down"}"#,
        )
        .unwrap();
        assert_eq!(job.status.as_deref(), Some("failed"));
        assert_eq!(JobReply::default().status, None);
        // The job row carries `"error": null` on every clean job.
        let job: JobReply = serde_json::from_slice(br#"{"status":"done","error":null}"#).unwrap();
        assert_eq!(job.status.as_deref(), Some("done"));
    }

    /// `none_if_empty` is what keeps an absent `cwd` from clobbering a stored
    /// one — the unpinned survivor C2b's review named.
    #[test]
    fn empty_strings_are_sent_as_null_rather_than_as_an_empty_string() {
        assert_eq!(none_if_empty(""), None);
        assert_eq!(none_if_empty("/repo"), Some("/repo"));
        assert_eq!(
            serde_json::json!({ "cwd": none_if_empty("") }).to_string(),
            r#"{"cwd":null}"#
        );
    }
}
