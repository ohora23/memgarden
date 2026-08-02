//! Query-side temporal-constraint extraction (CE-8).
//!
//! Legacy references: `temporal_periods.py` (the period rules and the
//! `NO_TEMPORAL_CONSTRAINT` sentinel, `:17-21`) and
//! `query_analyzer.py:228-258` (the call order: lowercase, periods, the
//! sentinel short-circuit, then the fallback parser on the **original**).
//!
//! **Deferred to Phase C+**: `chinese_temporal_periods.py`, ~150 ordered
//! rules whose *ordering* is load-bearing (~1,800 lines of regex). These
//! banks hold no Chinese; porting it buys nothing measurable and gets the
//! ordering subtly wrong. Flagged here so it is a decision, not an omission.

use jiff::Span;
use jiff::civil::Date;

use super::parse::{contains_expression, date_of, midnight_ms, parse_iso_ms};

/// One day, minus a millisecond: legacy closes a period at
/// `23:59:59.999999` (`temporal_periods.py:_constraint`). Our grain is ms.
const DAY_MS: i64 = 86_400_000;

/// What a query says about time. Three states, not two — the third is the
/// whole point (`temporal_periods.py:17-21`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    /// An inclusive `[start_ms, end_ms]` window.
    Range { start_ms: i64, end_ms: i64 },
    /// legacy `NO_TEMPORAL_CONSTRAINT`: a temporal expression **was**
    /// recognized and is deliberately unconstrainable — a recurring event
    /// ("every Monday") or an open-ended "before X". It must short-circuit
    /// before the fallback parser (`query_analyzer.py:230-231`), otherwise
    /// "every Monday" silently becomes whatever single date the fallback
    /// happens to find.
    Unconstrainable,
}

/// The period rules, resolved against the reference date.
#[derive(Debug, Clone, Copy)]
enum Period {
    /// A single day at `now + n` days.
    Day(i64),
    /// An inclusive day range `[now + a, now + b]`.
    Days(i64, i64),
    /// The Monday..Sunday week `n` weeks from the current one.
    Week(i64),
    /// The Saturday..Sunday weekend `n` weekends back (1 = last weekend).
    Weekend(i64),
    /// The calendar month `n` months from the current one.
    Month(i64),
    /// The calendar year `n` years from the current one.
    Year(i64),
}

/// The recognized period set, **in match order**. English phrases match on
/// word boundaries; Korean ones by containment (see `contains_expression`).
///
/// Ordering is behaviour, in both directions, because Korean containment has
/// no word boundary to stop it:
/// * **suffix** — `지난주말` *contains* `지난주`, so every weekend rule runs
///   before its week rule. English is safe either way (`\blast week\b` does
///   not match inside "last weekend"), which is why legacy can afford the
///   opposite order (`temporal_periods.py:118-137`).
/// * **prefix** — `지지난주` *contains* `지난주` and `재작년` contains
///   `작년`, so a `지지난`/`재작` rule that came *after* its container would
///   never fire and the query would silently resolve one period too late.
///   This is the direction legacy guards with `(?<![上下大小])` on every
///   Chinese period rule (`chinese_temporal_periods.py:551,726,759,825`);
///   with literal matching the equivalent is to put the longer form first.
/// * `그저께` before `어제`, `며칠 전` before the bare day rules — same
///   containment reason.
///
/// Offsets for the vague English forms are legacy's
/// (`_extract_non_chinese_period`, `temporal_periods.py:54-101`); the Korean
/// column and the `this`/`next` rows are MemGarden's addition — legacy's
/// non-Chinese set is past-only, but its *Chinese* set has 这周/下周, so the
/// concept is legacy-supported, just not in the Latin-script module.
const RULES: &[(&[&str], Period)] = &[
    (&["지지난 주말", "지지난주말"], Period::Weekend(2)),
    (
        &["last weekend", "지난 주말", "지난주말", "저번 주말"],
        Period::Weekend(1),
    ),
    (&["day before yesterday", "그저께", "그제"], Period::Day(-2)),
    (&["yesterday", "어제"], Period::Day(-1)),
    (&["today", "오늘"], Period::Day(0)),
    (&["tomorrow", "내일"], Period::Day(1)),
    (
        &["couple of days ago", "couple days ago"],
        Period::Days(-3, -1),
    ),
    (
        &["few days ago", "며칠 전", "몇 일 전"],
        Period::Days(-5, -2),
    ),
    (&["지지난 주", "지지난주"], Period::Week(-2)),
    (
        &["last week", "지난주", "지난 주", "저번주", "저번 주"],
        Period::Week(-1),
    ),
    (&["this week", "이번 주", "이번주", "금주"], Period::Week(0)),
    (&["next week", "다음 주", "다음주", "담주"], Period::Week(1)),
    (
        &["couple of weeks ago", "couple weeks ago"],
        Period::Days(-21, -7),
    ),
    (&["few weeks ago", "몇 주 전"], Period::Days(-35, -14)),
    (&["지지난 달", "지지난달"], Period::Month(-2)),
    (
        &["last month", "지난달", "지난 달", "저번달", "저번 달"],
        Period::Month(-1),
    ),
    (
        &["this month", "이번 달", "이번달", "금월"],
        Period::Month(0),
    ),
    (&["next month", "다음 달", "다음달"], Period::Month(1)),
    (
        &["couple of months ago", "couple months ago"],
        Period::Days(-90, -30),
    ),
    (
        &["few months ago", "몇 달 전", "몇 개월 전"],
        Period::Days(-150, -60),
    ),
    // `재작` alone is deliberately NOT a literal here: it would also match
    // `재작업` ("rework"), which is not a date.
    (&["재작년", "지지난해", "지지난 해"], Period::Year(-2)),
    (
        &["last year", "작년", "지난해", "지난 해"],
        Period::Year(-1),
    ),
    (&["this year", "올해", "금년"], Period::Year(0)),
];

