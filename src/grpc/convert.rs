//! Conversions between internal storage types and `file_storage.v1` protobuf
//! messages. All conversions are lossless in both directions so the same
//! data can cross either transport.

use prost_types::Timestamp;
use uuid::Uuid;

use crate::error::{ser, StorageError};
use crate::security::tenant::Tenant as InternalTenant;
use crate::storage::manifest::{ChunkRef as InternalChunkRef, Manifest as InternalManifest};

use super::generated::pb;

/// Epoch milliseconds → protobuf Timestamp.
pub fn ts_from_ms(ms: i64) -> Timestamp {
    Timestamp {
        seconds: ms.div_euclid(1000),
        nanos: (ms.rem_euclid(1000) * 1_000_000) as i32,
    }
}

/// protobuf Timestamp → epoch milliseconds.
pub fn ts_to_ms(ts: Option<Timestamp>) -> i64 {
    ts.map(|t| t.seconds * 1000 + (t.nanos / 1_000_000) as i64)
        .unwrap_or_default()
}

pub fn chunk_ref_to_proto(c: &InternalChunkRef) -> pb::ChunkRef {
    pb::ChunkRef {
        hash: c.hash.to_vec(),
        plain_hash: c.plain_hash.to_vec(),
        offset: c.offset,
        size_original: c.size_original,
        size_stored: c.size_stored,
        compressed: c.compressed,
        encrypted: c.encrypted,
    }
}

/// Parses a 32-byte block-store key arriving over the wire.
pub fn hash_from_bytes(bytes: &[u8], what: &str) -> Result<[u8; 32], StorageError> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| StorageError::Validation(format!("{what} must be 32 bytes")))?;
    Ok(arr)
}

/// Parses a UUID string arriving over the wire.
pub fn uuid_from_str(s: &str, what: &str) -> Result<Uuid, StorageError> {
    Uuid::parse_str(s.trim())
        .map_err(|e| StorageError::Validation(format!("invalid {what} '{s}': {e}")))
}

pub fn manifest_to_proto(m: &InternalManifest, include_chunks: bool) -> pb::Manifest {
    pb::Manifest {
        file_id: m.file_id.to_string(),
        tenant_id: m.tenant_id.to_string(),
        name: m.name.clone(),
        mime_type: m.mime.clone(),
        size: m.size_original,
        size_stored: m.size_stored,
        original_hash: m.original_hash.to_vec(),
        chunks: if include_chunks {
            m.chunks.iter().map(chunk_ref_to_proto).collect()
        } else {
            Vec::new()
        },
        tags: m.tags.clone(),
        created_at: Some(ts_from_ms(m.created_at_ms)),
        updated_at: Some(ts_from_ms(m.updated_at_ms)),
        deleted_at: m.deleted_at_ms.map(ts_from_ms),
        version: m.version,
        // M-04c: full-fidelity conversion. Only the read-side helper
        // `manifest_to_proto_capped` ever sets the truncation flag.
        chunks_truncated: false,
    }
}

/// M-04c: read-side manifest conversion with a hard cap on the chunk list.
/// A hostile/oversized manifest (100k chunks) must not OOM the handler that
/// serializes it; the cap matches the REST meta endpoint (`MAX_LISTED_CHUNKS`).
/// Replication and upload responses use the full-fidelity
/// [`manifest_to_proto`] so no data is ever lost on the wire that needs it.
pub fn manifest_to_proto_capped(
    m: &InternalManifest,
    include_chunks: bool,
    cap: usize,
) -> pb::Manifest {
    let mut proto = manifest_to_proto(m, include_chunks);
    if include_chunks && proto.chunks.len() > cap {
        proto.chunks.truncate(cap);
        proto.chunks_truncated = true;
    }
    proto
}

