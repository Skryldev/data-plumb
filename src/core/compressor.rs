//! zstd compression with incompressible-data fallback.
//!
//! If compression does not shrink the payload (or the payload is below
//! `min_size`), the original bytes are stored verbatim with
//! `compressed = false`; decompression relies on the explicit flag instead of
//! magic-byte sniffing.

use bytes::Bytes;

use crate::error::StorageError;

pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

#[derive(Clone)]
pub struct Compressor {
    /// H-04: maximum legitimate plaintext size for one chunk. Decompression
    /// is clamped to this server-side bound — `size_original` from the index
    /// or a replication peer is UNTRUSTED.
    max_chunk_plain: u64,
    pub level: i32,
    pub min_size: usize,
}

/// Result of one compression attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressOutcome {
    pub data: Bytes,
    pub compressed: bool,
    pub size_original: u64,
    pub size_stored: u64,
}

impl Compressor {
    pub fn new(level: i32, min_size: usize) -> Self {
        Self::with_max_plain(level, min_size, 16 * 1024 * 1024)
    }

    /// Builds a compressor with the server-side decompression ceiling wired
    /// from `chunking.max` (H-04).
    pub fn with_max_plain(level: i32, min_size: usize, max_chunk_plain: u64) -> Self {
        Self {
            max_chunk_plain,
            level,
            min_size,
        }
    }

    pub fn compress(&self, data: &[u8]) -> CompressOutcome {
        let size_original = data.len() as u64;
        if data.len() < self.min_size {
            return CompressOutcome {
                data: Bytes::copy_from_slice(data),
                compressed: false,
                size_original,
                size_stored: size_original,
            };
        }
        match zstd::stream::encode_all(data, self.level) {
            Ok(c) if c.len() < data.len() => {
                let size_stored = c.len() as u64;
                CompressOutcome {
                    data: Bytes::from(c),
                    compressed: true,
                    size_original,
                    size_stored,
                }
            }
            _ => CompressOutcome {
                data: Bytes::copy_from_slice(data),
                compressed: false,
                size_original,
                size_stored: size_original,
            },
        }
    }

    /// Inverse of [`Compressor::compress`].
    ///
    /// H-04: `size_hint` is UNTRUSTED (redb row, replication peer, or future
    /// tooling). The allocation is clamped to `max_chunk_plain` — a chunk
    /// can never legitimately exceed `chunking.max` plaintext bytes — so a
    /// hostile `size_original` can neither panic (`u64::MAX` previously hit
    /// `Vec::with_capacity` "capacity overflow") nor attempt multi-GiB
    /// allocations. Any mismatch between the honest size and the hint is a
    /// Corruption error, never an allocation size.
    pub fn decompress(
        &self,
        data: &[u8],
        compressed: bool,
        size_hint: u64,
    ) -> Result<Bytes, StorageError> {
        if !compressed {
            if data.len() as u64 != size_hint {
                return Err(StorageError::Corruption(format!(
                    "raw chunk size {} != expected {size_hint}",
                    data.len()
                )));
            }
            return Ok(Bytes::copy_from_slice(data));
        }
        let cap = usize::try_from(size_hint.min(self.max_chunk_plain)).map_err(|_| {
            StorageError::Corruption("size_original out of range for this platform".to_string())
        })?;
        let out = zstd::bulk::decompress(data, cap)
            .map_err(|e| StorageError::Compression(format!("zstd decompress: {e}")))?;
        if out.len() as u64 != size_hint {
            return Err(StorageError::Corruption(format!(
                "decompressed {} bytes, expected {size_hint}",
                out.len()
            )));
        }
        Ok(Bytes::from(out))
    }

    /// True when the payload looks like a zstd frame (diagnostics only).
    pub fn looks_compressed(data: &[u8]) -> bool {
        data.len() >= 4 && data[0..4] == ZSTD_MAGIC
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compressor() -> Compressor {
        Compressor::new(3, 256)
    }

    #[test]
    fn compressible_roundtrip() {
        let c = compressor();
        let data = vec![b'a'; 50_000];
        let out = c.compress(&data);
        assert!(out.compressed);
        assert!(out.size_stored < 1_000);
        let back = c
            .decompress(&out.data, out.compressed, out.size_original)
            .unwrap();
        assert_eq!(back.as_ref(), data);
    }

    #[test]
    fn incompressible_falls_back_to_raw() {
        let c = compressor();
        let data: Vec<u8> = (0..4096u32).map(|i| ((i * 7919) % 256) as u8).collect();
        let out = c.compress(&data);
        if out.compressed {
            let back = c.decompress(&out.data, true, data.len() as u64).unwrap();
            assert_eq!(back.as_ref(), data);
        } else {
            assert_eq!(out.size_stored, data.len() as u64);
        }
    }

    #[test]
    fn tiny_payloads_stored_raw() {
        let c = compressor();
        let out = c.compress(b"small");
        assert!(!out.compressed);
        assert_eq!(out.data.as_ref(), b"small");
    }

    #[test]
    fn decompress_rejects_oversize_mismatch() {
        let c = compressor();
        let data = vec![b'x'; 10_000];
        let out = c.compress(&data);
        assert!(c
            .decompress(&out.data, true, (data.len() + 1) as u64)
            .is_err());
        assert!(c
            .decompress(&out.data, true, (data.len() - 1) as u64)
            .is_err());
    }

    #[test]
    fn corruption_is_detected() {
        let c = compressor();
        let data = vec![b'y'; 5_000];
        let out = c.compress(&data);
        // Truncation is always a hard zstd error. (Bit flips are covered by
        // the BLAKE3 verification on the retrieval path, because zstd frames
        // do not carry a checksum at this level.)
        let truncated = &out.data[..out.data.len() - 3];
        let r = c.decompress(truncated, true, data.len() as u64);
        assert!(r.is_err());
    }

    #[test]
    fn magic_detection() {
        assert!(!Compressor::looks_compressed(b"plain"));
        let c = compressor();
        let out = c.compress(&vec![b'q'; 5_000]);
        assert!(Compressor::looks_compressed(&out.data));
    }
}
