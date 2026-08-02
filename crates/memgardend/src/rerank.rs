//! Embedded cross-encoder reranking (CE-11) — **off by default**.
//!
//! `Xenova/ms-marco-MiniLM-L-6-v2` is the ONNX export of
//! `cross-encoder/ms-marco-MiniLM-L-6-v2`, the exact model legacy loads
//! (`engine/cross_encoder.py:103,131`). fastembed ships no built-in entry for
//! it, so it goes through `try_new_from_user_defined`, which takes file bytes
//! rather than a repo id — hence the explicit download below.
//!
//! When this is enabled the cross-encoder's sigmoid-normalized logit replaces
//! `scoring::passthrough_base` as the base relevance signal; the three
//! multiplicative boosts (recency, temporal, proof) are untouched. When it is
//! disabled nothing in the recall path changes at all — see
//! `docs/design/ce-11-reranker.md` for why "disabled" is the parity
//! configuration rather than a reduced one.

use std::path::Path;
use std::sync::{Arc, Mutex};

use fastembed::{
    OnnxSource, RerankInitOptionsUserDefined, TextRerank, TokenizerFiles, UserDefinedRerankingModel,
};

use memgarden_core::config::{RERANK_TOP_K_WARN_ABOVE, RerankerConfig};

use crate::state::AppState;

/// The ONNX graph inside the Xenova repo. **Not** `model.onnx` at the root:
/// that path 404s in this repo, the export lives under `onnx/`. The four
/// tokenizer files below are at the root and all four are mandatory
/// (`fastembed/src/common.rs:32` — `TokenizerFiles` has no optional field).
const ONNX_FILE: &str = "onnx/model.onnx";
const TOKENIZER_FILE: &str = "tokenizer.json";
const CONFIG_FILE: &str = "config.json";
const SPECIAL_TOKENS_MAP_FILE: &str = "special_tokens_map.json";
const TOKENIZER_CONFIG_FILE: &str = "tokenizer_config.json";

/// Every config value that crosses into fastembed, resolved in one place.
///
/// This type exists because a `const` can be mutation-proof while the value
/// that actually reaches the wire is not: `threads` and `batch_size` are only
/// meaningful once they are inside `RerankInitOptionsUserDefined` and the
/// `batch_size` argument of `TextRerank::rerank`. Deriving them once and
/// asserting on *this* struct (and on the options struct it builds) tests the
/// value that arrives rather than the constant it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RerankArgs {
    pub intra_threads: usize,
    pub batch_size: usize,
}

impl RerankArgs {
    pub fn from_cfg(cfg: &RerankerConfig) -> RerankArgs {
        RerankArgs {
            intra_threads: cfg.threads,
            batch_size: cfg.batch_size,
        }
    }
}

/// The exact options handed to `TextRerank::try_new_from_user_defined`.
/// `max_length` is left at fastembed's 512, which is also the model's own
/// position-embedding limit — raising it would silently produce garbage.
fn init_options(args: &RerankArgs) -> RerankInitOptionsUserDefined {
    RerankInitOptionsUserDefined::new().with_intra_threads(args.intra_threads)
}

/// One loaded ONNX session behind a mutex, mirroring [`crate::embed::Embedder`]:
/// `TextRerank::rerank` takes `&mut self`.
///
/// // ponytail: one session, one mutex, same as the embedder. A concurrent
/// // recall waits out the ~15-26ms of a `top_k = 10` batch. Add a second
/// // session only if p99 says so — and note that a second session is also a
/// // second copy of the model in RAM.
pub struct Reranker {
    inner: Mutex<TextRerank>,
    args: RerankArgs,
}

