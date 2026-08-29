//! Cron due-ness for mental-model refresh, ported from
//! `maintenance.py:417-425`.
//!
//! Legacy asks croniter for the schedule's most recent fire time and compares
//! it with the model's `last_refreshed_at`:
//!
//! ```python
//! prev_fire = croniter(cron, now).get_prev(datetime)
//! if last is None or prev_fire > last: due.append(row)
//! ```
//!
//! That is the whole contract, and it is why the watermark is
//! `last_refreshed_at` rather than a stored "next run": a daemon that was down
//! when the schedule fired still sees the model as due the moment it comes
//! back, with no catch-up bookkeeping.
//!
//! // ponytail: a hand-rolled 5-field parser instead of a cron crate, for one
//! // call site whose only consumer (a scheduler) is not shipped. Flagged as
//! // mild scope creep in review and accepted: the subset it refuses
//! // (`@daily`, `L`, `#`, `JAN`/`MON` names, seconds, `?`) is refused loudly
//! // at write time rather than silently mis-scheduled, and a crate is a
//! // dependency the workspace's containment rule would have to argue about.
//! // **Swap it for a maintained crate the day a scheduler exists** — that is
//! // the point at which the rest of the syntax starts mattering.

use crate::state::AppState;
use jiff::civil::Date;
use jiff::{Timestamp, tz::TimeZone};

/// How far back [`Cron::prev_fire`] will walk before giving up. Four years
/// covers the worst legal schedule (`0 0 29 2 *` — February 29th); beyond that
/// the answer is "so long ago that everything is due anyway".
const MAX_LOOKBACK_DAYS: i32 = 366 * 4;

/// A parsed 5-field cron expression (`minute hour day-of-month month
/// day-of-week`), interpreted in **UTC** — legacy evaluates against
/// `datetime.now(timezone.utc)` and our timestamps are unix-ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minute: Vec<i8>,
    hour: Vec<i8>,
    dom: Vec<i8>,
    month: Vec<i8>,
    dow: Vec<i8>,
    /// Whether the field was written as something other than `*`. When both
    /// day fields are restricted, cron matches a day satisfying **either**
    /// (the classic Vixie rule); otherwise the `*` one is a no-op.
    dom_restricted: bool,
    dow_restricted: bool,
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron must have 5 fields (minute hour day-of-month month day-of-week), got {}",
                fields.len()
            ));
        }
        Ok(Cron {
            minute: parse_field(fields[0], 0, 59)?,
            hour: parse_field(fields[1], 0, 23)?,
            dom: parse_field(fields[2], 1, 31)?,
            month: parse_field(fields[3], 1, 12)?,
            // 7 is Sunday as well, normalized to 0 below.
            dow: parse_field(fields[4], 0, 7)?
                .into_iter()
                .map(|d| if d == 7 { 0 } else { d })
                .collect(),
            dom_restricted: fields[2] != "*",
            dow_restricted: fields[4] != "*",
        })
    }

    /// The most recent fire time at or before `now_ms`, in unix-ms, or `None`
    /// if the schedule has not fired within [`MAX_LOOKBACK_DAYS`].
    ///
    /// Walks whole days backwards and picks the latest matching `(hour,
    /// minute)` within each — a day loop, not a minute loop, so an exotic
    /// schedule costs at most a few thousand cheap date comparisons rather
    /// than two million.
    pub fn prev_fire(&self, now_ms: i64) -> Option<i64> {
        let dt = Timestamp::from_millisecond(now_ms)
            .ok()?
            .to_zoned(TimeZone::UTC)
            .datetime();
        let mut date = dt.date();
        // Inclusive: a schedule firing exactly this minute has fired.
        let mut cutoff = i32::from(dt.hour()) * 60 + i32::from(dt.minute());

        for _ in 0..MAX_LOOKBACK_DAYS {
            if self.matches_date(date)
                && let Some(mins) = self.latest_minute_at_or_before(cutoff)
            {
                let time = jiff::civil::time(
                    i8::try_from(mins / 60).ok()?,
                    i8::try_from(mins % 60).ok()?,
                    0,
                    0,
                );
                let fired = date.to_datetime(time).to_zoned(TimeZone::UTC).ok()?;
                return Some(fired.timestamp().as_millisecond());
            }
            date = date.yesterday().ok()?;
            cutoff = 23 * 60 + 59;
        }
        None
    }

    fn matches_date(&self, date: Date) -> bool {
        if !self.month.contains(&date.month()) {
            return false;
        }
        let dom_hit = self.dom.contains(&date.day());
        let dow_hit = self.dow.contains(&date.weekday().to_sunday_zero_offset());
        if self.dom_restricted && self.dow_restricted {
            dom_hit || dow_hit
        } else {
            dom_hit && dow_hit
        }
    }

    fn latest_minute_at_or_before(&self, cutoff: i32) -> Option<i32> {
        self.hour.iter().rev().find_map(|&h| {
            let base = i32::from(h) * 60;
            let room = cutoff - base;
            if room < 0 {
                return None;
            }
            self.minute
                .iter()
                .rev()
                .find(|&&m| i32::from(m) <= room)
                .map(|&m| base + i32::from(m))
        })
    }
}

