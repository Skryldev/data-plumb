//! File / auth / tenant / misc DTOs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::manifest::Manifest;

/// Chunk summary inside a file listing.
#[derive(Debug, Clone, Serialize)]
pub struct ChunkDto {
    pub offset: u64,
    pub size_original: u64,
    pub size_stored: u64,
    pub hash: String,
    pub compressed: bool,
    pub encrypted: bool,
}

/// Full file metadata (`GET /files/:id/meta`).
#[derive(Debug, Clone, Serialize)]
pub struct FileMetaDto {
    pub file_id: Uuid,
    pub name: String,
    pub tenant_id: Uuid,
    pub mime: String,
    pub size: u64,
    pub size_stored: u64,
    pub content_hash: String,
    pub chunk_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<ChunkDto>>,
    pub chunks_truncated: bool,
    pub tags: Vec<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted: bool,
    pub version: u64,
    /// `files/<file_id>` — the API path for this object.
    pub href: String,
}

const MAX_LISTED_CHUNKS: usize = 1000;

impl FileMetaDto {
    pub fn from_manifest(m: &Manifest, include_chunks: bool) -> Self {
        let (chunks, truncated) = if include_chunks {
            (
                Some(
                    m.chunks
                        .iter()
                        .take(MAX_LISTED_CHUNKS)
                        .map(|c| ChunkDto {
                            offset: c.offset,
                            size_original: c.size_original,
                            size_stored: c.size_stored,
                            hash: hex::encode(c.hash),
                            compressed: c.compressed,
                            encrypted: c.encrypted,
                        })
                        .collect(),
                ),
                m.chunks.len() > MAX_LISTED_CHUNKS,
            )
        } else {
            (None, false)
        };
        Self {
            file_id: m.file_id,
            name: m.name.clone(),
            tenant_id: m.tenant_id,
            mime: m.mime.clone(),
            size: m.size_original,
            size_stored: m.size_stored,
            content_hash: hex::encode(m.original_hash),
            chunk_count: m.chunks.len(),
            chunks,
            chunks_truncated: truncated,
            tags: m.tags.clone(),
            created_at_ms: m.created_at_ms,
            updated_at_ms: m.updated_at_ms,
            deleted: m.is_deleted(),
            version: m.version,
            href: format!("files/{}", m.file_id),
        }
    }
}

/// Compact summary in list responses.
#[derive(Debug, Clone, Serialize)]
pub struct FileSummaryDto {
    pub file_id: Uuid,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub size_stored: u64,
    pub chunk_count: usize,
    pub tags: Vec<String>,
    pub created_at_ms: i64,
    pub deleted: bool,
}

impl From<&Manifest> for FileSummaryDto {
    fn from(m: &Manifest) -> Self {
        Self {
            file_id: m.file_id,
            name: m.name.clone(),
            mime: m.mime.clone(),
            size: m.size_original,
            size_stored: m.size_stored,
            chunk_count: m.chunks.len(),
            tags: m.tags.clone(),
            created_at_ms: m.created_at_ms,
            deleted: m.is_deleted(),
        }
    }
}

/// `GET /api/v1/files` query string.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub tag: Option<String>,
    pub name: Option<String>,
    pub include_deleted: Option<bool>,
}

/// `GET /api/v1/files` response.
#[derive(Debug, Serialize)]
pub struct FileListDto {
    pub total: u64,
    pub limit: usize,
    pub offset: usize,
    pub items: Vec<FileSummaryDto>,
}

// -- auth -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Serialize)]
pub struct TokenPairDto {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
}

// -- tenants ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateTenantRequest {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct TenantDto {
    pub id: Uuid,
    pub name: String,
    pub created_at_ms: i64,
    pub file_count: u64,
    pub bytes_used: u64,
}

// -- audit ------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AuditEntryDto {
    pub id: Uuid,
    pub ts_ms: i64,
    pub actor: String,
    pub tenant_id: Option<Uuid>,
    pub action: String,
    pub object: String,
    pub ip: String,
    pub status: String,
    pub detail: String,
}

// -- ops --------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct GcReportDto {
    pub manifests_purged: u64,
    pub chunks_removed: u64,
    pub orphans_removed: u64,
    pub bytes_reclaimed: u64,
}

#[derive(Debug, Serialize)]
pub struct ResyncDto {
    pub file_id: Uuid,
    pub replicated: bool,
}

#[derive(Debug, Serialize)]
pub struct HealthDto {
    pub status: &'static str,
    pub version: &'static str,
    pub uptime_secs: u64,
}

#[derive(Debug, Serialize)]
pub struct ReadyDto {
    pub status: &'static str,
    pub database: bool,
    pub storage: bool,
}
