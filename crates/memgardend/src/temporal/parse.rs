//! ISO-8601 parsing and the retain-side relative-expression fallback.
//!
//! Legacy references: `retain/fact_extraction.py:75-111` (`_infer_temporal_date`)
//! and `retain/orchestrator.py:228-258` (`parse_datetime_flexible`).

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;

/// Best-effort ISO-8601 -> unix ms. Accepts a full timestamp
/// (`2024-06-10T00:00:00Z`), a naive datetime (assumed **UTC**), or a bare
/// date.
///
/// legacy: `parse_datetime_flexible`, `orchestrator.py:228-258` — it rewrites
/// a trailing `Z` to `+00:00` before `fromisoformat` and stamps UTC onto a
/// naive value. `jiff::Timestamp` already accepts `Z` directly, so the port is
/// the three fall-throughs, not the string surgery.
pub fn parse_iso_ms(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(ts) = s.parse::<Timestamp>() {
        return Some(ts.as_millisecond());
    }
    if let Ok(dt) = s.parse::<jiff::civil::DateTime>() {
        return dt
            .to_zoned(TimeZone::UTC)
            .ok()
            .map(|z| z.timestamp().as_millisecond());
    }
    if let Ok(date) = s.parse::<Date>() {
        return midnight_ms(date);
    }
    None
}

/// Midnight UTC of `date`, in unix ms. The single place a `Date` becomes a
/// timestamp, so every "truncated to midnight" claim in this module means the
/// same thing.
pub fn midnight_ms(date: Date) -> Option<i64> {
    date.to_zoned(TimeZone::UTC)
        .ok()
        .map(|z| z.timestamp().as_millisecond())
}

/// The UTC calendar date `unix_ms` falls on.
pub fn date_of(unix_ms: i64) -> Option<Date> {
    Timestamp::from_millisecond(unix_ms)
        .ok()
        .map(|ts| ts.to_zoned(TimeZone::UTC).date())
}

/// The 14 relative expressions and their day offsets, in **iteration order**
/// — legacy's `temporal_patterns` dict is walked in insertion order and the
/// first match wins (`fact_extraction.py:86-111`), so the order is behaviour,
/// not formatting.
///
/// Each entry carries the literal alternatives of one legacy regex. Only
/// `tonight` has two: legacy's pattern is `\btonigh?t\b`, i.e. it also
/// matches the misspelling `tonigt`. Ported as written.
pub const RELATIVE_DAY_OFFSETS: [(&[&str], i64); 14] = [
    (&["last night"], -1),
    (&["yesterday"], -1),
    (&["today"], 0),
    (&["this morning"], 0),
    (&["this afternoon"], 0),
    (&["this evening"], 0),
    (&["tonight", "tonigt"], 0),
    (&["tomorrow"], 1),
    (&["last week"], -7),
    (&["this week"], 0),
    (&["next week"], 7),
    (&["last month"], -30),
    (&["this month"], 0),
    (&["next month"], 30),
];

/// `\b<needle>\b` against an **already-lowercased** haystack, without pulling
/// in a regex crate. Python's `\b` on `str` patterns is Unicode-aware
/// (`\w` = letters, digits, underscore in any script), so the boundary test is
/// `is_alphanumeric() || '_'` rather than an ASCII class — that is what makes
/// `yesterday` fail to match inside `yesterday어제`, exactly as legacy does.
///
/// Non-ASCII needles are matched by plain containment: Korean (like legacy's
/// Chinese module) has no whitespace word boundary, and requiring one would
/// reject `지난주에`, which is how the expression is actually written.
pub fn contains_expression(lower_hay: &str, needle: &str) -> Option<(usize, usize)> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    lower_hay
        .match_indices(needle)
        .find(|(i, m)| {
            if !needle.is_ascii() {
                return true;
            }
            let before = lower_hay[..*i]
                .chars()
                .next_back()
                .is_none_or(|c| !is_word(c));
            let after = lower_hay[*i + m.len()..]
                .chars()
                .next()
                .is_none_or(|c| !is_word(c));
            before && after
        })
        .map(|(i, m)| (i, i + m.len()))
}