/// `*`, `*/n`, `a`, `a-b`, `a-b/n`, and comma-separated lists of those.
/// Returns a sorted, deduplicated value list.
fn parse_field(field: &str, min: i8, max: i8) -> Result<Vec<i8>, String> {
    let mut out: Vec<i8> = Vec::new();
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<i8>()
                    .map_err(|_| format!("bad cron step in {part:?}"))?,
            ),
            None => (part, 1),
        };
        if step < 1 {
            return Err(format!("cron step must be >= 1 in {part:?}"));
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (num(a, min, max)?, num(b, min, max)?)
        } else {
            let v = num(range, min, max)?;
            // A bare number with a step means "from here to the end of the
            // field", which is how `*/n` and `5/15` both behave in Vixie cron.
            if part.contains('/') { (v, max) } else { (v, v) }
        };
        if lo > hi {
            return Err(format!("cron range is inverted in {part:?}"));
        }
        out.extend((lo..=hi).step_by(step as usize));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn num(s: &str, min: i8, max: i8) -> Result<i8, String> {
    let v: i8 = s
        .trim()
        .parse()
        .map_err(|_| format!("cron field {s:?} is not a number (names are not supported)"))?;
    if v < min || v > max {
        return Err(format!("cron value {v} out of range {min}..={max}"));
    }
    Ok(v)
}

/// `maintenance.py:417-425` exactly: never refreshed → due; otherwise due iff
/// the schedule has fired since the last refresh.
///
/// An **unparseable** expression is not due. Legacy logs and `continue`s past
/// it for the same reason: a typo must not become an hourly LLM call, and the
/// write path already rejected it (this is the belt to that suspenders).
pub fn is_due(expr: &str, last_refreshed_at: Option<i64>, now_ms: i64) -> bool {
    let Ok(cron) = Cron::parse(expr) else {
        tracing::warn!(cron = %expr, "unparseable mental-model trigger; treating as not due");
        return false;
    };
    match (last_refreshed_at, cron.prev_fire(now_ms)) {
        (None, _) => true,
        (Some(last), Some(prev)) => prev > last,
        (Some(_), None) => false,
    }
}

