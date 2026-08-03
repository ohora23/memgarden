//! `hook catchup` — the detached child `session-start` spawns.
//!
//! This is what makes "retain fails closed" survivable across a whole-session
//! outage. `retain` never spools a payload, because the transcript already is
//! one (plan §Binding decisions #9); a session whose daemon was down for its
//! entire life leaves a state file whose `offset` is far behind the file, and
//! **the next session's catch-up is the last thing that will ever look at it**.
//!
//! It runs detached with `/dev/null` on all three streams, so nothing it does
//! can reach the model's context or hold the parent's pipe open. That also
//! means it is unobservable by construction, which is why `--dry-run` exists:
//! run by hand, it prints what the child would have selected and touches
//! nothing.
//!
//! # What the child does
//!
//! It collects the state directory, **selects**, and — since C4b — posts each
//! selected session's delta through `retain::advance`.
//!
//! The posting half was deliberately not in C2b. It needs C4a's transcript
//! reader and C4b's `pending` reconciliation, and the second of those is not
//! optional sequencing: a POST without it commits the cursor on a bare 202,
//! which the plan's own sequencing note calls out as turning silent loss "from
//! a rare event into the normal case".
//!
//! **`is_initial` is `offset == 0`, not the hardcoded `false` the plan's C2b
//! line specifies.** Catch-up at offset 0 *is* a session's first retain — a
//! whole transcript, the largest payload in the system — so `false` there
//! takes the daemon's **uncapped** branch on exactly the shape
//! `retain.max_initial_messages` exists to bound. `retain::advance` derives it
//! from the cursor, once, for all three callers.

use std::path::Path;

use memgarden_core::config::HooksConfig;
use memgarden_core::now_ms;

use crate::state::{self, SessionState};

/// A session catch-up would work, and the two facts that made it a candidate.
#[derive(Debug)]
pub struct Candidate {
    pub state: SessionState,
    /// The transcript's size right now. `> state.offset` is the whole reason
    /// this session is here.
    pub file_size: u64,
    /// Transcript mtime, for ordering. Most recently active first, because
    /// `catchup_max_sessions` truncates and the newest outage is the one whose
    /// content the user is most likely to ask about.
    pub modified_ms: i64,
}

pub fn run(args: &[String]) {
    // argv: `hook catchup <current_session_id> [--dry-run]`. Hand-parsed, like
    // everything else in this binary — `clap`'s usage errors exit 2.
    //
    // **Both slots are fixed positions, and argv[2] is the session id whatever
    // it looks like.** The first version skipped a `--`-prefixed argv[2] and
    // scanned the whole of argv for `--dry-run`, which review demonstrated
    // against the real binary: a session id of `--dry-run` made `excluded`
    // empty, so the live session became a catch-up candidate — the exclusion
    // filter turned *off* by its own subject. The id arrives on untrusted
    // stdin; it does not get to choose which argv slot it occupies.
    let current_session_id = args.get(2).map(String::as_str).unwrap_or("");
    let dry_run = args.get(3).is_some_and(|a| a == "--dry-run");

    let Some(cfg) = super::enabled_config() else {
        return;
    };
    let dir = cfg.hooks.state_dir.as_path();

    // Housekeeping first, so a session collected here is never selected below.
    //
    // C2a shipped `state::gc` with **no caller at all**: one file per session,
    // forever, in a directory nothing pruned. This child is the right place
    // for it — once per session, off every latency budget, already reading the
    // directory — and it runs regardless of `catchup_max_sessions`.
    //
    // That last part is a deliberate deviation from the plan, which gates the
    // whole child on `catchup_max_sessions > 0`. A knob that means "work
    // through fewer stale sessions" must not also mean "leak a state file per
    // session forever"; `0` here means housekeeping only.
    let cutoff_ms = now_ms().saturating_sub(days_ms(cfg.hooks.session_retention_days));
    let collected = state::gc(dir, cutoff_ms).unwrap_or(0);

    let picked = select(dir, current_session_id, &cfg.hooks, now_ms());

    if dry_run {
        // Unpadded on purpose: this is the only window into a process that
        // otherwise writes to `/dev/null`, so it is also what the tests and
        // the runbook grep.
        println!("state_dir {}", dir.display());
        println!("excluded {current_session_id}");
        println!("collected {collected}");
        println!("selected {}", picked.len());
        for c in &picked {
            println!(
                "  {} bank={} offset={} size={} behind={} transcript={}",
                c.state.session_id,
                c.state.bank_id,
                c.state.offset,
                c.file_size,
                c.file_size.saturating_sub(c.state.offset),
                c.state.transcript_path,
            );
        }
    }

    // The marker C2b left, honoured: **per candidate, re-load under
    // `state::with_lock` and re-check `offset < file_size` before posting**,
    // and validate `transcript_path` at the point of the read.
    //
    // The re-load is not defensive padding. `Candidate` is a **selection
    // snapshot**: `state` came from `load_all` outside any lock and
    // `file_size` is a `stat` from the same moment. Posting from it means
    // posting a cursor that a live session's own retain hook may already have
    // moved — a session live in *another Claude Code window* passes every
    // filter in `select`, so the exclusion above removes one instance of the
    // race and not the class. `retain::advance` does both halves inside the
    // lock: it re-`stat`s the transcript and returns on `size <= offset`.
    //
    // `--dry-run` is the only window into a process whose three streams are
    // `/dev/null`, so it must stay observation-only.
    if dry_run {
        return;
    }
    for candidate in &picked {
        let session_id = candidate.state.session_id.as_str();
        let _ = state::with_lock(dir, session_id, || {
            let Some(mut st) = state::load(dir, session_id) else {
                return;
            };
            // `force`: the turn gate counts *this* session's `Stop`s, and
            // catch-up is not one of them — the session it is working on may
            // not have had a `Stop` in days. The breaker and the poison
            // throttle still apply; `advance` checks both.
            super::retain::advance(&cfg, &mut st, true, now_ms());
            if let Err(e) = state::store(dir, &st) {
                super::debug(&cfg.hooks, &format!("catchup: state write failed: {e}"));
            }
        });
    }
}

