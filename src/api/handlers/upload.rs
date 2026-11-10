//! `POST /api/v1/files` — multipart or raw-body upload.

use axum::body::Body;
use axum::extract::{FromRequest, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Extension;
use axum::Json;
use futures::StreamExt;
use tokio::io::AsyncWriteExt as _;

use crate::api::dto::{UploadQuery, UploadResponse};
use crate::api::middleware::tenant::check_write;
use crate::error::StorageError;
use crate::security::auth::AuthContext;
use crate::state::AppState;

const MULTIPART: &str = "multipart/form-data";
const OCTET_STREAM: &str = "application/octet-stream";
const MAX_META_FIELD: usize = 4096;

pub async fn upload_file(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): axum::extract::Query<UploadQuery>,
    headers: HeaderMap,
    request: Request<Body>,
) -> Result<impl IntoResponse, StorageError> {
    check_write(&ctx)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Early 413 when Content-Length is present and over the cap.
    if let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        state.0.validator.validate_size(len)?;
    }

    if content_type.starts_with(MULTIPART) {
        upload_multipart(state, ctx, request).await
    } else {
        upload_raw(state, ctx, q, content_type, request).await
    }
}
fn map_body_err(e: axum::Error) -> StorageError {
    StorageError::Internal(format!("body read: {e}"))
}

async fn upload_raw(
    state: AppState,
    ctx: AuthContext,
    q: UploadQuery,
    declared_ct: String,
    request: Request<Body>,
) -> Result<axum::response::Response, StorageError> {
    let name = q
        .name
        .clone()
        .or_else(|| default_name(&declared_ct))
        .unwrap_or_else(|| "untitled".to_string());
    let mime = q.mime.clone().unwrap_or_else(|| {
        let base = declared_ct.split(';').next().unwrap_or("").trim();
        if base.is_empty() {
            OCTET_STREAM.to_string()
        } else {
            base.to_string()
        }
    });
    let tags = q.tag_list();
    let stream = request
        .into_body()
        .into_data_stream()
        .map(|r| r.map_err(map_body_err));
    let outcome = state
        .0
        .pipeline
        .process(ctx.tenant_id, &name, &mime, tags, stream)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(UploadResponse::from_outcome(&ctx, outcome)),
    )
        .into_response())
}

async fn upload_multipart(
    state: AppState,
    ctx: AuthContext,
    request: Request<Body>,
) -> Result<axum::response::Response, StorageError> {
    use axum::extract::multipart::Multipart;

    let mut multipart = Multipart::from_request(request, &state)
        .await
        .map_err(|e| StorageError::Validation(e.body_text()))?;

    let mut name: Option<String> = None;
    let mut tags: Option<String> = None;
    let mut mime: Option<String> = None;
    // The file field is spooled to a temp file so the multipart reader can
    // advance to later metadata fields without holding a borrow.
    let mut spooled: Option<(tempfile::NamedTempFile, u64)> = None;
    let mut file_mime: Option<String> = None;

    while let Some(field) = multipart.next_field().await? {
        match field.name().unwrap_or("") {
            "name" => {
                let v = field.text().await?;
                if v.len() > MAX_META_FIELD {
                    return Err(StorageError::Validation("name field too long".into()));
                }
                let v = v.trim().to_string();
                if !v.is_empty() {
                    name = Some(v);
                }
            }
            "tags" => {
                let v = field.text().await?;
                if v.len() > MAX_META_FIELD {
                    return Err(StorageError::Validation("tags field too long".into()));
                }
                tags = Some(v);
            }
            "mime" | "content_type" => {
                let v = field.text().await?;
                let v = v.trim().to_string();
                if !v.is_empty() {
                    mime = Some(v);
                }
            }
            "file" => {
                file_mime = field.content_type().map(|s| s.to_string());
                let (tmp, size) = spool_field(&state, field).await?;
                spooled = Some((tmp, size));
            }
            _ => {
                // Unknown fields are consumed and ignored.
                let _ = field.text().await;
            }
        }
    }

    let (tmp, spooled_size) = spooled.ok_or_else(|| {
        StorageError::Validation("multipart body must contain a 'file' field".into())
    })?;
    state.0.validator.validate_size(spooled_size)?;
    let stream = Box::pin(spooled_file_stream(tmp));
    let name = name.unwrap_or_else(|| "untitled".to_string());
    let mime = mime
        .or(file_mime)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| OCTET_STREAM.to_string());
    let tag_list = match tags {
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
    };

    let outcome = state
        .0
        .pipeline
        .process(ctx.tenant_id, &name, &mime, tag_list, stream)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(UploadResponse::from_outcome(&ctx, outcome)),
    )
        .into_response())
}

/// Streams one multipart field into a temp file (memory-bounded spool).
/// Returns the temp file and the byte count.
async fn spool_field(
    state: &AppState,
    mut field: axum::extract::multipart::Field<'_>,
) -> Result<(tempfile::NamedTempFile, u64), StorageError> {
    let tmp = tempfile::NamedTempFile::new_in(&state.0.cfg.storage.temp_dir)
        .map_err(StorageError::from)?;
    let mut written: u64 = 0;
    {
        let mut out = tokio::fs::File::create(tmp.path())
            .await
            .map_err(StorageError::from)?;
        while let Some(chunk) = field.chunk().await.map_err(StorageError::from)? {
            written += chunk.len() as u64;
            state.0.validator.validate_size(written)?;
            out.write_all(&chunk).await.map_err(StorageError::from)?;
        }
        out.flush().await.map_err(StorageError::from)?;
        out.sync_all().await.map_err(StorageError::from)?;
    }
    Ok((tmp, written))
}

/// Re-opens a spooled file as a byte stream for the pipeline.
fn spooled_file_stream(
    tmp: tempfile::NamedTempFile,
) -> impl futures::Stream<Item = Result<bytes::Bytes, StorageError>> + Send {
    async_stream::try_stream! {
        let path = tmp
            .into_temp_path()
            .keep()
            .map_err(|e| StorageError::Internal(format!("temp file persist: {e}")))?;
        use tokio::io::AsyncReadExt as _;
        let mut file = tokio::fs::File::open(&path).await.map_err(StorageError::from)?;
        let mut buf = vec![0u8; 128 * 1024];
        loop {
            let n = file.read(&mut buf).await.map_err(StorageError::from)?;
            if n == 0 {
                break;
            }
            yield bytes::Bytes::copy_from_slice(&buf[..n]);
        }
        let _ = tokio::fs::remove_file(&path).await;
    }
}

fn default_name(content_type: &str) -> Option<String> {
    (!content_type.is_empty()).then(|| format!("upload-{}", uuid::Uuid::now_v7().simple()))
}

impl UploadResponse {
    fn from_outcome(ctx: &AuthContext, o: crate::core::pipeline::UploadOutcome) -> Self {
        let m = &o.manifest;
        let _ = ctx; // tenant scoping already enforced
        Self {
            file_id: m.file_id,
            name: m.name.clone(),
            mime: m.mime.clone(),
            size: m.size_original,
            size_stored: m.size_stored,
            chunk_count: m.chunks.len(),
            content_hash: hex::encode(m.original_hash),
            dedup_chunks: o.dedup_chunks,
            dedup_bytes: o.dedup_bytes,
            created_at_ms: m.created_at_ms,
        }
    }
}