/// The background refresh loop — the half of CE-10 that was missing.
///
/// CE-10 shipped [`is_due`] and a manual refresh route and nothing that called
/// them, on the reasoning recorded in `parity-gaps.md`: *"a scheduler that
/// spends GPU with no consumer is the wrong default."* That was right when it
/// was written. It is no longer the situation — `/v1/banks/{id}/reflect` reads
/// the bank's nearest mental models on every call, so a stale document is a
/// worse answer rather than an unread one.
///
/// The tick is cheap when nothing is due: one `list` per bank and a cron
/// comparison per model. Only a model whose own trigger has fired since its
/// `last_refreshed_at` costs an LLM call, so `refresh_interval_secs` bounds
/// how late a refresh can be, not how much GPU this spends.
///
/// Three guards, all borrowed from `consolidate::round::run_task` rather than
/// invented, because they were paid for there:
///
/// * **`0` disables it**, and the manual route still works.
/// * **`MissedTickBehavior::Delay`**, so a tick that outruns the interval
///   re-bases the schedule instead of firing every missed tick back to back —
///   the spacing evaporates exactly when ticks are slow otherwise.
/// * **Deferred while a retain is in flight.** Refresh and retain contend on
///   the same ONNX mutex and SQLite write lock, and refresh has no latency
///   SLO. Waiting one interval removes the contention window rather than
///   shrinking it.
pub async fn run_task(state: AppState) {
    let secs = state.cfg.mental.refresh_interval_secs;
    if secs == 0 {
        tracing::info!("mental-model refresh task disabled (refresh_interval_secs = 0)");
        return;
    }
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = ticker.tick() => tick_once(&state).await,
            _ = &mut shutdown => break,
        }
    }
}