/// legacy: `_infer_temporal_date`, `fact_extraction.py:75-111`. The fallback
/// used when the LLM extracted an `event` fact but gave it no
/// `occurred_start`: resolve a relative expression in the fact text against
/// the retain job's `event_date`, truncated to midnight.
///
/// `None` in, `None` out — legacy's first line (`if event_date is None`).
pub fn infer_temporal_date(fact_text: &str, event_date_ms: Option<i64>) -> Option<Timestamp> {
    let event_date_ms = event_date_ms?;
    let lower = fact_text.to_lowercase();
    let (_, offset_days) = RELATIVE_DAY_OFFSETS.iter().find(|(phrases, _)| {
        phrases
            .iter()
            .any(|p| contains_expression(&lower, p).is_some())
    })?;

    // `event_date + timedelta(days=n)` then `.replace(hour=0, ...)`. Adding
    // whole days to a UTC instant and truncating commutes with truncating
    // first, so this resolves on the calendar date and is DST-free by
    // construction (everything here is UTC).
    let target = date_of(event_date_ms)?
        .checked_add(jiff::Span::new().days(*offset_days))
        .ok()?;
    Timestamp::from_millisecond(midnight_ms(target)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-06-10T14:23:45Z — deliberately mid-afternoon so the midnight
    /// truncation is observable.
    const EVENT: i64 = 1_718_029_425_000;

    fn infer(text: &str) -> Option<String> {
        infer_temporal_date(text, Some(EVENT)).map(|ts| ts.to_string())
    }

    #[test]
    fn all_fourteen_relative_expressions_resolve_to_their_offset() {
        // (expression, expected date) — offsets are days off 2024-06-10.
        let cases: [(&str, &str); 15] = [
            ("we shipped it last night", "2024-06-09"),
            ("yesterday the daemon crashed", "2024-06-09"),
            ("today we merged", "2024-06-10"),
            ("this morning the build broke", "2024-06-10"),
            ("this afternoon we rolled back", "2024-06-10"),
            ("this evening the alert cleared", "2024-06-10"),
            ("tonight we deploy", "2024-06-10"),
            // legacy's `\btonigh?t\b` also accepts the misspelling.
            ("tonigt we deploy", "2024-06-10"),
            ("tomorrow we cut the release", "2024-06-11"),
            ("last week we chose sqlite", "2024-06-03"),
            ("this week we chose sqlite", "2024-06-10"),
            ("next week we revisit it", "2024-06-17"),
            ("last month the p95 regressed", "2024-05-11"),
            ("this month the p95 regressed", "2024-06-10"),
            ("next month we ship phase c", "2024-07-10"),
        ];
        for (text, want_date) in cases {
            assert_eq!(
                infer(text).as_deref(),
                Some(format!("{want_date}T00:00:00Z").as_str()),
                "{text}"
            );
        }
        assert_eq!(RELATIVE_DAY_OFFSETS.len(), 14, "14 patterns, 15 literals");
    }

    #[test]
    fn truncates_to_midnight_not_to_the_event_time() {
        // The event is 14:23:45Z; the inferred date must carry no time.
        assert_eq!(infer("today"), Some("2024-06-10T00:00:00Z".to_string()));
    }

    #[test]
    fn first_matching_pattern_wins() {
        // "last night" precedes "tonight" in the table, and both are present.
        assert_eq!(infer("tonight, like last night"), infer("last night"));
        assert_eq!(infer("tonight, like last night"), infer("yesterday"));
    }

    #[test]
    fn no_event_date_and_no_expression_both_yield_none() {
        assert!(infer_temporal_date("yesterday", None).is_none());
        assert!(infer("the daemon binds 127.0.0.1:9100").is_none());
    }

    #[test]
    fn word_boundaries_are_respected() {
        // `\btoday\b` must not fire inside another word.
        assert!(infer("we ran the todaylist migration").is_none());
        assert!(infer("nottoday").is_none());
        // Punctuation is a boundary; a trailing Korean particle is not
        // (Python's `\w` is Unicode-aware and so is this).
        assert!(infer("(today)").is_some());
        assert!(infer("today어제").is_none());
    }

    #[test]
    fn iso_parsing_covers_zulu_naive_and_bare_date() {
        assert_eq!(
            parse_iso_ms("2024-06-10T00:00:00Z"),
            Some(1_717_977_600_000)
        );
        // Naive is assumed UTC (orchestrator.py:250-252), so it equals the Z form.
        assert_eq!(parse_iso_ms("2024-06-10T00:00:00"), Some(1_717_977_600_000));
        assert_eq!(parse_iso_ms("2024-06-10"), Some(1_717_977_600_000));
        // An explicit offset is honoured, not assumed away.
        assert_eq!(
            parse_iso_ms("2024-06-10T00:00:00+09:00"),
            Some(1_717_977_600_000 - 9 * 3_600_000)
        );
        assert_eq!(parse_iso_ms("  "), None);
        assert_eq!(parse_iso_ms("last tuesday"), None);
    }
}