/// The sessions catch-up would work, most recently active first, at most
/// `catchup_max_sessions`.
///
/// Pure apart from the filesystem reads, and separated from [`run`] for the
/// reason every predicate here exists: each one of them is a silent, once-per-
/// launch failure if it is wrong, in a process whose output goes to
/// `/dev/null`.
pub fn select(
    dir: &Path,
    current_session_id: &str,
    cfg: &HooksConfig,
    now_ms: i64,
) -> Vec<Candidate> {
    let mut found: Vec<Candidate> = state::load_all(dir)
        .into_iter()
        // **The current session is excluded.** A `source: resume`
        // `SessionStart` otherwise has catch-up and the live retain hook
        // working one cursor concurrently: two of our own processes, so
        // `File::lock()` does serialize them — but serializing a
        // read-modify-write does not make "catch-up posts 0..N while retain
        // posts M..N" correct, it only makes the two writes atomic.
        //
        // This filter is **not** a complete answer to that race and must not
        // be described as one. A session live in *another Claude Code window*
        // passes every filter here, and its own `Stop` hook is on the same
        // cursor. What bounds that is C4b's re-load under the lock, not this
        // line; this line only removes the one instance we can identify for
        // free. See §Known limits.
        .filter(|s| s.session_id != current_session_id)
        // A poisoned session retries once per `poison_retry_secs`, exactly as
        // `retain` does. Selecting purely on `offset < file_size` — the
        // earlier draft — would re-attempt a session the daemon has durably
        // rejected on **every** launch, which is how a slow-retry state
        // becomes a hot loop.
        .filter(|s| !super::poisoned_within_throttle(s, cfg.poison_retry_secs, now_ms))
        .filter_map(|state| {
            // A transcript that is gone is not recoverable by retrying and is
            // not the daemon's fault (§Failure posture). An empty
            // `transcript_path` — a state file written before that field
            // existed — lands here too, via a failed `metadata`.
            let meta = std::fs::metadata(&state.transcript_path).ok()?;
            let file_size = meta.len();
            if file_size <= state.offset {
                return None;
            }
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Some(Candidate {
                state,
                file_size,
                modified_ms,
            })
        })
        .collect();
    found.sort_by_key(|c| std::cmp::Reverse(c.modified_ms));
    found.truncate(cfg.catchup_max_sessions);
    found
}

