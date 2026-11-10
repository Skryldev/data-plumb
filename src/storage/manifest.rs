//! Manifest & chunk metadata structures.
//!
//! A `Manifest` is the authoritative description of a stored file: ordered
//! chunk references plus provenance and lifecycle fields. It is serialised
//! with MessagePack into the redb `files` table.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Reference to one chunk inside a file.
///
/// * `hash`        — block-store key: plaintext BLAKE3 hash, or
///   `BLAKE3(tenant_id || plain_hash)` when the blob is encrypted (keeps
///   encrypted dedup tenant-scoped).
/// * `plain_hash`  — BLAKE3 of the *plaintext* chunk; used for read-back
///   verification regardless of encryption.
/// * `offset`      — absolute offset of the chunk inside the original file.
/// * `compressed`  — blob is zstd (magic-based compression can be ambiguous
///   for tiny final chunks, so we store an explicit flag).
/// * `encrypted`   — blob is AES-256-GCM ciphertext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: [u8; 32],
    pub plain_hash: [u8; 32],
    pub offset: u64,
    pub size_original: u64,
    pub size_stored: u64,
    pub compressed: bool,
    pub encrypted: bool,
}

/// Per-chunk metadata kept in the `chunks` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkMeta {
    /// Number of live manifests referencing this chunk.
    pub refcount: u64,
    /// Byte size of the blob on disk (includes nonce+tag when encrypted).
    pub blob_size: u64,
    /// Creation timestamp (epoch ms).
    pub created_at_ms: i64,
    /// Relative path inside the block store (`ab/cd/<hex>.bin`).
    pub on_disk_path: String,
}

/// Soft-deletion timestamp of a file (epoch ms), if deleted.
pub type DeletedAt = Option<i64>;

/// Full file manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub file_id: Uuid,
    pub name: String,
    pub tenant_id: Uuid,
    pub mime: String,
    pub size_original: u64,
    pub size_stored: u64,
    /// BLAKE3 over the whole original (plaintext) file.
    pub original_hash: [u8; 32],
    /// Ordered chunk references (sorted by offset).
    pub chunks: Vec<ChunkRef>,
    pub tags: Vec<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted_at_ms: DeletedAt,
    /// Optimistic-concurrency version, bumped on every mutation.
    pub version: u64,
}

impl Manifest {
    /// Number of chunks (0 for empty files).
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// True when the file is soft-deleted.
    pub fn is_deleted(&self) -> bool {
        self.deleted_at_ms.is_some()
    }

    /// Chunks intersecting the half-open byte range `[start, end)`.
    pub fn chunks_in_range(&self, start: u64, end: u64) -> Vec<&ChunkRef> {
        self.chunks
            .iter()
            .filter(|c| c.offset < end && c.offset + c.size_original > start)
            .collect()
    }

    /// Sum of `size_stored` across chunks.
    pub fn recompute_stored_size(&mut self) {
        self.size_stored = self.chunks.iter().map(|c| c.size_stored).sum();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(offset: u64, size: u64) -> ChunkRef {
        ChunkRef {
            hash: [offset as u8; 32],
            plain_hash: [offset as u8; 32],
            offset,
            size_original: size,
            size_stored: size / 2,
            compressed: true,
            encrypted: false,
        }
    }

    #[test]
    fn msgpack_roundtrip() {
        let m = Manifest {
            file_id: Uuid::now_v7(),
            name: "report.pdf".into(),
            tenant_id: Uuid::now_v7(),
            mime: "application/pdf".into(),
            size_original: 100,
            size_stored: 60,
            original_hash: [7u8; 32],
            chunks: vec![chunk(0, 64), chunk(64, 36)],
            tags: vec!["q1".into(), "finance".into()],
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_001_000,
            deleted_at_ms: None,
            version: 3,
        };
        let bytes = rmp_serde::to_vec(&m).unwrap();
        let back: Manifest = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn chunks_in_range_selects_intersecting() {
        let chunks = vec![chunk(0, 100), chunk(100, 100), chunk(200, 100)];
        let m = Manifest {
            file_id: Uuid::now_v7(),
            name: "f".into(),
            tenant_id: Uuid::now_v7(),
            mime: "app".into(),
            size_original: 300,
            size_stored: 150,
            original_hash: [0; 32],
            chunks: chunks.clone(),
            tags: vec![],
            created_at_ms: 0,
            updated_at_ms: 0,
            deleted_at_ms: None,
            version: 1,
        };
        assert_eq!(m.chunks_in_range(0, 300).len(), 3);
        assert_eq!(m.chunks_in_range(50, 150).len(), 2);
        assert_eq!(m.chunks_in_range(150, 160).len(), 1);
        assert_eq!(m.chunks_in_range(299, 300).len(), 1);
        assert_eq!(m.chunks_in_range(300, 400).len(), 0);
    }

    #[test]
    fn recompute_stored_size() {
        let mut m = Manifest {
            file_id: Uuid::now_v7(),
            name: "f".into(),
            tenant_id: Uuid::now_v7(),
            mime: "app".into(),
            size_original: 300,
            size_stored: 0,
            original_hash: [0; 32],
            chunks: vec![chunk(0, 100), chunk(100, 200)],
            tags: vec![],
            created_at_ms: 0,
            updated_at_ms: 0,
            deleted_at_ms: None,
            version: 1,
        };
        m.recompute_stored_size();
        assert_eq!(m.size_stored, 150);
    }
}
