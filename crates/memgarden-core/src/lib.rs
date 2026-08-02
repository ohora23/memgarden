pub mod config;
pub mod error;
pub mod metrics;
pub mod paths;
pub mod types;

pub use error::Error;

/// Embedding vector dimension (bge-small-en-v1.5 parity with legacy system).
pub const EMBEDDING_DIM: usize = 384;

/// Identifies the *producer* of a vector, stored on
/// `memory_nodes.embedding_model` and required to match before any cosine or
/// KNN comparison (AX-1). `NULL` means "unknown producer" — a legacy import
/// that arrived without a tag — and is excluded from dense comparison too.
///
/// Format is `<runtime>:<model>`, and the runtime half is the point: weights
/// alone do not determine a vector. `sentence-transformers` and `fastembed`
/// both serve `BAAI/bge-small-en-v1.5`, and whether they agree on pooling and
/// normalization is exactly the thing MG-1 (Phase D) has to verify before
/// importing the legacy bank without re-embedding. Tagging only the weights
/// would make the two indistinguishable and the check impossible after the
/// fact.
///
/// **Deliberately not versioned by crate version.** `fastembed = "=5.17.4"` is
/// pinned in the workspace, and folding that version in would invalidate every
/// stored vector on a routine dependency bump — a full re-embed as the price
/// of a patch release. Bump this string only when the *output* changes:
/// different weights, a different runtime, or a fastembed upgrade whose
/// release notes touch pooling/normalization. `embed::mg1_reference_vector`
/// is the check that tells you whether it did.
///
/// **A const, not config.** It has to describe what the code actually ran; a
/// value an operator can set is a value that can lie about the bytes on disk.
///
/// Lives here rather than in `memgardend::embed` (which owns the model) only
/// because `memgarden-store` writes the column and cannot depend on the
/// daemon crate — the same reason `EMBEDDING_DIM` is here.
pub const EMBEDDING_MODEL_ID: &str = "fastembed:BAAI/bge-small-en-v1.5";

/// Current unix time in milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as i64
}
