//! Entity normalization and resolution (CE-7, PR B5).
//!
//! Legacy references: `engine/retain/entity_processing.py` (the extraction →
//! entity handoff) and `engine/retain/entity_resolver.py:684-717` (the
//! scoring formula and the 0.6 threshold). The SQL lives in
//! `memgarden_store::graph`.

use std::collections::HashSet;

use memgarden_store::graph::ResolutionContext;

/// A resolved mention scores above this to reuse an existing entity.
/// `entity_resolver.py:715` — the comparison is strict `>`, so a score that
/// lands exactly on the threshold creates a new entity.
pub const RESOLUTION_THRESHOLD: f64 = 0.6;

/// The temporal-proximity term only applies inside this window
/// (`entity_resolver.py:706`).
const TEMPORAL_WINDOW_DAYS: f64 = 7.0;

const MS_PER_DAY: f64 = 86_400_000.0;

/// A merge needs the two names to be more alike than not.
///
/// **Diverged from legacy, deliberately.** `resolution_score` weights the name
/// at 0.5 and the two circumstantial terms — co-occurrence and same-day
/// proximity — at 0.3 + 0.2, against a `RESOLUTION_THRESHOLD` of 0.6. Both
/// halves cap at exactly 0.5, so *neither can clear the gate alone*: a
/// **perfect** name match with no circumstantial support scores 0.5 and is
/// rejected, while a name with **no** similarity at all reaches the same 0.5
/// from circumstance. Identity is therefore decided by "mentioned together,
/// recently", with the name as a tiebreak — and a mention needs a ratio of
/// only 0.2 to merge when the circumstantial terms max out.
///
/// That is not a threshold anyone chose; it is what the weights and the gate
/// do when multiplied out. It is also why the exact-match short-circuit above
/// had to be added as a special case: without it an exactly-matching name was
/// *rejected* unless it happened to co-occur.
///
/// Replaying the scoring over the largest live bank (2,491 entities, 10,437
/// mentions, each name held out as if it had just arrived) produced 2,406
/// distinct merges, of which **26% rest on a name similarity below 0.5** —
/// `ollama` into `ddl`, `llm` into `legacy`, `rrf` into `critic`, every one
/// scoring 0.602-0.618, just over the gate and entirely on circumstance.
///
/// 0.5 is the smallest claim that can be stated rather than tuned: the names
/// share at least half their characters. It is far below every real spelling
/// variant the resolver exists for (`memgardend`/`memgarden` is 0.95,
/// `claude code`/`claude-code` 0.91).
const NAME_FLOOR: f64 = 0.5;

/// Canonical form of an entity name: trim + lowercase.
///
/// `to_lowercase` is Unicode-aware and Hangul has no case, so a Korean name
/// round-trips byte-identically — the assertion `korean_names_survive`
/// pins that. Recorded as a divergence (Critic Revision NIT 24): the
/// canonical name is also the *displayed* name, so an English entity comes
/// back lowercased rather than as written.
pub fn normalize(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Ratcliff/Obershelp similarity, i.e. Python's
/// `difflib.SequenceMatcher(None, a, b).ratio()`: `2 * M / T` where `M` is
/// the total size of the recursively-found matching blocks and `T` is the
/// combined length. Ported rather than pulled from a crate because it is
/// forty lines and the exact tie-breaking has to match legacy's scores.
///
/// Operates on `char`s, not bytes — Python compares code points, and a
/// byte-wise version would score Korean names on UTF-8 fragments.
pub fn ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0; // difflib: two empty sequences are identical.
    }
    2.0 * matching_size(&a, &b) as f64 / total as f64
}

/// difflib's `get_matching_blocks` reduced to just the total matched length,
/// which is all `ratio()` needs.
fn matching_size(a: &[char], b: &[char]) -> usize {
    let b2j = build_b2j(b);
    let mut queue = vec![(0usize, a.len(), 0usize, b.len())];
    let mut total = 0usize;
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest_match(a, b, &b2j, alo, ahi, blo, bhi);
        if k == 0 {
            continue;
        }
        total += k;
        if alo < i && blo < j {
            queue.push((alo, i, blo, j));
        }
        if i + k < ahi && j + k < bhi {
            queue.push((i + k, ahi, j + k, bhi));
        }
    }
    total
}

