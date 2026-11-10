//! Upload-related DTOs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Query parameters accepted on `POST /api/v1/files` (raw-body mode).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UploadQuery {
    pub name: Option<String>,
    pub tags: Option<String>,
    pub mime: Option<String>,
}

impl UploadQuery {
    /// Parses `tags=a,b,c` into a trimmed, order-preserving, deduplicated vector.
    pub fn tag_list(&self) -> Vec<String> {
        match &self.tags {
            Some(t) if !t.trim().is_empty() => {
                let mut out: Vec<String> = Vec::new();
                for tag in t.split(',') {
                    let tag = tag.trim().to_string();
                    if !tag.is_empty() && !out.contains(&tag) {
                        out.push(tag);
                    }
                }
                out
            }
            _ => Vec::new(),
        }
    }
}

/// Response body of a successful upload.
#[derive(Debug, Clone, Serialize)]
pub struct UploadResponse {
    pub file_id: Uuid,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub size_stored: u64,
    pub chunk_count: usize,
    /// BLAKE3 hex of the original content.
    pub content_hash: String,
    pub dedup_chunks: usize,
    pub dedup_bytes: u64,
    pub created_at_ms: i64,
}
