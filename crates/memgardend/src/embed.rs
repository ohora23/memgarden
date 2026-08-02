//! In-binary CPU embeddings: `bge-small-en-v1.5` via fastembed/ONNX,
//! bit-identical (measured to 7 decimals) to the legacy Python
//! sentence-transformers stack — see the plan's Verified Environment Facts.
//!
//! Every vector this module produces is stamped on disk with
//! [`memgarden_core::EMBEDDING_MODEL_ID`] (AX-1), which is what makes the
//! claim in the paragraph above checkable rather than remembered. The const
//! lives in `memgarden-core` only because `memgarden-store` writes the column;
//! this module is what it describes, and `mg1_reference_vector` below is the
//! check that says whether it still does.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use memgarden_core::EMBEDDING_DIM;
use memgarden_core::config::EmbeddingConfig;

/// One loaded ONNX session behind a mutex — `fastembed::TextEmbedding::embed`
/// takes `&mut self`, and `Arc<Mutex<TextEmbedding>>` is `Send + Sync`
/// (compile-verified in the plan's Verified Environment Facts).
pub struct Embedder {
    inner: Mutex<TextEmbedding>,
}

impl Embedder {
    /// Loads the model from `cfg.model_dir` (downloading into it on first
    /// run if missing — offline-friendly once cached). Blocking: callers
    /// must run this in `spawn_blocking`.
    pub fn load(cfg: &EmbeddingConfig) -> anyhow::Result<Self> {
        let options = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_cache_dir(cfg.model_dir.clone())
            .with_intra_threads(cfg.intra_threads)
            // The default download progress bar writes to stderr, which is
            // noise in a daemon's log stream.
            .with_show_download_progress(false);
        let model = TextEmbedding::try_new(options)?;
        Ok(Embedder {
            inner: Mutex::new(model),
        })
    }

    /// Embeds a batch of already-augmented strings (see
    /// `augment_for_embedding`), L2-normalizing every output vector
    /// (decision #3: fastembed already returns unit vectors — measured
    /// `‖v‖ = 1.000000` — but the 0.7/0.97 cosine thresholds used by B5/B7
    /// are meaningless without a *guaranteed* unit vector). Blocking:
    /// callers must run this in `spawn_blocking`.
    pub fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut model = self.inner.lock().expect("embedder mutex poisoned");
        let raw = model.embed(texts, None)?;
        raw.into_iter()
            .map(|v| {
                anyhow::ensure!(
                    v.len() == EMBEDDING_DIM,
                    "fastembed returned dim {} (expected {EMBEDDING_DIM})",
                    v.len()
                );
                Ok(normalize_l2(v))
            })
            .collect()
    }
}

fn normalize_l2(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

/// Ported verbatim from legacy `embedding_processing.py:15-46`: the string
/// actually fed to the embedder diverges from the stored `text` — augmenting
/// it with a human-readable date and a trailing entity list keeps MG-1's
/// imported vectors in the same embedding space as ours (plan decision #2).
/// `entities` is always empty until CE-7 (B5) adds entity resolution.
pub fn augment_for_embedding(
    text: &str,
    occurred_start: Option<i64>,
    occurred_end: Option<i64>,
    mentioned_at: Option<i64>,
    entities: &[String],
) -> String {
    let augmented = if let (Some(start), Some(end)) = (occurred_start, occurred_end) {
        // Range: both endpoints known.
        format!(
            "{text} (happened from {} to {})",
            format_month_year(start),
            format_month_year(end)
        )
    } else if let Some(date) = occurred_start.or(mentioned_at) {
        // Point: a single date is known — occurred_start, else mentioned_at
        // (memory_engine.py:3453,3472-3474).
        format!("{text} (happened in {})", format_month_year(date))
    } else {
        text.to_string()
    };

    if entities.is_empty() {
        augmented
    } else {
        format!("{augmented} [{}]", entities.join(", "))
    }
}

/// `unix_ms` -> `"%B %Y"` (month + year only), UTC. Falls back to the bare
/// timestamp string on a jiff error (out-of-range input) rather than
/// panicking on untrusted data.
fn format_month_year(unix_ms: i64) -> String {
    let Ok(ts) = jiff::Timestamp::from_millisecond(unix_ms) else {
        return unix_ms.to_string();
    };
    let zoned = ts.to_zoned(jiff::tz::TimeZone::UTC);
    jiff::fmt::strtime::format("%B %Y", &zoned).unwrap_or_else(|_| unix_ms.to_string())
}

/// Coarse embedding-subsystem status, reported by `/healthz` (decision #1:
/// DEGRADED on `Error`) and read/written via the atomic below — same
/// lock-free-static pattern as `memgarden_core::metrics::METRICS`.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedStatus {
    Loading = 0,
    Ready = 1,
    Disabled = 2,
    Error = 3,
}

impl EmbedStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EmbedStatus::Loading => "loading",
            EmbedStatus::Ready => "ready",
            EmbedStatus::Disabled => "disabled",
            EmbedStatus::Error => "error",
        }
    }

    fn from_u8(v: u8) -> EmbedStatus {
        match v {
            1 => EmbedStatus::Ready,
            2 => EmbedStatus::Disabled,
            3 => EmbedStatus::Error,
            _ => EmbedStatus::Loading,
        }
    }
}

static EMBED_STATUS: AtomicU8 = AtomicU8::new(EmbedStatus::Loading as u8);

pub fn embed_status() -> EmbedStatus {
    EmbedStatus::from_u8(EMBED_STATUS.load(Ordering::Relaxed))
}