/// `b`'s element → index lists, with difflib's `autojunk` heuristic applied
/// (`difflib.py:_chain_b`): for sequences of 200+ elements, any element
/// occurring in more than 1% of them is dropped from the index and can no
/// longer anchor a match. Entity names are capped at 256 chars upstream, so
/// this is reachable — omitting it would silently diverge from legacy on the
/// longest names.
fn build_b2j(b: &[char]) -> std::collections::HashMap<char, Vec<usize>> {
    let mut b2j: std::collections::HashMap<char, Vec<usize>> = std::collections::HashMap::new();
    for (j, &ch) in b.iter().enumerate() {
        b2j.entry(ch).or_default().push(j);
    }
    if b.len() >= 200 {
        let ntest = b.len() / 100 + 1;
        b2j.retain(|_, indices| indices.len() <= ntest);
    }
    b2j
}

/// difflib's `find_longest_match`: returns `(i, j, size)`, the
/// leftmost-longest common block in `a[alo..ahi]` / `b[blo..bhi]`.
///
/// The post-DP extension pair **is** ported, and has to be. CPython gates it
/// on `isbjunk`, which reads `bjunk` — populated only from an explicit
/// `isjunk` predicate. Autojunk drops elements into `bpopular`, a *different*
/// set, so `isbjunk` stays false for them and the loops happily extend across
/// exactly the elements the DP could not anchor on. Skipping them scored
/// `ratio("a"*250, "a"*250)` as **0.0** where CPython says **1.0**. The
/// second, junk-gated pair genuinely is a no-op with `isjunk=None` and is not
/// ported.
fn longest_match(
    a: &[char],
    b: &[char],
    b2j: &std::collections::HashMap<char, Vec<usize>>,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
    // j2len[j] = length of the longest block ending at a[i-1], b[j-1].
    let mut j2len: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (offset, &ch) in a[alo..ahi].iter().enumerate() {
        let i = alo + offset;
        let mut newj2len: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        if let Some(indices) = b2j.get(&ch) {
            for &j in indices {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j.checked_sub(1).and_then(|p| j2len.get(&p)).unwrap_or(&0) + 1;
                newj2len.insert(j, k);
                if k > bestsize {
                    besti = i + 1 - k;
                    bestj = j + 1 - k;
                    bestsize = k;
                }
            }
        }
        j2len = newj2len;
    }
    // Extend over elements the DP index could not anchor on (see above).
    while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
        besti -= 1;
        bestj -= 1;
        bestsize += 1;
    }
    while besti + bestsize < ahi
        && bestj + bestsize < bhi
        && a[besti + bestsize] == b[bestj + bestsize]
    {
        bestsize += 1;
    }
    (besti, bestj, bestsize)
}

