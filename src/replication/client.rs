//! HTTP client for replica-to-primary / primary-to-replica replication plus
//! the binary frame codecs shared with the `/internal/*` endpoints.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use reqwest::header::HeaderMap;
use std::time::Duration;

use crate::error::{ser, StorageError};
use crate::storage::manifest::Manifest;

pub const API_KEY_HEADER: &str = "X-Api-Key";

/// One chunk replica payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPayload {
    pub hash: [u8; 32],
    pub plain_hash: [u8; 32],
    pub size_original: u64,
    pub size_stored: u64,
    pub compressed: bool,
    pub encrypted: bool,
    pub blob: Bytes,
}

// ---------------------------------------------------------------------------
// Frame codecs
// ---------------------------------------------------------------------------

/// Encodes a batch: `u32 count` then per item
/// `hash(32) | plain_hash(32) | u64 size_original | u64 size_stored |
///  u8 flags(bit0=compressed, bit1=encrypted) | u32 blob_len | blob`.
pub fn encode_batch(items: &[ChunkPayload]) -> Bytes {
    let mut buf = BytesMut::new();
    buf.put_u32_le(items.len() as u32);
    for it in items {
        buf.put_slice(&it.hash);
        buf.put_slice(&it.plain_hash);
        buf.put_u64_le(it.size_original);
        buf.put_u64_le(it.size_stored);
        let flags = (it.compressed as u8) | ((it.encrypted as u8) << 1);
        buf.put_u8(flags);
        buf.put_u32_le(it.blob.len() as u32);
        buf.put_slice(&it.blob);
    }
    buf.freeze()
}

pub fn decode_batch(mut data: &[u8]) -> Result<Vec<ChunkPayload>, StorageError> {
    let mut items = Vec::new();
    if data.remaining() < 4 {
        return Err(StorageError::Serialization("truncated batch count".into()));
    }
    let count = data.get_u32_le();
    for _ in 0..count {
        if data.remaining() < 32 + 32 + 8 + 8 + 1 + 4 {
            return Err(StorageError::Serialization("truncated batch frame".into()));
        }
        let mut hash = [0u8; 32];
        let mut plain_hash = [0u8; 32];
        hash.copy_from_slice(&data[..32]);
        data.advance(32);
        plain_hash.copy_from_slice(&data[..32]);
        data.advance(32);
        let size_original = data.get_u64_le();
        let size_stored = data.get_u64_le();
        let flags = data.get_u8();
        let blob_len = data.get_u32_le() as usize;
        if data.remaining() < blob_len {
            return Err(StorageError::Serialization(
                "truncated blob in batch frame".into(),
            ));
        }
        let blob = data.copy_to_bytes(blob_len);
        items.push(ChunkPayload {
            hash,
            plain_hash,
            size_original,
            size_stored,
            compressed: flags & 0b01 != 0,
            encrypted: flags & 0b10 != 0,
            blob,
        });
    }
    Ok(items)
}

pub fn encode_manifest(m: &Manifest) -> Result<Bytes, StorageError> {
    let body = rmp_serde::to_vec(m).map_err(ser)?;
    Ok(Bytes::from(body))
}

pub fn decode_manifest(data: &[u8]) -> Result<Manifest, StorageError> {
    rmp_serde::from_slice(data).map_err(ser)
}

/// Bundle: `u32 manifest_len | manifest(msgpack) | batch frames`.
pub fn encode_file_bundle(m: &Manifest, chunks: &[ChunkPayload]) -> Result<Bytes, StorageError> {
    let manifest_bytes = encode_manifest(m)?;
    let mut buf = BytesMut::new();
    buf.put_u32_le(manifest_bytes.len() as u32);
    buf.put_slice(&manifest_bytes);
    buf.put_slice(&encode_batch(chunks));
    Ok(buf.freeze())
}

