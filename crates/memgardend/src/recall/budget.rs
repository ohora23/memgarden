//! Token budget: how many tokens of recalled text may be injected, and the
//! greedy filter that enforces it. Ported from `engine/memory_engine.py`.

/// `low | mid | high` -> fixed token budget (`memory_engine.py:776-778`,
/// `config.py:1246-1248`). Anything else falls back to MID, which is also
/// legacy's default for a `None` budget (`memory_engine.py:761`).
pub fn budget_tokens(level: &str) -> usize {
    match level {
        "low" => 100,
        "high" => 1000,
        _ => 300,
    }
}

/// Candidates carried into token filtering (`memory_engine.py:5266`:
/// `rerank_limit = thinking_budget * 2`).
pub fn rerank_limit(thinking_budget: usize) -> usize {
    thinking_budget * 2
}

/// Greedy fit of `texts` into `max_tokens`, counting the **text field only**
/// (`memory_engine.py:5893-5920`). Returns how many leading items fit and
/// their total token count.
///
/// It **breaks** on the first item that would overflow — it does not skip it
/// and keep looking for a smaller one. That quirk is deliberate: the AC-1
/// A/B compares MemGarden's injections against legacy's, so "one long fact
/// truncates everything after it" has to be reproduced, not improved.
pub fn fit_to_budget(
    texts: &[String],
    max_tokens: usize,
    count: impl Fn(&str) -> u64,
) -> (usize, u64) {
    let mut total = 0u64;
    for (i, text) in texts.iter().enumerate() {
        let tokens = count(text);
        if total + tokens > max_tokens as u64 {
            return (i, total);
        }
        total += tokens;
    }
    (texts.len(), total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One "token" per character, so the boundary arithmetic is exact.
    fn chars(s: &str) -> u64 {
        s.chars().count() as u64
    }

    fn texts(lens: &[usize]) -> Vec<String> {
        lens.iter().map(|&n| "x".repeat(n)).collect()
    }

    #[test]
    fn exact_fit_is_included() {
        let (kept, total) = fit_to_budget(&texts(&[6, 4]), 10, chars);
        assert_eq!((kept, total), (2, 10), "`<=` budget, not `<`");
    }

    #[test]
    fn one_over_the_boundary_stops() {
        let (kept, total) = fit_to_budget(&texts(&[6, 5]), 10, chars);
        assert_eq!((kept, total), (1, 6));
    }

    #[test]
    fn overflow_breaks_it_does_not_skip() {
        // The 100-token item does not fit; the 1-token item after it would.
        // Legacy `break`s, so it is dropped too — port the quirk.
        let (kept, total) = fit_to_budget(&texts(&[5, 100, 1]), 10, chars);
        assert_eq!(
            (kept, total),
            (1, 5),
            "must break on first overflow, not continue past it"
        );
    }

    #[test]
    fn empty_and_zero_budget() {
        assert_eq!(fit_to_budget(&[], 10, chars), (0, 0));
        assert_eq!(fit_to_budget(&texts(&[1]), 0, chars), (0, 0));
    }

    #[test]
    fn budget_levels() {
        assert_eq!(budget_tokens("low"), 100);
        assert_eq!(budget_tokens("mid"), 300);
        assert_eq!(budget_tokens("high"), 1000);
        assert_eq!(budget_tokens("nonsense"), 300, "unknown falls back to mid");
        assert_eq!(rerank_limit(budget_tokens("mid")), 600);
    }
}