impl Reranker {
    /// Downloads the five model files into `model_dir` on first run (cached
    /// afterwards, so subsequent boots are offline) and loads the session.
    /// Blocking: callers must run this in `spawn_blocking`.
    ///
    /// `model_dir` is `[embedding] model_dir`, deliberately shared rather than
    /// given its own knob — it is the daemon's one model cache, and hf-hub
    /// namespaces each repo under `models--<org>--<name>/` inside it.
    pub fn load(cfg: &RerankerConfig, model_dir: &Path) -> anyhow::Result<Self> {
        let args = RerankArgs::from_cfg(cfg);
        let repo = hf_hub::api::sync::ApiBuilder::new()
            .with_cache_dir(model_dir.to_path_buf())
            // The progress bar writes to stderr, which is noise in a daemon's
            // log stream (same call as `Embedder::load`).
            .with_progress(false)
            .build()?
            .model(cfg.model.clone());

        let onnx = repo.get(ONNX_FILE)?;
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: std::fs::read(repo.get(TOKENIZER_FILE)?)?,
            config_file: std::fs::read(repo.get(CONFIG_FILE)?)?,
            special_tokens_map_file: std::fs::read(repo.get(SPECIAL_TOKENS_MAP_FILE)?)?,
            tokenizer_config_file: std::fs::read(repo.get(TOKENIZER_CONFIG_FILE)?)?,
        };

        let model = TextRerank::try_new_from_user_defined(
            UserDefinedRerankingModel::new(OnnxSource::File(onnx), tokenizer_files),
            init_options(&args),
        )?;
        Ok(Reranker {
            inner: Mutex::new(model),
            args,
        })
    }

    /// Cross-encoder relevance for `docs` **in input order**, sigmoid-mapped
    /// to `[0, 1]`. Blocking: callers must run this in `spawn_blocking`.
    ///
    /// fastembed returns the results *sorted by score*, so the scores are
    /// scattered back by `RerankResult::index` rather than zipped — zipping
    /// would silently pair every score with the wrong document.
    pub fn scores(&self, query: &str, docs: &[String]) -> anyhow::Result<Vec<f64>> {
        if docs.is_empty() {
            return Ok(vec![]);
        }
        let mut out = vec![0.0f64; docs.len()];
        let ranked = {
            let mut model = self.inner.lock().expect("reranker mutex poisoned");
            model.rerank(query.to_string(), docs, false, Some(self.args.batch_size))?
        };
        for r in ranked {
            let slot = out.get_mut(r.index).ok_or_else(|| {
                anyhow::anyhow!(
                    "reranker returned index {} for {} docs",
                    r.index,
                    docs.len()
                )
            })?;
            *slot = sigmoid(f64::from(r.score));
        }
        Ok(out)
    }
}

/// Raw logit -> `[0, 1]` (`reranking.py:301-302`). Local cross-encoders emit
/// unbounded logits, and the multiplicative boosts downstream assume a base
/// in the same range the passthrough produced.
///
/// NaN maps to 0.0 rather than propagating (`reranking.py:318-324`, ported
/// because legacy hit it): a NaN base sorts unpredictably and serializes to
/// JSON `null`, which breaks a client expecting a number.
///
/// Saturates rather than overflows at the extremes — `(-x).exp()` is `inf`
/// for very negative `x`, giving exactly 0.0, and `0.0` for very positive
/// `x`, giving exactly 1.0.
pub fn sigmoid(logit: f64) -> f64 {
    if logit.is_nan() {
        return 0.0;
    }
    1.0 / (1.0 + (-logit).exp())
}

/// The string the cross-encoder actually scores, ported from
/// `reranking.py:272-286`. Two decorations, in this order:
///
/// 1. `context` is prefixed as `"{context}: {text}"` when present.
/// 2. `occurred_start` — **and only `occurred_start`**, not the recency
///    COALESCE — is prefixed as `"[Date: {%B %d, %Y} ({%Y-%m-%d})] "`. Both
///    styles are emitted because the model has seen both in training; legacy
///    says so explicitly at `:279`.
///
/// Legacy's comment at `:281` writes the example as "June 5, 2022", but
/// Python's `%d` is zero-padded, so it really produces "June 05, 2022".
/// jiff's `%d` is zero-padded too, so this matches byte-for-byte — the
/// comment is wrong, not the code, and this note is here so nobody "fixes"
/// it to `%-d` and diverges.
pub fn decorate(text: &str, context: Option<&str>, occurred_start: Option<i64>) -> String {
    let doc = match context.filter(|c| !c.is_empty()) {
        Some(context) => format!("{context}: {text}"),
        None => text.to_string(),
    };
    let Some(ms) = occurred_start else {
        return doc;
    };
    let Ok(ts) = jiff::Timestamp::from_millisecond(ms) else {
        // Untrusted timestamp: score the undecorated text rather than panic.
        return doc;
    };
    let zoned = ts.to_zoned(jiff::tz::TimeZone::UTC);
    let (Ok(readable), Ok(iso)) = (
        jiff::fmt::strtime::format("%B %d, %Y", &zoned),
        jiff::fmt::strtime::format("%Y-%m-%d", &zoned),
    ) else {
        return doc;
    };
    format!("[Date: {readable} ({iso})] {doc}")
}

