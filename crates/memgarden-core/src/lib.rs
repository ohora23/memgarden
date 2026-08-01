pub mod config;
pub mod error;
pub mod paths;
pub mod types;

pub use error::Error;

/// Embedding vector dimension (bge-small-en-v1.5 parity with legacy system).
pub const EMBEDDING_DIM: usize = 384;

/// Current unix time in milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as i64
}