/// Rebuilds an internal manifest from its protobuf form. Used by the
/// replication service when a peer ships a manifest.
pub fn manifest_from_proto(p: &pb::Manifest) -> Result<InternalManifest, StorageError> {
    let mut chunks = Vec::with_capacity(p.chunks.len());
    for c in &p.chunks {
        chunks.push(InternalChunkRef {
            hash: hash_from_bytes(&c.hash, "chunk hash")?,
            plain_hash: hash_from_bytes(&c.plain_hash, "chunk plain_hash")?,
            offset: c.offset,
            size_original: c.size_original,
            size_stored: c.size_stored,
            compressed: c.compressed,
            encrypted: c.encrypted,
        });
    }
    Ok(InternalManifest {
        file_id: uuid_from_str(&p.file_id, "file_id")?,
        tenant_id: uuid_from_str(&p.tenant_id, "tenant_id")?,
        name: p.name.clone(),
        mime: p.mime_type.clone(),
        size_original: p.size,
        size_stored: p.size_stored,
        original_hash: hash_from_bytes(&p.original_hash, "original_hash")?,
        chunks,
        tags: p.tags.clone(),
        created_at_ms: ts_to_ms(p.created_at),
        updated_at_ms: ts_to_ms(p.updated_at),
        deleted_at_ms: p.deleted_at.map(|_| ts_to_ms(p.deleted_at)),
        version: p.version,
    })
}

pub fn tenant_to_proto(t: &InternalTenant) -> pb::Tenant {
    pb::Tenant {
        id: t.id.to_string(),
        name: t.name.clone(),
        quota_bytes: 0, // reserved; quotas are not enforced yet
        file_count: t.file_count,
        used_bytes: t.bytes_used,
        created_at: Some(ts_from_ms(t.created_at_ms)),
    }
}

/// Serialises an internal manifest the way the replication wire protocol
/// carries it (MessagePack), reusing the exact V1 codec.
pub fn manifest_msgpack(m: &InternalManifest) -> Result<Vec<u8>, StorageError> {
    rmp_serde::to_vec(m).map_err(ser)
}

pub fn manifest_from_msgpack(data: &[u8]) -> Result<InternalManifest, StorageError> {
    rmp_serde::from_slice(data).map_err(ser)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::manifest::ChunkRef;

    fn sample() -> InternalManifest {
        InternalManifest {
            file_id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            name: "report.pdf".into(),
            mime: "application/pdf".into(),
            size_original: 300,
            size_stored: 150,
            original_hash: [9u8; 32],
            chunks: vec![ChunkRef {
                hash: [1u8; 32],
                plain_hash: [2u8; 32],
                offset: 0,
                size_original: 300,
                size_stored: 150,
                compressed: true,
                encrypted: false,
            }],
            tags: vec!["q1".into()],
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_001_000,
            deleted_at_ms: None,
            version: 4,
        }
    }

    #[test]
    fn manifest_roundtrip() {
        let m = sample();
        let proto = manifest_to_proto(&m, true);
        let back = manifest_from_proto(&proto).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn msgpack_roundtrip_matches_v1_codec() {
        let m = sample();
        let bytes = manifest_msgpack(&m).unwrap();
        let back = manifest_from_msgpack(&bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn bad_hash_rejected() {
        let mut proto = manifest_to_proto(&sample(), true);
        proto.chunks[0].hash = vec![1, 2, 3];
        assert!(manifest_from_proto(&proto).is_err());
    }

    #[test]
    fn ts_conversion() {
        let ms = 1_700_000_123_456i64;
        let ts = ts_from_ms(ms);
        assert_eq!(ts.seconds, 1_700_000_123);
        assert_eq!(ts.nanos, 456_000_000);
        assert_eq!(ts_to_ms(Some(ts)), ms);
        assert_eq!(ts_to_ms(None), 0);
    }

    #[test]
    fn deleted_at_survives() {
        let mut m = sample();
        m.deleted_at_ms = Some(1_700_000_002_000);
        let back = manifest_from_proto(&manifest_to_proto(&m, false)).unwrap();
        assert_eq!(back.deleted_at_ms, m.deleted_at_ms);
    }
}