pub fn set_embed_status(status: EmbedStatus) {
    EMBED_STATUS.store(status as u8, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn augment_no_date_is_bare_text() {
        assert_eq!(
            augment_for_embedding("hello", None, None, None, &[]),
            "hello"
        );
    }

    #[test]
    fn augment_point_from_occurred_start() {
        // 2025-08-15T00:00:00Z
        let ms = 1_755_216_000_000;
        assert_eq!(
            augment_for_embedding("met bob", Some(ms), None, None, &[]),
            "met bob (happened in August 2025)"
        );
    }

    #[test]
    fn augment_point_falls_back_to_mentioned_at() {
        let ms = 1_755_216_000_000;
        assert_eq!(
            augment_for_embedding("met bob", None, None, Some(ms), &[]),
            "met bob (happened in August 2025)"
        );
    }

    #[test]
    fn augment_range_uses_both_endpoints() {
        let start = 1_735_689_600_000; // 2025-01-01
        let end = 1_755_216_000_000; // 2025-08-15
        assert_eq!(
            augment_for_embedding("sprint", Some(start), Some(end), None, &[]),
            "sprint (happened from January 2025 to August 2025)"
        );
    }

    #[test]
    fn augment_appends_entities_suffix() {
        let entities = vec!["Alice".to_string(), "Bob".to_string()];
        assert_eq!(
            augment_for_embedding("met", None, None, None, &entities),
            "met [Alice, Bob]"
        );
    }

    #[test]
    fn embed_status_round_trips() {
        set_embed_status(EmbedStatus::Ready);
        assert_eq!(embed_status(), EmbedStatus::Ready);
        set_embed_status(EmbedStatus::Error);
        assert_eq!(embed_status(), EmbedStatus::Error);
        // Restore for other tests in this binary sharing the global static.
        set_embed_status(EmbedStatus::Loading);
    }

    /// Requires the 133MB model download — run manually:
    ///   cargo test -p memgardend -- --ignored model_smoke
    #[test]
    #[ignore]
    fn model_smoke() {
        let cfg = EmbeddingConfig {
            enabled: true,
            model_dir: memgarden_core::paths::models_dir().unwrap(),
            intra_threads: 4,
            batch_size: 8,
            backlog_poll_secs: 5,
            debug_endpoint: false,
        };
        let embedder = Embedder::load(&cfg).unwrap();
        let texts = vec![
            "the database migration completed successfully last night".to_string(),
            "the migration script finished running without errors".to_string(),
            "a banana is a good source of potassium".to_string(),
        ];
        let vectors = embedder.embed_batch(&texts).unwrap();
        assert_eq!(vectors.len(), 3);
        for v in &vectors {
            assert_eq!(v.len(), EMBEDDING_DIM);
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "‖v‖ = {norm}, expected ~1.0");
        }

        let cos = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        let sim_close = cos(&vectors[0], &vectors[1]);
        let sim_far = cos(&vectors[0], &vectors[2]);
        println!("cos(migration,migration2)={sim_close} cos(migration,banana)={sim_far}");
        assert!(
            sim_close > sim_far,
            "on-topic pair must score higher than the unrelated sentence: {sim_close} vs {sim_far}"
        );
    }

    /// **MG-1's reference vector** (AX-1). Prints what the active embedder
    /// produces for one fixed sentence, so that a Phase D run can embed the
    /// *same* sentence with the legacy sentence-transformers stack, compute
    /// the cosine against these numbers, and decide from data whether the
    /// legacy bank can be imported without re-embedding.
    ///
    /// Prints rather than asserts, deliberately. There is no committed
    /// expectation to compare against yet — producing one here would just be
    /// asserting that fastembed equals itself. The output is the artifact:
    /// paste it into the MG-1 PR next to the legacy side's numbers.
    ///
    /// The sentence is ASCII, punctuation-free, and passed **raw** — no
    /// `augment_for_embedding`, no BGE query/passage prefix (neither side uses
    /// one; the prefixes live only in legacy's unused `OnnxEmbeddings`) — so
    /// the two stacks are fed byte-identical input and the only variables left
    /// are pooling and normalization.
    ///
    /// Requires the 133MB model download — run manually:
    ///   cargo test -p memgardend -- --ignored --nocapture mg1_reference_vector
    #[test]
    #[ignore]
    fn mg1_reference_vector() {
        const REFERENCE_TEXT: &str = "the database migration completed successfully last night";

        let cfg = EmbeddingConfig {
            enabled: true,
            model_dir: memgarden_core::paths::models_dir().unwrap(),
            intra_threads: 4,
            batch_size: 8,
            backlog_poll_secs: 5,
            debug_endpoint: false,
        };
        let embedder = Embedder::load(&cfg).unwrap();
        let v = embedder
            .embed_batch(&[REFERENCE_TEXT.to_string()])
            .unwrap()
            .pop()
            .unwrap();

        assert_eq!(v.len(), EMBEDDING_DIM);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        println!("mg1: model_id  = {}", memgarden_core::EMBEDDING_MODEL_ID);
        println!("mg1: text      = {REFERENCE_TEXT:?}");
        println!("mg1: dim       = {}", v.len());
        println!("mg1: L2 norm   = {norm:.7}");
        println!(
            "mg1: dims[0..8]= {:?}",
            v[..8].iter().map(|x| format!("{x:.7}")).collect::<Vec<_>>()
        );
        // Not a tolerance for the legacy comparison — just proof that this
        // output describes a unit vector, so a cosine against it is a dot
        // product and MG-1 does not have to renormalize.
        assert!((norm - 1.0).abs() < 1e-4, "‖v‖ = {norm}, expected ~1.0");
    }
}