/// Recurrence markers that stand alone (`매주` = "every week").
/// legacy: the `每/各/隔` family, `chinese_temporal_periods.py:540-548`.
///
/// Korean only, on purpose. The English adjectives (`weekly`, `monthly`, …)
/// were here and had to go: they are *nouns' adjectives*, not recurrence
/// markers, so "the weekly report from last week" and "monthly metrics we
/// reviewed yesterday" both went `Unconstrainable` and threw away a window
/// the query also states. Legacy has no counterpart — its sentinel needs
/// 每/各/隔 *bound to a unit* — and `every|each + unit` below already covers
/// genuine English recurrence. The Korean markers are unambiguous: `매주`
/// cannot be an adjective for anything.
const RECURRING: &[&str] = &[
    "매일",
    "매주",
    "매달",
    "매월",
    "매년",
    "격일",
    "격주",
    "날마다",
    "주마다",
    "달마다",
    "해마다",
];

/// `every`/`each` only recur when a time unit follows — "every detail" is not
/// a temporal expression.
const RECUR_QUANTIFIERS: &[&str] = &["every", "each"];
const RECUR_UNITS: &[&str] = &[
    "day",
    "days",
    "week",
    "weeks",
    "weekend",
    "month",
    "months",
    "year",
    "years",
    "morning",
    "evening",
    "night",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// Open-ended *into the past*: no lower bound exists, so there is no range to
/// build. legacy returns the sentinel for exactly this shape
/// (`N天以前`, `chinese_temporal_periods.py:905-910`).
const BEFORE_MARKERS_EN: &[&str] = &["before", "until", "up to"];
const BEFORE_MARKERS_KO: &[&str] = &["이전", "까지"];

/// Open-ended *from a point up to the end of today* — that IS a range
/// (legacy `since_constraint`, `chinese_temporal_periods.py:451-454`).
///
/// `from` is deliberately absent: "notes from yesterday" means yesterday, not
/// yesterday-onwards, and legacy has no `from` marker at all.
const SINCE_MARKERS_EN: &[&str] = &["since", "after"];
const SINCE_MARKERS_KO: &[&str] = &["부터", "이후"];

/// Extracts what `query` says about time, resolved against `now_ms` (UTC).
///
/// The call order is legacy's (`query_analyzer.py:226-258`):
/// 1. lowercase the query — but hand the **original** to the fallback parser
///    (`:228-229` vs `:253`), because a parser reads case-sensitive input;
/// 2. recurrence first, so it short-circuits everything below it;
/// 3. the period rules, adjusted by an adjacent before/since marker;
/// 4. only then the fallback parser.
pub fn extract_constraint(query: &str, now_ms: i64) -> Option<Constraint> {
    let lower = query.to_lowercase();
    let today = date_of(now_ms)?;

    if is_recurring(&lower) {
        return Some(Constraint::Unconstrainable);
    }

    if let Some((span, period)) = find_period(&lower) {
        if marker_before(&lower, span, BEFORE_MARKERS_EN, BEFORE_MARKERS_KO) {
            return Some(Constraint::Unconstrainable);
        }
        let (start, end) = resolve(period, today)?;
        let start_ms = midnight_ms(start)?;
        if marker_before(&lower, span, SINCE_MARKERS_EN, SINCE_MARKERS_KO) {
            // `since_constraint`, `chinese_temporal_periods.py:451-454`: from
            // the period's start to the end of today — and the sentinel, not
            // a range, when that start is in the future. "since next week"
            // has no window; building one inverts it (start > end), which the
            // SQL survives but scoring does not.
            if start_ms > now_ms {
                return Some(Constraint::Unconstrainable);
            }
            return Some(Constraint::Range {
                start_ms,
                end_ms: midnight_ms(today)? + DAY_MS - 1,
            });
        }
        return Some(Constraint::Range {
            start_ms,
            end_ms: midnight_ms(end)? + DAY_MS - 1,
        });
    }

    // Fallback. legacy runs dateparser here; MemGarden ships none, so this is
    // an explicit-date scan — and, like legacy, it reads the ORIGINAL query.
    let day = fallback_date(query)?;
    let start_ms = midnight_ms(day)?;
    Some(Constraint::Range {
        start_ms,
        end_ms: start_ms + DAY_MS - 1,
    })
}

fn is_recurring(lower: &str) -> bool {
    if RECURRING
        .iter()
        .any(|m| contains_expression(lower, m).is_some())
    {
        return true;
    }
    RECUR_QUANTIFIERS.iter().any(|q| {
        contains_expression(lower, q).is_some_and(|(_, end)| {
            let rest = lower[end..].trim_start();
            RECUR_UNITS
                .iter()
                .any(|u| contains_expression(rest, u).is_some_and(|(i, _)| i == 0))
        })
    })
}

/// First rule whose phrase appears, in `RULES` order.
fn find_period(lower: &str) -> Option<((usize, usize), Period)> {
    RULES.iter().find_map(|(phrases, period)| {
        phrases
            .iter()
            .find_map(|p| contains_expression(lower, p))
            .map(|span| (span, *period))
    })
}

/// Is one of these markers *adjacent* to the matched period — the English
/// word immediately before it, or the Korean particle immediately after it?
///
/// Adjacency matters. A bare scan for "after" would turn "what did we decide
/// after the meeting last week" into an open-ended query, which is not what
/// it says; "since last week" and "지난주부터" are.
fn marker_before(lower: &str, span: (usize, usize), en: &[&str], ko: &[&str]) -> bool {
    let (start, end) = span;
    let head = lower[..start].trim_end();
    if en.iter().any(|m| head.ends_with(m)) {
        // Guard the boundary: "...bloomsince" is not "since".
        let cut = en.iter().find(|m| head.ends_with(**m)).unwrap().len();
        let before = head[..head.len() - cut].chars().next_back();
        if before.is_none_or(|c| !c.is_alphanumeric() && c != '_') {
            return true;
        }
    }
    let tail = lower[end..].trim_start();
    ko.iter().any(|m| tail.starts_with(m))
}

fn resolve(period: Period, today: Date) -> Option<(Date, Date)> {
    let add = |d: Date, days: i64| d.checked_add(Span::new().days(days)).ok();
    // Monday = 0, matching Python's `datetime.weekday()`.
    let weekday = i64::from(today.weekday().to_monday_zero_offset());
    match period {
        Period::Day(n) => add(today, n).map(|d| (d, d)),
        Period::Days(a, b) => Some((add(today, a)?, add(today, b)?)),
        Period::Week(n) => {
            let start = add(today, -weekday + n * 7)?;
            Some((start, add(start, 6)?))
        }
        Period::Weekend(n) => {
            // legacy `temporal_periods.py:130-136`: walk back to the most
            // recent Saturday, and on a Saturday take the previous one.
            let mut days_since_sat = (weekday + 2) % 7;
            if days_since_sat == 0 {
                days_since_sat = 7;
            }
            let sat = add(today, -(days_since_sat + (n - 1) * 7))?;
            Some((sat, add(sat, 1)?))
        }
        Period::Month(n) => {
            let first = today
                .first_of_month()
                .checked_add(Span::new().months(n))
                .ok()?;
            Some((first, first.last_of_month()))
        }
        Period::Year(n) => {
            let first = today
                .first_of_year()
                .checked_add(Span::new().years(n))
                .ok()?;
            Some((first, first.last_of_year()))
        }
    }
}

/// The fallback parser, fed the **original** (non-lowercased) query —
/// `query_analyzer.py:253`. Legacy's dateparser understands prose; this
/// understands ISO-8601, which is what an agent transcript actually contains.
/// First parseable token wins (legacy scores and takes the strongest,
/// `:270-283`).
///
/// `// ponytail: ISO only. Add prose dates when a real query needs one — the
/// period rules above already cover every relative form these banks use.`
fn fallback_date(original: &str) -> Option<Date> {
    original
        .split_whitespace()
        .map(|tok| {
            tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '+' && c != ':')
        })
        // Extended format only. `jiff` also accepts basic ISO (`20260715`),
        // which would read a bare 8-digit id — a node number, a port, a hash
        // prefix — as a date. The separator is what makes it a date.
        .filter(|tok| tok.len() >= 10 && tok.contains('-'))
        .find_map(|tok| parse_iso_ms(tok).and_then(date_of))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-02T04:55:41Z — a **Sunday**, which is the interesting weekday
    /// for the Monday-zero week arithmetic and the weekend rule.
    const NOW: i64 = 1_785_646_541_000;

    fn range(query: &str) -> Option<(String, String)> {
        match extract_constraint(query, NOW) {
            Some(Constraint::Range { start_ms, end_ms }) => Some((
                jiff::Timestamp::from_millisecond(start_ms)
                    .unwrap()
                    .to_string(),
                jiff::Timestamp::from_millisecond(end_ms)
                    .unwrap()
                    .to_string(),
            )),
            _ => None,
        }
    }

    fn days(query: &str) -> Option<(String, String)> {
        range(query).map(|(a, b)| (a[..10].to_string(), b[..10].to_string()))
    }

    #[test]
    fn english_period_set() {
        assert_eq!(days("what happened yesterday"), day("2026-08-01"));
        assert_eq!(days("what did we decide today"), day("2026-08-02"));
        // Sunday 2026-08-02: this week is Mon 07-27..Sun 08-02.
        assert_eq!(
            days("what did we decide this week"),
            span("2026-07-27", "2026-08-02")
        );
        assert_eq!(
            days("what did we decide last week"),
            span("2026-07-20", "2026-07-26")
        );
        assert_eq!(days("next week plans"), span("2026-08-03", "2026-08-09"));
        assert_eq!(
            days("last month's decisions"),
            span("2026-07-01", "2026-07-31")
        );
        assert_eq!(days("this month so far"), span("2026-08-01", "2026-08-31"));
        assert_eq!(days("next month roadmap"), span("2026-09-01", "2026-09-30"));
        assert_eq!(
            days("last year we chose rust"),
            span("2025-01-01", "2025-12-31")
        );
        assert_eq!(days("a few days ago"), span("2026-07-28", "2026-07-31"));
        assert_eq!(
            days("a couple of days ago"),
            span("2026-07-30", "2026-08-01")
        );
        // NOW is a Sunday, and legacy walks back to the most recent Saturday
        // (`temporal_periods.py:130-136`) — so on a Sunday "last weekend" is
        // yesterday and today, not the previous calendar weekend. Ported as
        // written.
        assert_eq!(days("last weekend"), span("2026-08-01", "2026-08-02"));
    }

    #[test]
    fn korean_period_set() {
        // The banks are Korean-heavy; these are the forms that actually show up.
        assert_eq!(days("어제 뭘 했지"), day("2026-08-01"));
        assert_eq!(days("오늘 결정한 것"), day("2026-08-02"));
        assert_eq!(
            days("지난주에 뭘 결정했지"),
            span("2026-07-20", "2026-07-26")
        );
        assert_eq!(days("저번 주 회의 내용"), span("2026-07-20", "2026-07-26"));
        assert_eq!(days("이번 주 진행 상황"), span("2026-07-27", "2026-08-02"));
        assert_eq!(days("다음 달 계획"), span("2026-09-01", "2026-09-30"));
        assert_eq!(days("지난달 지표"), span("2026-07-01", "2026-07-31"));
        assert_eq!(days("작년에 정한 규칙"), span("2025-01-01", "2025-12-31"));
        assert_eq!(days("그저께 있었던 일"), day("2026-07-31"));
        assert_eq!(
            days("며칠 전에 고친 버그"),
            span("2026-07-28", "2026-07-31")
        );
    }

    /// Korean has no word boundary, so `지난주말` contains `지난주`. The
    /// weekend rule has to come first or the weekend query silently widens
    /// into a whole week.
    #[test]
    fn korean_weekend_is_not_swallowed_by_the_week_rule() {
        // The week rule would have answered 07-20..07-26; the weekend rule
        // answers with a two-day span (Sat/Sun of the reference weekend).
        assert_eq!(days("지난주말에 뭐 했지"), span("2026-08-01", "2026-08-02"));
        assert_eq!(days("지난 주말 배포"), span("2026-08-01", "2026-08-02"));
        assert_ne!(days("지난주말에 뭐 했지"), days("지난주에 뭐 했지"));
    }

    /// Review round MEDIUM-1: the *prefix* direction of the same containment
    /// problem. `지지난주` contains `지난주` and `재작년` contains `작년`, so
    /// before the longer forms were listed first these did not widen into a
    /// superset — they returned a **wrong** period, one step too late, with
    /// no signal that anything was off.
    #[test]
    fn korean_double_prefix_is_not_swallowed_by_its_container() {
        assert_eq!(days("지지난주에 뭐 했지"), span("2026-07-13", "2026-07-19"));
        assert_eq!(days("지지난 주 회의"), span("2026-07-13", "2026-07-19"));
        assert_eq!(days("지지난달 지표"), span("2026-06-01", "2026-06-30"));
        assert_eq!(days("지지난 달 지표"), span("2026-06-01", "2026-06-30"));
        assert_eq!(days("재작년에 정한 규칙"), span("2024-01-01", "2024-12-31"));
        assert_eq!(days("지지난해 결정"), span("2024-01-01", "2024-12-31"));
        assert_eq!(days("지지난주말 배포"), span("2026-07-25", "2026-07-26"));

        // Each is exactly one period earlier than its container, which is the
        // property that broke.
        assert_ne!(days("지지난주"), days("지난주"));
        assert_ne!(days("지지난달"), days("지난달"));
        assert_ne!(days("재작년"), days("작년"));
        assert_ne!(days("지지난주말"), days("지난주말"));

        // `재작` alone is not a literal: this must not read as a year.
        assert_eq!(extract_constraint("재작업이 필요한 부분", NOW), None);
    }

    #[test]
    fn a_range_closes_at_the_last_millisecond_of_its_last_day() {
        let (start, end) = range("yesterday").unwrap();
        assert_eq!(start, "2026-08-01T00:00:00Z");
        assert_eq!(end, "2026-08-01T23:59:59.999Z");
    }

    /// The third state. "every Monday" is a recognized temporal expression
    /// with no date range behind it — and it must NOT reach the fallback.
    #[test]
    fn no_temporal_constraint_short_circuits_before_the_fallback() {
        for q in [
            "every monday",
            "we sync every week",
            "매주 월요일 회의",
            "매일 하는 일",
        ] {
            assert_eq!(
                extract_constraint(q, NOW),
                Some(Constraint::Unconstrainable),
                "{q}"
            );
        }

        // The ordering proof: both queries carry an expression the rules
        // below would happily turn into a range — a period phrase, and a
        // date the fallback parses. Neither may produce one.
        for q in [
            "every monday, same as last week",
            "every monday since 2026-07-15",
        ] {
            assert_eq!(
                extract_constraint(q, NOW),
                Some(Constraint::Unconstrainable),
                "{q}"
            );
            assert!(range(q).is_none(), "{q} must produce NO range");
        }

        // ...and the same queries without the recurrence marker DO produce
        // one, which is what makes the assertion above meaningful.
        assert!(range("same as last week").is_some());
        assert!(range("what shipped on 2026-07-15").is_some());
    }

    #[test]
    fn every_needs_a_time_unit_after_it() {
        // "every detail" is not a temporal expression.
        assert_eq!(
            extract_constraint("every detail of yesterday", NOW),
            range_of("2026-08-01")
        );
    }

    /// Review round MEDIUM-3: a bare English adjective is not a recurrence
    /// marker. These queries name a window *and* a cadence, and the window is
    /// the part the caller can act on.
    #[test]
    fn english_recurrence_adjectives_do_not_eat_the_window() {
        assert_eq!(
            days("the weekly report from last week"),
            span("2026-07-20", "2026-07-26")
        );
        assert_eq!(
            days("monthly metrics we reviewed yesterday"),
            day("2026-08-01")
        );
        // Real English recurrence still short-circuits, via `every|each`.
        assert_eq!(
            extract_constraint("the report we write every week", NOW),
            Some(Constraint::Unconstrainable)
        );
    }

    #[test]
    fn open_ended_before_is_unconstrainable_but_since_is_a_range() {
        assert_eq!(
            extract_constraint("what did we decide before last week", NOW),
            Some(Constraint::Unconstrainable)
        );
        assert_eq!(
            extract_constraint("지난주 이전 결정", NOW),
            Some(Constraint::Unconstrainable)
        );
        // "since last week" runs from the period's start to the end of today
        // (legacy closes on the reference *date*, not the reference instant).
        let (start, end) = range("since last week").unwrap();
        assert_eq!(start, "2026-07-20T00:00:00Z");
        assert_eq!(end, "2026-08-02T23:59:59.999Z");
        assert_eq!(range("지난주부터의 변경"), range("since last week"));

        // Adjacency: a marker that is not next to the period does not change it.
        assert_eq!(
            days("what did we decide after the meeting last week"),
            span("2026-07-20", "2026-07-26")
        );
        // `from` is NOT a since-marker: "notes from yesterday" means yesterday.
        assert_eq!(days("notes from yesterday"), day("2026-08-01"));
    }

    /// Review round MEDIUM-2: a since-window whose start is in the future has
    /// no range at all. Building one inverts it (start > end); the SQL
    /// survives that (BETWEEN matches nothing) but scoring did not — an
    /// inverted window hit the zero-width shortcut and handed every dated
    /// candidate a 1.0 proximity, i.e. a uniform boost on a query where the
    /// arm returned nothing. legacy: `chinese_temporal_periods.py:451-454`.
    #[test]
    fn since_a_future_period_is_unconstrainable_not_an_inverted_range() {
        for q in [
            "since next week",
            "since tomorrow",
            "다음주부터",
            "내일부터의 계획",
            "since next month",
        ] {
            assert_eq!(
                extract_constraint(q, NOW),
                Some(Constraint::Unconstrainable),
                "{q}"
            );
        }
        // A plain future period is still a perfectly good (forward) window —
        // only the *since* form is unconstrainable.
        assert_eq!(days("next week plans"), span("2026-08-03", "2026-08-09"));
    }

    /// legacy matches on the lowercased query (`query_analyzer.py:228-229`)
    /// but hands the ORIGINAL to the date parser (`:253`).
    #[test]
    fn matching_is_case_insensitive_and_the_fallback_gets_the_original() {
        // The matching half: rules run on the lowercased copy, so case in the
        // query cannot hide a period.
        assert_eq!(days("What Did We Decide LAST WEEK"), days("last week"));
        // The fallback half. Review note: this does NOT prove the fallback
        // gets the original — `jiff` parses lowercase `t`/`z` identically, so
        // it would pass either way. It is here as a round trip through the
        // uppercase form; the call itself (`fallback_date(query)`, not
        // `fallback_date(&lower)`) is what carries the guarantee, and the
        // reason it matters is a parser that is not `jiff`.
        assert_eq!(
            days("Anything about 2026-07-15T09:30:00Z?"),
            day("2026-07-15")
        );
        assert_eq!(days("what shipped on 2026-07-15"), day("2026-07-15"));
    }

    #[test]
    fn no_temporal_expression_at_all_is_none() {
        assert_eq!(
            extract_constraint("why did recall latency regress", NOW),
            None
        );
        assert_eq!(extract_constraint("메모리 회수 파이프라인", NOW), None);
        // A bare number is not a date.
        assert_eq!(extract_constraint("what about node 20260715", NOW), None);
    }

    fn day(d: &str) -> Option<(String, String)> {
        Some((d.to_string(), d.to_string()))
    }
    fn span(a: &str, b: &str) -> Option<(String, String)> {
        Some((a.to_string(), b.to_string()))
    }
    fn range_of(d: &str) -> Option<Constraint> {
        let start_ms = midnight_ms(d.parse::<Date>().unwrap()).unwrap();
        Some(Constraint::Range {
            start_ms,
            end_ms: start_ms + DAY_MS - 1,
        })
    }
}
