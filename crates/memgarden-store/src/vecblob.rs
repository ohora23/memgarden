//! Encodes embedding vectors as little-endian `f32` BLOBs — the format
//! stored in `memory_nodes.embedding` and fed to sqlite-vec's `vec0` table.

use memgarden_core::EMBEDDING_DIM;
use memgarden_core::error::{Error, Result};

/// Encodes `embedding` as a little-endian `f32` BLOB. Errors if
/// `embedding.len() != EMBEDDING_DIM`.
pub fn encode(embedding: &[f32]) -> Result<Vec<u8>> {
    if embedding.len() != EMBEDDING_DIM {
        return Err(Error::Invalid(format!(
            "embedding dimension mismatch: expected {EMBEDDING_DIM}, got {}",
            embedding.len()
        )));
    }
    Ok(embedding.iter().flat_map(|f| f.to_le_bytes()).collect())
}

/// Decodes a little-endian `f32` BLOB back into an embedding vector. Errors
/// if `blob.len() != EMBEDDING_DIM * 4`.
pub fn decode(blob: &[u8]) -> Result<Vec<f32>> {
    let expected = EMBEDDING_DIM * 4;
    if blob.len() != expected {
        return Err(Error::Invalid(format!(
            "embedding blob size mismatch: expected {expected} bytes, got {}",
            blob.len()
        )));
    }
    Ok(blob
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let v: Vec<f32> = (0..EMBEDDING_DIM).map(|i| i as f32 * 0.5).collect();
        let blob = encode(&v).unwrap();
        assert_eq!(blob.len(), EMBEDDING_DIM * 4);
        assert_eq!(decode(&blob).unwrap(), v);
    }

    #[test]
    fn wrong_dim_is_err() {
        assert!(encode(&[1.0, 2.0]).is_err());
        assert!(decode(&[0u8; 8]).is_err());
    }
}