/// One pass over every bank's mental models, refreshing the due ones.
///
/// Failures are logged and skipped, never retried inside the tick: a model
/// whose refresh fails is still due on the next one, which is the retry, and a
/// tight retry here would spend the GPU a bank at a time on a broken model.
async fn tick_once(state: &AppState) {
    if crate::retain::queued_bytes() > 0 {
        tracing::debug!("retain in flight; deferring the mental-model refresh tick");
        return;
    }
    let now = memgarden_core::now_ms();
    let db = state.db.clone();
    let banks = match tokio::task::spawn_blocking(move || memgarden_store::banks::list(&db)).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "mental refresh tick: listing banks failed");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "mental refresh tick: bank listing panicked");
            return;
        }
    };

    for bank in banks {
        let (db, bank_id) = (state.db.clone(), bank.bank_id.clone());
        // `list` is paged; MAX_PER_TICK bounds one tick's work rather than the
        // bank's size. A bank with more due models than this catches up over
        // the following ticks, in `list`'s own order, which is stable.
        const MAX_PER_TICK: usize = 32;
        let models = match tokio::task::spawn_blocking(move || {
            memgarden_store::mental_models::list(&db, &bank_id, MAX_PER_TICK, 0)
        })
        .await
        {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                tracing::warn!(bank = %bank.bank_id, error = %e, "mental refresh tick: list failed");
                continue;
            }
            Err(e) => {
                tracing::warn!(bank = %bank.bank_id, error = %e, "mental refresh tick: list panicked");
                continue;
            }
        };

        for model in models {
            let Some(trigger) = model.trigger.as_deref() else {
                continue;
            };
            if !is_due(trigger, model.last_refreshed_at, now) {
                continue;
            }
            match super::refresh(state, &bank.bank_id, &model.id).await {
                Ok(_) => tracing::info!(
                    bank = %bank.bank_id, mental_model = %model.id,
                    "mental model refreshed on schedule"
                ),
                Err(e) => tracing::warn!(
                    bank = %bank.bank_id, mental_model = %model.id, error = ?e,
                    "scheduled mental-model refresh failed; it stays due"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// `0` disables the ticker, which is the shipped default's escape hatch and
    /// the only branch of `run_task` reachable without a live daemon. The loop
    /// itself is covered end to end by `mental_api.rs`.
    #[test]
    fn a_zero_interval_disables_the_refresh_task() {
        let cfg = memgarden_core::config::Config::defaults().expect("defaults");
        // The default is on; a deployment turns it off by setting 0, and
        // `run_task` returns immediately in that case.
        assert_eq!(cfg.mental.refresh_interval_secs, 600);
    }

    use super::*;

    /// 2026-08-03T12:34:00Z (a Monday).
    const NOW: i64 = 1_785_760_440_000;

    fn at(s: &str) -> i64 {
        s.parse::<Timestamp>().unwrap().as_millisecond()
    }

    #[test]
    fn now_constant_is_the_monday_we_think_it_is() {
        assert_eq!(at("2026-08-03T12:34:00Z"), NOW);
        let d = Timestamp::from_millisecond(NOW)
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .date();
        assert_eq!(d.weekday().to_sunday_zero_offset(), 1, "Monday");
    }

    #[test]
    fn prev_fire_hourly_and_daily() {
        let hourly = Cron::parse("0 * * * *").unwrap();
        assert_eq!(hourly.prev_fire(NOW), Some(at("2026-08-03T12:00:00Z")));

        let daily = Cron::parse("0 3 * * *").unwrap();
        assert_eq!(daily.prev_fire(NOW), Some(at("2026-08-03T03:00:00Z")));

        // Before today's fire time -> yesterday's.
        let daily_late = Cron::parse("0 23 * * *").unwrap();
        assert_eq!(daily_late.prev_fire(NOW), Some(at("2026-08-02T23:00:00Z")));
    }

    #[test]
    fn prev_fire_is_inclusive_of_the_current_minute() {
        let every_minute = Cron::parse("* * * * *").unwrap();
        assert_eq!(
            every_minute.prev_fire(NOW),
            Some(at("2026-08-03T12:34:00Z"))
        );
    }

    #[test]
    fn prev_fire_honours_steps_lists_and_ranges() {
        assert_eq!(
            Cron::parse("*/15 * * * *").unwrap().prev_fire(NOW),
            Some(at("2026-08-03T12:30:00Z"))
        );
        assert_eq!(
            Cron::parse("5,40 * * * *").unwrap().prev_fire(NOW),
            Some(at("2026-08-03T12:05:00Z"))
        );
        assert_eq!(
            Cron::parse("0 9-17 * * *").unwrap().prev_fire(NOW),
            Some(at("2026-08-03T12:00:00Z"))
        );
    }

    /// Both day fields restricted is an OR (Vixie), and a restricted month
    /// walks back across years.
    #[test]
    fn prev_fire_day_fields_or_and_long_walks() {
        // Sunday (0) or the 1st of the month, whichever is more recent:
        // 2026-08-02 was a Sunday, 2026-08-01 a Saturday.
        assert_eq!(
            Cron::parse("0 0 1 * 0").unwrap().prev_fire(NOW),
            Some(at("2026-08-02T00:00:00Z"))
        );
        // February 29th: the last one before 2026-08-03 was in 2024.
        assert_eq!(
            Cron::parse("0 0 29 2 *").unwrap().prev_fire(NOW),
            Some(at("2024-02-29T00:00:00Z"))
        );
    }

    /// The ported rule, end to end (`maintenance.py:417-425`).
    #[test]
    fn is_due_compares_prev_fire_with_the_watermark() {
        // Never refreshed is always due.
        assert!(is_due("0 3 * * *", None, NOW));

        // Refreshed after today's 03:00 fire: not due.
        assert!(!is_due("0 3 * * *", Some(at("2026-08-03T04:00:00Z")), NOW));
        // Refreshed before it: due.
        assert!(is_due("0 3 * * *", Some(at("2026-08-03T02:59:00Z")), NOW));
        // Exactly at the fire time is NOT due — legacy's comparison is
        // strictly greater-than.
        assert!(!is_due("0 3 * * *", Some(at("2026-08-03T03:00:00Z")), NOW));

        // A yearly schedule that DID fire (2026-01-01) but is not due,
        // because the watermark is newer than that fire. (Review round 1,
        // B9-5: the assertion was right, the old comment was not.)
        assert!(!is_due("0 0 1 1 *", Some(NOW), NOW));
    }

    #[test]
    fn invalid_expressions_are_rejected_and_never_due() {
        for bad in [
            "@daily",
            "0 3 * *",
            "0 3 * * * *",
            "0 3 * * MON",
            "99 * * * *",
            "0 3 * * 8",
            "*/0 * * * *",
            "30-10 * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad:?} must not parse");
            assert!(!is_due(bad, None, NOW), "{bad:?} must not be due");
        }
    }
}