/// The startup warning for a `top_k` deep enough to threaten AC-2, returned
/// rather than logged so it is testable. `None` when there is nothing to say.
pub fn top_k_warning(cfg: &RerankerConfig) -> Option<String> {
    (cfg.enabled && cfg.top_k > RERANK_TOP_K_WARN_ABOVE).then(|| {
        format!(
            "reranker.top_k = {} exceeds {RERANK_TOP_K_WARN_ABOVE}: the cross-encoder costs \
             ~1.5-2.6ms per candidate on CPU, so this budgets ~{:.0}-{:.0}ms of AC-2's 35ms p50 \
             for reranking alone",
            cfg.top_k,
            cfg.top_k as f64 * 1.5,
            cfg.top_k as f64 * 2.6,
        )
    })
}

/// Loads the cross-encoder and publishes it into `AppState.reranker`.
/// Spawned from main.rs *after* the listener binds, same reason as the
/// embedder: a first-run download must not delay the port bind.
///
/// A load failure is logged and leaves the slot `None`, which degrades recall
/// to the RRF passthrough — the configuration everything else in Phase B was
/// measured against. It is deliberately not fatal and deliberately not
/// reported by `/healthz`: an optional ranking refinement being absent is not
/// a degraded memory system.
pub async fn load_at_startup(state: AppState) {
    if !state.cfg.reranker.enabled {
        return;
    }
    let cfg = state.cfg.reranker.clone();
    let model_dir = state.cfg.embedding.model_dir.clone();
    match tokio::task::spawn_blocking(move || Reranker::load(&cfg, &model_dir)).await {
        Ok(Ok(reranker)) => {
            *state.reranker.write().expect("reranker lock poisoned") = Some(Arc::new(reranker));
            tracing::info!(
                model = %state.cfg.reranker.model,
                top_k = state.cfg.reranker.top_k,
                "cross-encoder reranker ready"
            );
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "reranker failed to load; recall stays on RRF passthrough")
        }
        Err(e) => tracing::error!(error = %e, "reranker load task panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RerankerConfig {
        memgarden_core::config::Config::defaults().unwrap().reranker
    }

    #[test]
    fn sigmoid_is_bounded_and_monotone() {
        // Bounds, including the saturating extremes an f32 logit can reach.
        for logit in [-1e9, -800.0, -10.0, -1.0, 0.0, 1.0, 10.0, 800.0, 1e9] {
            let s = sigmoid(logit);
            assert!((0.0..=1.0).contains(&s), "sigmoid({logit}) = {s}");
            assert!(s.is_finite(), "sigmoid({logit}) = {s}");
        }
        assert_eq!(sigmoid(0.0), 0.5);
        assert_eq!(sigmoid(-800.0), 0.0, "must saturate, not overflow");
        assert_eq!(sigmoid(800.0), 1.0);

        // Strictly increasing across the range where the model actually lives
        // — ms-marco logits are roughly [-12, +11].
        let mut previous = f64::NEG_INFINITY;
        for step in -120..=110 {
            let s = sigmoid(f64::from(step) / 10.0);
            assert!(s > previous, "not monotone at {step}: {s} <= {previous}");
            previous = s;
        }

        // NaN is sanitized, never propagated (legacy reranking.py:318-324).
        assert_eq!(sigmoid(f64::NAN), 0.0);
    }

    /// 2022-06-05T12:00:00Z — a single-digit day, which is the case where
    /// zero-padding is visible.
    const JUNE_5_2022: i64 = 1_654_430_400_000;

    #[test]
    fn decoration_matches_the_legacy_format() {
        assert_eq!(
            decorate("the daemon binds 9100", None, Some(JUNE_5_2022)),
            "[Date: June 05, 2022 (2022-06-05)] the daemon binds 9100"
        );
        // Context first, then the date wraps the whole thing (`:275-286`).
        assert_eq!(
            decorate("binds 9100", Some("memgardend"), Some(JUNE_5_2022)),
            "[Date: June 05, 2022 (2022-06-05)] memgardend: binds 9100"
        );
        assert_eq!(
            decorate("binds 9100", Some("memgardend"), None),
            "memgardend: binds 9100"
        );
        // No date at all: the text is scored bare. `occurred_start` ONLY —
        // legacy does not fall back to mentioned_at here, unlike
        // `scoring::effective_time`.
        assert_eq!(decorate("binds 9100", None, None), "binds 9100");
        assert_eq!(decorate("binds 9100", Some(""), None), "binds 9100");
        // A garbage timestamp degrades to the undecorated text.
        assert_eq!(decorate("x", None, Some(i64::MAX)), "x");
    }

    /// The values that reach fastembed, not the constants they came from.
    #[test]
    fn config_values_arrive_at_fastembed() {
        let mut c = cfg();
        c.threads = 7;
        c.batch_size = 3;
        let args = RerankArgs::from_cfg(&c);
        assert_eq!(args.batch_size, 3, "the argument passed to rerank()");
        assert_eq!(
            init_options(&args).intra_threads,
            Some(7),
            "the field ONNX Runtime reads"
        );
        // fastembed's default is all cores; leaving it unset would silently
        // ignore the config value, which is exactly the failure this asserts
        // against.
        assert_ne!(
            init_options(&args).intra_threads,
            RerankInitOptionsUserDefined::default().intra_threads
        );
        // 512 is the model's position-embedding limit, not a tunable.
        assert_eq!(init_options(&args).max_length, 512);
    }

    #[test]
    fn top_k_warning_fires_only_when_it_matters() {
        let mut c = cfg();
        assert_eq!(c.top_k, 10);
        assert_eq!(top_k_warning(&c), None, "disabled: nothing to warn about");

        c.top_k = 600; // legacy's thinking_budget * 2
        assert_eq!(top_k_warning(&c), None, "still disabled");

        c.enabled = true;
        c.top_k = RERANK_TOP_K_WARN_ABOVE;
        assert_eq!(top_k_warning(&c), None, "the bound itself is not over it");

        c.top_k = RERANK_TOP_K_WARN_ABOVE + 1;
        let msg = top_k_warning(&c).expect("must warn above the bound");
        assert!(msg.contains("2.6ms per candidate"), "{msg}");
        assert!(msg.contains("35ms p50"), "names the budget it threatens");
    }

    /// Requires the ~90MB model download and a real ONNX session — run
    /// manually:
    ///   cargo test -p memgardend -- --ignored --nocapture live_rerank
    ///
    /// Asserts the ordering claim (the on-topic document ranks first) and
    /// prints the per-candidate cost that CE-11's latency budget is stated
    /// in.
    #[test]
    #[ignore]
    fn live_rerank() {
        let cfg = cfg();
        let model_dir = memgarden_core::paths::models_dir().unwrap();
        let reranker = Reranker::load(&cfg, &model_dir).unwrap();

        let query = "how long is the retain job wall clock";
        let docs: Vec<String> = [
            "a banana is a good source of potassium",
            "the per-job retain wall clock is 7200 seconds",
            "the FTS5 tokenizer uses unicode61 with a prefix index",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let scores = reranker.scores(query, &docs).unwrap();
        println!("live_rerank: scores = {scores:?}");
        assert_eq!(scores.len(), docs.len());
        for s in &scores {
            assert!((0.0..=1.0).contains(s), "{s} outside [0,1]");
        }
        // Scores come back in INPUT order, so index 1 is the on-topic doc.
        let best = scores
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert_eq!(best, 1, "on-topic doc must rank first: {scores:?}");

        // Per-candidate cost, warm. This is the number the `top_k` default and
        // the startup warning are both stated in, so it is measured rather
        // than remembered.
        let batch: Vec<String> = (0..cfg.top_k)
            .map(|i| format!("{} (variant {i})", docs[i % docs.len()]))
            .collect();
        let _ = reranker.scores(query, &batch).unwrap(); // warm
        let started = std::time::Instant::now();
        let runs = 10;
        for _ in 0..runs {
            reranker.scores(query, &batch).unwrap();
        }
        let per_call = started.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
        println!(
            "live_rerank: top_k={} -> {per_call:.1}ms/call, {:.2}ms/candidate",
            cfg.top_k,
            per_call / cfg.top_k as f64
        );
    }
}