/// Two names whose digits differ are different entities, however alike the
/// letters around them.
///
/// Ratcliff/Obershelp scores `ce-6` against `ce-8` at 0.75 and `pr #28`
/// against `pr #29` at 0.83 — the character that carries the whole meaning is
/// one of the few they do not share, so a similarity metric rates them as
/// near-identical. **130 of the live bank's wrong merges sit at 0.7 or above**,
/// where no name floor reaches them: `ce-11` into `ce-9`, `phase 0` into
/// `phase e`, `vec0` into `vec`, `rusqlite 0.40.1` into `r2d2_sqlite 0.35`.
///
/// Runs rather than a set: `v1.5` and `v5.1` are not the same version, and
/// comparing `["1","5"]` to `["5","1"]` says so where a set would not.
///
/// Names with no digits on either side compare equal here and are left to the
/// similarity floor, which is every real spelling variant this resolver was
/// written for.
fn digits_differ(a: &str, b: &str) -> bool {
    fn runs(s: &str) -> Vec<String> {
        // `char::is_numeric`, not `is_ascii_digit`: an entity name can carry
        // fullwidth or Arabic-Indic digits, which share no byte with 0x30..0x39
        // and would make both sides look digit-free — the gate silently off for
        // exactly the names it exists to separate.
        let mut out: Vec<String> = Vec::new();
        let mut cur = String::new();
        for c in s.chars() {
            if c.is_numeric() {
                // Fullwidth digits fold to ASCII — `버전 ３` and `버전 3` are
                // one number written twice, and CJK input produces both.
                // Other numeral scripts keep their own characters, so such a
                // pair stays two entities: the conservative outcome, and the
                // one this gate exists to reach.
                cur.push(match c {
                    '０'..='９' => char::from(b'0' + (c as u32 - '０' as u32) as u8),
                    _ => c,
                });
            } else if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }
    runs(a) != runs(b)
}

/// `entity_resolver.py:684-712`: name similarity (0-0.5) + co-occurring
/// entity overlap (0-0.3) + temporal proximity (0-0.2).
///
/// `nearby_total` is the number of *other* entities named alongside this
/// mention; `overlap` is how many of them the candidate has co-occurred with
/// before. `days_diff` is `None` when either side has no date, which drops
/// the temporal term rather than defaulting it.
pub fn resolution_score(
    name_ratio: f64,
    overlap: usize,
    nearby_total: usize,
    days_diff: Option<f64>,
) -> f64 {
    let mut score = name_ratio * 0.5;
    if nearby_total > 0 {
        score += (overlap as f64 / nearby_total as f64) * 0.3;
    }
    if let Some(days) = days_diff
        && days < TEMPORAL_WINDOW_DAYS
    {
        score += (1.0 - days / TEMPORAL_WINDOW_DAYS).max(0.0) * 0.2;
    }
    score
}

/// Upper bound on `resolution_score` from the two names' lengths alone:
/// `ratio <= 2 * min(la, lb) / (la + lb)`, and the co-occurrence and temporal
/// terms cap at 0.3 and 0.2. Returns false when even a perfect score on
/// everything else could not clear `RESOLUTION_THRESHOLD`.
fn can_clear_threshold(len_a: usize, len_b: usize) -> bool {
    let total = len_a + len_b;
    if total == 0 {
        return true;
    }
    let ratio_ceiling = 2.0 * len_a.min(len_b) as f64 / total as f64;
    // `NAME_FLOOR` binds before the threshold does, so a pair whose lengths
    // cannot reach it is skipped without running the O(n*m) `ratio`. Without
    // this the prefilter still admitted anything above a 0.2 ceiling, which
    // has not been the binding constraint since the floor went in.
    if ratio_ceiling < NAME_FLOOR {
        return false;
    }
    // Scored through `resolution_score` itself rather than inlining
    // `* 0.5 + 0.5`: the two are not the same double (0.1+0.3+0.2 lands one
    // ulp above 0.6) and the bound has to be exact, not approximately exact.
    resolution_score(ratio_ceiling, 1, 1, Some(0.0)) > RESOLUTION_THRESHOLD
}

/// [`resolve_fact`]'s first half on its own: trim + lowercase, empties
/// dropped, duplicates within the fact dropped.
///
/// `pub` because MG-1's importer wants exactly this and *not* the fuzzy pass
/// that follows it — legacy already merged its own spelling variants, so
/// scoring already-canonical names against each other only produces false
/// merges (`migrate::import::write_entities`). It was six duplicated lines
/// there until review pointed out that the invariant "these two agree" was
/// held by a doc comment; now it is held by the compiler.
///
/// The dedup is load-bearing on both paths: a fact naming the same entity
/// twice must not inflate `mention_count` or fabricate a self-pair in
/// `entity_cooccurrences`, whose CHECK would reject `a = a` anyway.
pub fn normalized_mentions(mentions: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    mentions
        .iter()
        .map(|m| normalize(m))
        .filter(|m| !m.is_empty() && seen.insert(m.clone()))
        .collect()
}

/// Resolves one fact's entity mentions to canonical names: each mention
/// either matches an existing entity (score `> 0.6`) and adopts its canonical
/// name, or keeps its own normalized name and becomes a new entity when the
/// batch is written.
///
/// Empty and duplicate mentions are dropped first — a fact naming the same
/// entity twice must not inflate `mention_count` or fabricate a self-pair in
/// `entity_cooccurrences` (the CHECK would reject `a = a` anyway).
pub fn resolve_fact(
    mentions: &[String],
    event_date_ms: Option<i64>,
    ctx: &ResolutionContext,
) -> Vec<String> {
    let normalized = normalized_mentions(mentions);
    if normalized.is_empty() {
        return vec![];
    }

    normalized
        .iter()
        .map(|mention| {
            // `nearby` = the other entities in this same fact, lowercased
            // (`entity_resolver.py:682`).
            let nearby: HashSet<&str> = normalized
                .iter()
                .filter(|n| n.as_str() != mention.as_str())
                .map(String::as_str)
                .collect();

            let mut best_score = 0.0f64;
            let mut best: Option<&str> = None;
            let mention_len = mention.chars().count();
            for candidate in &ctx.candidates {
                // An exact name is the entity, and nothing can outrank it.
                //
                // Without this the argmax can hand the mention to a *different*
                // entity, because the score is only half about the name:
                // `ratio*0.5 + overlap*0.3 + temporal*0.2`. An exact match on a
                // candidate whose `last_seen` is old scores at most 0.8 — the
                // temporal term is zero — while a fresher, co-occurring
                // near-match reaches `0.9*0.5 + 0.3 + 0.2 = 0.95` and wins.
                //
                // A migrated bank is where this bites: every entity carries its
                // *legacy* date, so months after cutover an exactly-matching
                // migrated candidate is permanently the stale one, and a later
                // `CE-4` can be routed onto `ce-1` rather than onto the migrated
                // `ce-4` row it names.
                //
                // Unambiguous by schema: `entities` is
                // `UNIQUE (bank_id, canonical_name)` and the candidates come
                // from one bank, so at most one can match exactly.
                if candidate.canonical_name.as_str() == mention.as_str() {
                    best = Some(candidate.canonical_name.as_str());
                    best_score = 1.0;
                    break;
                }
                // Cheap length bound before the O(n*m) `ratio`. Ratcliff/
                // Obershelp cannot exceed `2*min(len)/(len_a+len_b)`, and the
                // other two terms cap at 0.3 + 0.2, so a candidate whose best
                // conceivable total cannot clear the threshold is skipped
                // without ever being compared. Pure speed: the skipped
                // candidates provably could not have won.
                if !can_clear_threshold(mention_len, candidate.canonical_name.chars().count()) {
                    continue;
                }
                // Two gates the circumstantial terms cannot talk their way
                // past. Cheapest first: `digits_differ` is a scan, `ratio` is
                // the O(n*m) Ratcliff/Obershelp pass.
                if digits_differ(mention, &candidate.canonical_name) {
                    continue;
                }
                let name_ratio = ratio(mention, &candidate.canonical_name);
                if name_ratio < NAME_FLOOR {
                    continue;
                }
                let overlap = ctx
                    .cooccurring
                    .get(&candidate.id)
                    .map(|names| nearby.iter().filter(|n| names.contains(**n)).count())
                    .unwrap_or(0);
                let days_diff = match (candidate.last_seen, event_date_ms) {
                    (Some(last), Some(now)) => Some((now - last).abs() as f64 / MS_PER_DAY),
                    _ => None,
                };
                let score = resolution_score(name_ratio, overlap, nearby.len(), days_diff);
                if score > best_score {
                    best_score = score;
                    best = Some(candidate.canonical_name.as_str());
                }
            }
            match best {
                Some(name) if best_score > RESOLUTION_THRESHOLD => name.to_string(),
                _ => mention.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use memgarden_store::graph::EntityCandidate;

    /// Reference values taken from CPython's
    /// `difflib.SequenceMatcher(None, a, b).ratio()`.
    #[test]
    fn ratio_matches_python_sequencematcher() {
        let cases: &[(&str, &str, f64)] = &[
            ("ollama", "ollama", 1.0),
            ("ollama", "olama", 0.909_090_909_090_909_1),
            ("postgres", "postgresql", 0.888_888_888_888_888_8),
            ("recall", "retain", 0.5),
            ("메모리 시스템", "메모리시스템", 0.923_076_923_076_923_1),
            ("abc", "", 0.0),
            ("", "", 1.0),
            ("kitten", "sitting", 0.615_384_615_384_615_4),
            ("claude code", "claude-code", 0.909_090_909_090_909_1),
            ("memgarden", "hindsight", 0.111_111_111_111_111_1),
        ];
        // The ten above are all short. These two cross the 200-element
        // autojunk gate, where the DP index drops every repeated element and
        // only the post-DP extension loops can recover the match — the exact
        // case the first port got wrong (rust 0.0 vs python 1.0).
        let repeated = "a".repeat(250);
        let prose: String = PROSE.repeat(3).chars().take(252).collect();
        let mutated: String = prose
            .chars()
            .take(251)
            .chain(std::iter::once('X'))
            .collect();
        let long_cases: Vec<(&str, &str, f64)> = vec![
            (repeated.as_str(), repeated.as_str(), 1.0),
            (prose.as_str(), prose.as_str(), 1.0),
            (prose.as_str(), mutated.as_str(), 0.996_031_746_031_746),
        ];
        assert_eq!(prose.chars().count(), 252, "must cross the 200-char gate");

        for (a, b, expected) in cases.iter().chain(long_cases.iter()) {
            let got = ratio(a, b);
            assert!(
                (got - expected).abs() < 1e-12,
                "ratio({a:?}, {b:?}) = {got}, python says {expected}"
            );
            assert!(
                (ratio(b, a) - ratio(a, b)).abs() < 1e-12,
                "ratio must be symmetric for {a:?}/{b:?}"
            );
        }
    }

    const PROSE: &str = "the daemon binds 127.0.0.1:9100 and the recall pipeline fuses \
                         bm25 with vector candidates using reciprocal rank fusion over a \
                         sqlite backed store that keeps embeddings in a vec0 partition table ";

    #[test]
    fn length_prefilter_never_skips_a_candidate_that_could_have_won() {
        // Exhaustive over the length pairs an entity name can take: the
        // prefilter must reject only pairs that could not have won *under the
        // rules as they stand*. Since `NAME_FLOOR`, that is two conditions —
        // clearing the threshold is no longer sufficient, because a pair whose
        // length ceiling cannot reach the floor is rejected by `resolve_fact`
        // however good its circumstantial terms are.
        for la in 1..=64usize {
            for lb in 1..=64usize {
                let ceiling = 2.0 * la.min(lb) as f64 / (la + lb) as f64;
                let best_possible = resolution_score(ceiling, 1, 1, Some(0.0));
                let could_win = best_possible > RESOLUTION_THRESHOLD && ceiling >= NAME_FLOOR;
                assert_eq!(
                    can_clear_threshold(la, lb),
                    could_win,
                    "la={la} lb={lb} ceiling={ceiling} best={best_possible}"
                );
            }
        }
        // A 3-char name against a 30-char one can never win; equal lengths
        // always survive the filter.
        assert!(!can_clear_threshold(3, 30));
        assert!(can_clear_threshold(30, 30));
    }

    #[test]
    fn normalize_trims_and_lowercases_without_mangling_korean() {
        assert_eq!(normalize("  Ollama  "), "ollama");
        assert_eq!(normalize("PostgreSQL"), "postgresql");
        // Hangul has no case: the name must survive byte-identically.
        assert_eq!(normalize("  메모리 시스템 "), "메모리 시스템");
        assert_eq!(normalize("제트슨 자비에"), "제트슨 자비에");
        // Mixed script: only the Latin half changes.
        assert_eq!(normalize("Jetson 자비에"), "jetson 자비에");
        assert_eq!(normalize("   "), "");
    }

    #[test]
    fn resolution_score_terms_are_weighted_as_legacy() {
        // Name only.
        assert_eq!(resolution_score(1.0, 0, 0, None), 0.5);
        // Full overlap adds the whole 0.3.
        assert_eq!(resolution_score(0.0, 2, 2, None), 0.3);
        // Half overlap adds half of it.
        assert!((resolution_score(0.0, 1, 2, None) - 0.15).abs() < 1e-12);
        // Same-day adds the whole 0.2; 3.5 days adds half; 7+ days adds none.
        assert!((resolution_score(0.0, 0, 0, Some(0.0)) - 0.2).abs() < 1e-12);
        assert!((resolution_score(0.0, 0, 0, Some(3.5)) - 0.1).abs() < 1e-12);
        assert_eq!(resolution_score(0.0, 0, 0, Some(7.0)), 0.0);
        assert_eq!(resolution_score(0.0, 0, 0, Some(700.0)), 0.0);
        // No date at all is not the same as a distant date — the term is
        // skipped, not zeroed through the window check.
        assert_eq!(resolution_score(0.4, 0, 0, None), 0.2);
    }

    fn ctx(candidates: Vec<EntityCandidate>) -> ResolutionContext {
        ResolutionContext {
            candidates,
            cooccurring: Default::default(),
        }
    }

    /// The threshold is a strict `>`: 0.5999 creates a new entity, 0.6001
    /// reuses the existing one.
    #[test]
    fn resolution_threshold_is_strict_at_zero_point_six() {
        let candidate = |name: &str| {
            vec![EntityCandidate {
                id: 1,
                canonical_name: name.to_string(),
                last_seen: None,
            }]
        };

        // name_ratio * 0.5 is the only live term (no nearby, no dates), so
        // the boundary in name-similarity terms is exactly 1.2.
        let below = ratio("ollama", "olama") * 0.5; // 0.4545…
        assert!(below < RESOLUTION_THRESHOLD);
        assert_eq!(
            resolve_fact(&["olama".to_string()], None, &ctx(candidate("ollama"))),
            vec!["olama".to_string()],
            "below threshold must keep its own name"
        );

        // Push it over with the co-occurrence and temporal terms:
        // 0.4545 + 0.3 = 0.7545 > 0.6.
        let mut c = ctx(candidate("ollama"));
        c.cooccurring
            .insert(1, ["qwen3"].iter().map(|s| s.to_string()).collect());
        assert_eq!(
            resolve_fact(&["olama".to_string(), "qwen3".to_string()], None, &c)[0],
            "ollama",
            "above threshold must adopt the candidate's canonical name"
        );

        // Directly on the boundary: 0.6 * 0.5 + 1/1 * 0.3 lands on exactly
        // 0.6, which must NOT resolve; a hair above it must.
        assert_eq!(resolution_score(0.6, 1, 1, None), 0.6);
        assert!(resolution_score(0.6, 1, 1, None) <= RESOLUTION_THRESHOLD);
        assert!(resolution_score(0.600_2, 1, 1, None) > RESOLUTION_THRESHOLD);
    }

    #[test]
    fn resolve_fact_drops_empties_and_duplicates() {
        let out = resolve_fact(
            &[
                "Ollama".to_string(),
                "  ".to_string(),
                "ollama".to_string(),
                "Qwen3".to_string(),
            ],
            None,
            &ctx(vec![]),
        );
        assert_eq!(out, vec!["ollama".to_string(), "qwen3".to_string()]);
    }

    /// R6's candidate-recall property: resolution scores against *every*
    /// entity in the bank, so a match that a prefix or trigram prefilter
    /// would have missed still resolves.
    /// R6's candidate-recall property, and the reason the name term alone
    /// can never resolve anything: `name_ratio * 0.5` peaks at 0.5, below the
    /// 0.6 threshold, so a near-name match needs the co-occurrence or
    /// temporal term to carry it over. (An *exact* name match never reaches
    /// the resolver — it collides on `UNIQUE (bank_id, canonical_name)`.)
    #[test]
    fn full_scan_finds_a_candidate_no_prefix_index_would() {
        let now = 1_785_000_000_000i64;
        let candidates = (0..500)
            .map(|i| EntityCandidate {
                id: i,
                canonical_name: format!("unrelated entity number {i}"),
                last_seen: Some(now),
            })
            .chain(std::iter::once(EntityCandidate {
                id: 999,
                // Shares no leading character with the mention.
                canonical_name: "the memgarden daemon".to_string(),
                last_seen: Some(now),
            }))
            .collect();
        // 0.888 * 0.5 + same-day 0.2 = 0.644 > 0.6.
        let out = resolve_fact(
            &["memgarden daemon".to_string()],
            Some(now),
            &ctx(candidates),
        );
        assert_eq!(out, vec!["the memgarden daemon".to_string()]);
    }

    #[test]
    fn korean_entity_resolves_against_a_korean_candidate() {
        let candidates = vec![EntityCandidate {
            id: 1,
            canonical_name: "메모리 시스템".to_string(),
            last_seen: None,
        }];
        // ratio 0.923 * 0.5 = 0.46 — under the threshold on name alone, so
        // add same-day proximity (+0.2) to clear it: 0.66.
        let now = 1_785_000_000_000i64;
        let mut c = ctx(candidates);
        c.candidates[0].last_seen = Some(now);
        assert_eq!(
            resolve_fact(&["메모리시스템".to_string()], Some(now), &c),
            vec!["메모리 시스템".to_string()]
        );
    }

    /// The floor's own case, taken from the live bank: `ollama` merged into
    /// `ddl` at 0.611 — a name similarity of 0.22 carried over the gate by
    /// full co-occurrence and same-day proximity. Nothing about the names
    /// suggests they are the same thing, and nothing in the old scoring
    /// looked at that.
    #[test]
    fn circumstance_alone_can_no_longer_merge_two_unlike_names() {
        let now = 1_785_000_000_000i64;
        let mut c = ctx(vec![EntityCandidate {
            id: 1,
            canonical_name: "ddl".to_string(),
            last_seen: Some(now),
        }]);
        c.cooccurring
            .insert(1, ["sqlite".to_string()].into_iter().collect());

        // What the old scoring made of it, kept as an assertion so the test
        // fails if the weights move rather than silently agreeing.
        let r = ratio("ollama", "ddl");
        assert!(
            resolution_score(r, 1, 1, Some(0.0)) > RESOLUTION_THRESHOLD,
            "the pre-fix score cleared the gate on circumstance alone"
        );
        assert!(r < NAME_FLOOR);

        let out = resolve_fact(&["ollama".to_string(), "sqlite".to_string()], Some(now), &c);
        assert_eq!(
            out[0], "ollama",
            "a name this unlike must stay its own entity"
        );
    }

    /// `ce-6` against `ce-8` scores 0.75 on name similarity — comfortably over
    /// the floor — because the one character that carries the entire meaning
    /// is the one they do not share. 130 of the live bank's wrong merges sit
    /// at 0.7 or above, so the floor alone would not reach them.
    #[test]
    fn a_differing_digit_blocks_a_merge_the_floor_would_allow() {
        let now = 1_785_000_000_000i64;
        let mut c = ctx(vec![EntityCandidate {
            id: 1,
            canonical_name: "ce-8".to_string(),
            last_seen: Some(now),
        }]);
        c.cooccurring
            .insert(1, ["retain".to_string()].into_iter().collect());

        let r = ratio("ce-6", "ce-8");
        assert!(r > NAME_FLOOR, "the floor does not reach this pair");
        assert!(resolution_score(r, 1, 1, Some(0.0)) > RESOLUTION_THRESHOLD);

        let out = resolve_fact(&["ce-6".to_string(), "retain".to_string()], Some(now), &c);
        assert_eq!(out[0], "ce-6", "a different number is a different entity");
    }

    /// The variants the resolver exists for must still merge. Both gates are
    /// silent here: no digits on either side, and 0.95 is nowhere near the
    /// floor.
    #[test]
    fn a_real_spelling_variant_still_merges() {
        let now = 1_785_000_000_000i64;
        let c = ctx(vec![EntityCandidate {
            id: 1,
            canonical_name: "memgarden".to_string(),
            last_seen: Some(now),
        }]);
        assert!(ratio("memgardend", "memgarden") > NAME_FLOOR);
        assert!(!digits_differ("memgardend", "memgarden"));
        let out = resolve_fact(&["memgardend".to_string()], Some(now), &c);
        assert_eq!(out[0], "memgarden");
    }

    /// Runs in order, not a set of digits: `v1.5` and `v5.1` use the same two
    /// characters and are not the same version.
    /// Reported by review: `is_ascii_digit` sees no digit in a fullwidth or
    /// Arabic-Indic numeral, so both sides looked digit-free and the gate was
    /// silently off for exactly the names it exists to separate. Folding to
    /// the ASCII value also makes `버전 ３` and `버전 3` one entity, which they
    /// are — the same number written twice.
    #[test]
    fn non_ascii_digits_are_seen_and_folded() {
        assert!(
            digits_differ("버전 ３", "버전 ２"),
            "fullwidth digits differ"
        );
        assert!(
            !digits_differ("버전 ３", "버전 3"),
            "same number, two scripts"
        );
        assert!(digits_differ("ce-٣", "ce-٤"), "arabic-indic digits differ");
        assert!(!digits_differ("버전", "버전 이름"), "no digits either side");
    }

    #[test]
    fn digits_compare_as_ordered_runs() {
        assert!(digits_differ("v1.5", "v5.1"));
        assert!(digits_differ("pr #28", "pr #29"));
        assert!(digits_differ("vec0", "vec"), "a digit against none differs");
        assert!(!digits_differ("ac-1", "ac-1 criteria"));
        assert!(
            !digits_differ("retain", "retain cap"),
            "no digits, no opinion"
        );
        assert!(!digits_differ("rusqlite 0.40.1", "rusqlite 0.40.1 pin"));
    }

    /// An exactly-matching candidate must win even when a near-match outscores
    /// it — which it can, because only half the score is about the name.
    ///
    /// This is the shape a migrated bank makes permanent. Every migrated entity
    /// keeps its *legacy* `last_seen`, so its temporal term is zero forever,
    /// while entities written after cutover are fresh. Here `ce-4` is the
    /// migrated one and `ce-1` the fresh neighbour:
    ///
    /// * `ce-4` exact — `1.0*0.5` + no co-occurrence + no proximity = **0.50**
    /// * `ce-1` near  — `ratio*0.5` + full overlap `0.3` + same-day `0.2`
    ///
    /// `ratio("ce-4", "ce-1")` is 0.75, so `ce-1` scores 0.875 and the argmax
    /// hands `ce-4`'s mention to `ce-1`. MG-1b measured this class at 77 of
    /// 3,917 names when the importer ran the resolver, `ce-4` into `ce-1` among
    /// them; the same conditions recur on every retain after cutover.
    #[test]
    fn an_exact_name_beats_a_fresher_co_occurring_near_match() {
        let now = 1_785_000_000_000i64;
        let legacy_date = now - 400 * 86_400_000; // long outside the 7-day window

        let mut c = ctx(vec![
            EntityCandidate {
                id: 1,
                canonical_name: "ce-4".to_string(),
                last_seen: Some(legacy_date),
            },
            EntityCandidate {
                id: 2,
                canonical_name: "ce-1".to_string(),
                last_seen: Some(now),
            },
        ]);
        // `ce-1` has co-occurred with the other mention in this fact before.
        c.cooccurring
            .insert(2, ["retain".to_string()].into_iter().collect());

        let out = resolve_fact(&["ce-4".to_string(), "retain".to_string()], Some(now), &c);
        assert_eq!(
            out[0], "ce-4",
            "an exact name must not be absorbed by a better-scoring neighbour"
        );
    }
}