fn days_ms(days: u64) -> i64 {
    i64::try_from(days.saturating_mul(24 * 60 * 60 * 1000)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HooksConfig {
        memgarden_core::config::Config::defaults().unwrap().hooks
    }

    /// A session with `bytes` of transcript already written and a cursor at
    /// `offset`.
    fn plant(dir: &Path, session_id: &str, offset: u64, bytes: usize) -> SessionState {
        let transcript = dir.join(format!("{session_id}.jsonl"));
        std::fs::write(&transcript, "x".repeat(bytes)).unwrap();
        let mut s = SessionState::new(session_id, "claude-code::demo");
        s.transcript_path = transcript.to_string_lossy().into_owned();
        s.offset = offset;
        state::store(dir, &s).unwrap();
        s
    }

    fn ids(picked: &[Candidate]) -> Vec<&str> {
        picked.iter().map(|c| c.state.session_id.as_str()).collect()
    }

    #[test]
    fn only_sessions_whose_transcript_has_grown_are_selected() {
        let dir = tempfile::tempdir().unwrap();
        plant(dir.path(), "behind", 100, 500);
        plant(dir.path(), "caught-up", 500, 500);
        // A cursor past EOF (a transcript that was replaced by a shorter file)
        // is not a catch-up candidate either.
        plant(dir.path(), "ahead", 900, 500);

        let mut s = SessionState::new("no-transcript", "b1");
        s.transcript_path = dir.path().join("gone.jsonl").to_string_lossy().into_owned();
        state::store(dir.path(), &s).unwrap();

        let picked = select(dir.path(), "current", &cfg(), now_ms());
        assert_eq!(ids(&picked), vec!["behind"]);
    }

    /// The one race the advisory lock cannot arbitrate: a `resume`
    /// `SessionStart` whose own retain hook is live on the same cursor.
    #[test]
    fn the_current_session_is_excluded_even_when_it_is_the_stalest() {
        let dir = tempfile::tempdir().unwrap();
        plant(dir.path(), "resumed", 0, 10_000);
        plant(dir.path(), "other", 9_000, 10_000);

        assert_eq!(
            ids(&select(dir.path(), "resumed", &cfg(), now_ms())),
            vec!["other"]
        );
        // And with no exclusion it would have been picked first, which is what
        // makes the filter above load-bearing rather than incidental.
        let all = select(dir.path(), "", &cfg(), now_ms());
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn a_poisoned_session_is_skipped_inside_the_throttle_and_retried_outside_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = plant(dir.path(), "poisoned", 0, 5_000);
        let poisoned_at = now_ms();
        s.poisoned_at = Some(poisoned_at);
        state::store(dir.path(), &s).unwrap();

        let cfg = cfg();
        let window_ms = cfg.poison_retry_secs as i64 * 1000;
        // Inside the window: skipped. Checked at both ends of the window, so a
        // mutation that flips the comparison fails rather than passing on one
        // of them.
        assert!(select(dir.path(), "cur", &cfg, poisoned_at).is_empty());
        assert!(select(dir.path(), "cur", &cfg, poisoned_at + window_ms - 1).is_empty());
        // Outside it: retried. Poisoning is a slow-retry state, not a latch.
        assert_eq!(
            ids(&select(dir.path(), "cur", &cfg, poisoned_at + window_ms)),
            vec!["poisoned"]
        );
    }

    /// A stamp in the future is not a throttle. No attacker required: an NTP
    /// step, a VM resume or a dual-boot RTC produces one, and the bare
    /// `now < poisoned_at + window` test read it as "throttled" **forever** —
    /// silently removing a session from catch-up, decided by a process whose
    /// output goes to `/dev/null`.
    #[test]
    fn a_poisoned_at_in_the_future_does_not_throttle_a_session_out_of_existence() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = plant(dir.path(), "clock-skew", 0, 5_000);
        let now = now_ms();
        for future in [now + 1, now + days_ms(3650), i64::MAX] {
            s.poisoned_at = Some(future);
            state::store(dir.path(), &s).unwrap();
            assert_eq!(
                ids(&select(dir.path(), "cur", &cfg(), now)),
                vec!["clock-skew"],
                "poisoned_at = {future} (now = {now}) must not throttle"
            );
        }
    }

    /// The cap truncates, so the order decides *which* sessions are dropped —
    /// which makes the ordering a correctness property, not a presentation one.
    #[test]
    fn the_most_recently_active_transcripts_win_the_capped_slots() {
        let dir = tempfile::tempdir().unwrap();
        let base =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for (i, id) in ["oldest", "middle", "newest"].iter().enumerate() {
            plant(dir.path(), id, 0, 1_000);
            let transcript = dir.path().join(format!("{id}.jsonl"));
            std::fs::File::options()
                .write(true)
                .open(&transcript)
                .unwrap()
                .set_modified(base + std::time::Duration::from_secs(i as u64 * 3600))
                .unwrap();
        }

        let mut cfg = cfg();
        cfg.catchup_max_sessions = 2;
        assert_eq!(
            ids(&select(dir.path(), "cur", &cfg, now_ms())),
            vec!["newest", "middle"]
        );

        // `0` disables selection outright (the config's stated meaning) —
        // and, per `run`, leaves the housekeeping alone.
        cfg.catchup_max_sessions = 0;
        assert!(select(dir.path(), "cur", &cfg, now_ms()).is_empty());
    }

    #[test]
    fn the_retention_window_is_expressed_in_the_unit_gc_takes() {
        assert_eq!(days_ms(90), 90 * 86_400_000);
        assert_eq!(days_ms(0), 0);
        assert_eq!(days_ms(u64::MAX), i64::MAX);
    }
}