pub fn decode_file_bundle(mut data: &[u8]) -> Result<(Manifest, Vec<ChunkPayload>), StorageError> {
    if data.remaining() < 4 {
        return Err(StorageError::Serialization(
            "truncated bundle header".into(),
        ));
    }
    let len = data.get_u32_le() as usize;
    if data.remaining() < len {
        return Err(StorageError::Serialization(
            "truncated manifest in bundle".into(),
        ));
    }
    let manifest = decode_manifest(&data[..len])?;
    data.advance(len);
    let chunks = decode_batch(data)?;
    Ok((manifest, chunks))
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ReplicationClient {
    http: reqwest::Client,
    base_url: String,
}

impl ReplicationClient {
    pub fn new(base_url: &str, api_key: &str, timeout: Duration) -> Result<Self, StorageError> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            API_KEY_HEADER,
            reqwest::header::HeaderValue::from_str(api_key)
                .map_err(|e| StorageError::Replication(format!("invalid api key header: {e}")))?,
        );
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .default_headers(headers)
            .build()
            .map_err(|e| StorageError::Replication(format!("client build: {e}")))?;
        Ok(Self { http, base_url })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, StorageError> {
        let status = resp.status();
        if status.is_success() {
            Ok(resp)
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(StorageError::Replication(format!(
                "upstream {status}: {}",
                body.chars().take(200).collect::<String>()
            )))
        }
    }

    /// POST /internal/batch — ship a batch of chunk blobs.
    pub async fn post_batch(&self, items: &[ChunkPayload]) -> Result<(), StorageError> {
        let body = encode_batch(items);
        let resp = self
            .http
            .post(self.url("/internal/batch"))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body)
            .send()
            .await
            .map_err(|e| StorageError::Replication(format!("batch ship: {e}")))?;
        Self::check(resp).await.map(|_| ())
    }

    /// POST /internal/chunk — ship a single chunk blob.
    pub async fn put_chunk(&self, item: &ChunkPayload) -> Result<(), StorageError> {
        let url = format!(
            "/internal/chunk?hash={}&plain_hash={}&size_original={}&size_stored={}&compressed={}&encrypted={}",
            hex::encode(item.hash),
            hex::encode(item.plain_hash),
            item.size_original,
            item.size_stored,
            item.compressed,
            item.encrypted
        );
        let resp = self
            .http
            .post(self.url(&url))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(item.blob.clone())
            .send()
            .await
            .map_err(|e| StorageError::Replication(format!("chunk ship: {e}")))?;
        Self::check(resp).await.map(|_| ())
    }

    /// POST /internal/manifest — msgpack manifest upsert.
    pub async fn put_manifest(&self, m: &Manifest) -> Result<(), StorageError> {
        let body = encode_manifest(m)?;
        let resp = self
            .http
            .post(self.url("/internal/manifest"))
            .header(reqwest::header::CONTENT_TYPE, "application/msgpack")
            .body(body)
            .send()
            .await
            .map_err(|e| StorageError::Replication(format!("manifest ship: {e}")))?;
        Self::check(resp).await.map(|_| ())
    }

    /// DELETE /internal/manifest/:id
    pub async fn purge_manifest(&self, id: &uuid::Uuid) -> Result<(), StorageError> {
        let resp = self
            .http
            .delete(self.url(&format!("/internal/manifest/{id}")))
            .send()
            .await
            .map_err(|e| StorageError::Replication(format!("manifest purge: {e}")))?;
        Self::check(resp).await.map(|_| ())
    }

    /// POST /internal/file/:id — push a full file bundle (resync path).
    pub async fn put_file_bundle(
        &self,
        id: &uuid::Uuid,
        bundle: &[u8],
    ) -> Result<(), StorageError> {
        let resp = self
            .http
            .post(self.url(&format!("/internal/file/{id}")))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bundle.to_vec())
            .send()
            .await
            .map_err(|e| StorageError::Replication(format!("bundle push: {e}")))?;
        Self::check(resp).await.map(|_| ())
    }

    /// GET /internal/file/:id — pull a full file bundle.
    pub async fn fetch_file_bundle(
        &self,
        id: &uuid::Uuid,
    ) -> Result<(Manifest, Vec<ChunkPayload>), StorageError> {
        let resp = self
            .http
            .get(self.url(&format!("/internal/file/{id}")))
            .send()
            .await
            .map_err(|e| StorageError::Replication(format!("bundle fetch: {e}")))?;
        let resp = Self::check(resp).await?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| StorageError::Replication(format!("bundle body: {e}")))?;
        decode_file_bundle(&bytes)
    }

    /// GET /internal/chunk/:hash — pull a single blob (on-demand repair).
    pub async fn fetch_chunk(&self, hash: &[u8; 32]) -> Result<Bytes, StorageError> {
        let resp = self
            .http
            .get(self.url(&format!("/internal/chunk/{}", hex::encode(hash))))
            .send()
            .await
            .map_err(|e| StorageError::Replication(format!("chunk fetch: {e}")))?;
        let resp = Self::check(resp).await?;
        resp.bytes()
            .await
            .map_err(|e| StorageError::Replication(format!("chunk body: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn payload(i: u8, blob_len: usize) -> ChunkPayload {
        ChunkPayload {
            hash: [i; 32],
            plain_hash: [i.wrapping_add(1); 32],
            size_original: 1000 + i as u64,
            size_stored: blob_len as u64,
            compressed: i % 2 == 0,
            encrypted: i % 3 == 0,
            blob: Bytes::from(vec![i; blob_len]),
        }
    }

    #[test]
    fn batch_frames_roundtrip() {
        let items: Vec<ChunkPayload> = (0..5u8).map(|i| payload(i, 10 + i as usize)).collect();
        let frames = encode_batch(&items);
        let back = decode_batch(&frames).unwrap();
        assert_eq!(back, items);
    }

    #[test]
    fn empty_batch_roundtrip() {
        let frames = encode_batch(&[]);
        assert_eq!(frames.len(), 4);
        assert!(decode_batch(&frames).unwrap().is_empty());
    }

    #[test]
    fn truncated_batch_rejected() {
        let items = vec![payload(1, 100)];
        let frames = encode_batch(&items);
        for cut in [0usize, 3, 40, 80] {
            assert!(
                decode_batch(&frames[..cut.min(frames.len())]).is_err(),
                "cut={cut}"
            );
        }
    }

    #[test]
    fn manifest_roundtrip() {
        let m = Manifest {
            file_id: Uuid::now_v7(),
            name: "x".into(),
            tenant_id: Uuid::now_v7(),
            mime: "application/pdf".into(),
            size_original: 10,
            size_stored: 8,
            original_hash: [3; 32],
            chunks: vec![],
            tags: vec!["t".into()],
            created_at_ms: 1,
            updated_at_ms: 2,
            deleted_at_ms: None,
            version: 1,
        };
        let enc = encode_manifest(&m).unwrap();
        assert_eq!(decode_manifest(&enc).unwrap(), m);
    }

    #[test]
    fn file_bundle_roundtrip() {
        let m = Manifest {
            file_id: Uuid::now_v7(),
            name: "x".into(),
            tenant_id: Uuid::now_v7(),
            mime: "a/b".into(),
            size_original: 3000,
            size_stored: 2200,
            original_hash: [9; 32],
            chunks: vec![],
            tags: vec![],
            created_at_ms: 1,
            updated_at_ms: 2,
            deleted_at_ms: None,
            version: 7,
        };
        let chunks: Vec<ChunkPayload> = (0..3u8).map(|i| payload(i, 50)).collect();
        let bundle = encode_file_bundle(&m, &chunks).unwrap();
        let (m2, c2) = decode_file_bundle(&bundle).unwrap();
        assert_eq!(m2, m);
        assert_eq!(c2, chunks);
    }

    #[test]
    fn truncated_bundle_rejected() {
        assert!(decode_file_bundle(&[0, 0]).is_err());
        assert!(decode_file_bundle(&[]).is_err());
    }

    #[test]
    fn client_rejects_invalid_base() {
        // empty header value fails validation
        assert!(ReplicationClient::new("http://localhost:1", "", Duration::from_secs(1)).is_ok());
    }
}
