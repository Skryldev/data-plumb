//! BLAKE3 hashing helpers.

use crate::error::StorageError;

/// One-shot hash.
pub fn hash_bytes(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

/// Hex encoding of a one-shot hash.
pub fn hash_hex(data: &[u8]) -> String {
    hex::encode(hash_bytes(data))
}

/// Streaming wrapper around `blake3::Hasher` for whole-file hashing.
pub struct StreamingHasher {
    inner: blake3::Hasher,
}

impl Default for StreamingHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingHasher {
    pub fn new() -> Self {
        Self {
            inner: blake3::Hasher::new(),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    pub fn finalize(&self) -> [u8; 32] {
        *self.inner.finalize().as_bytes()
    }
}

/// Verifies `data` against an expected hash, mapping failure to
/// [`StorageError::HashMismatch`].
pub fn verify(data: &[u8], expected: &[u8; 32], what: &str) -> Result<(), StorageError> {
    let actual = hash_bytes(data);
    if &actual == expected {
        Ok(())
    } else {
        Err(StorageError::HashMismatch {
            expected: format!("{what}:{}", hex::encode(expected)),
            actual: hex::encode(actual),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BLAKE3 official empty-string vector.
    const EMPTY: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    #[test]
    fn empty_vector() {
        assert_eq!(hex::encode(hash_bytes(b"")), EMPTY);
    }

    #[test]
    fn streaming_matches_oneshot() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut sh = StreamingHasher::new();
        for part in data.chunks(3333) {
            sh.update(part);
        }
        assert_eq!(sh.finalize(), hash_bytes(&data));
    }

    #[test]
    fn verify_ok_and_mismatch() {
        let h = hash_bytes(b"payload");
        assert!(verify(b"payload", &h, "chunk").is_ok());
        let err = verify(b"tampered", &h, "chunk").unwrap_err();
        assert!(matches!(err, StorageError::HashMismatch { .. }));
    }
}
