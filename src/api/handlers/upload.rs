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
use crate::core::validation::{validate_expected_size, validate_upload, UploadRequestView};
use crate::error::StorageError;
use crate::security::auth::AuthContext;
use crate::state::AppState;

const MULTIPART: &str = "multipart/form-data";
const OCTET_STREAM: &str = "application/octet-stream";
const MAX_META_FIELD: usize = 4 * 1024;
/// H-02: unknown fields are drained (never buffered) under this cap.
const MAX_UNKNOWN_FIELD: usize = 4 * 1024;
/// H-02: total metadata budget is inherently bounded (fields x cap).
const MAX_META_FIELDS: usize = 32;

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

    // M-04e: the declared-size cap runs BEFORE admission control so an
    // over-cap declaration is a clean 400 (parity with the gRPC
    // INVALID_ARGUMENT), not a budget 413. The same shared code gRPC uses
    // via validate_upload enforces it there.
    validate_expected_size(q.expected_size, &state.0.cfg.limits)?;

    // H-01: admission control. Known-size requests are admitted or rejected
    // BEFORE any byte streams (413 budget / 429 concurrency). The lease is
    // held for the whole upload and released on drop (success, error, panic).
    let declared_len = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let lease = match declared_len {
        Some(len) => state
            .0
            .upload_gate
            .try_reserve_known(len)
            .map_err(gate_error)?,
        None => state
            .0
            .upload_gate
            .try_reserve_unknown()
            .map_err(gate_error)?,
    };
    let lease = lease;

    let result = if content_type.starts_with(MULTIPART) {
        upload_multipart(state, ctx, q, request).await
    } else {
        upload_raw(state, ctx, q, content_type, request).await
    };
    drop(lease);
    result
}

/// H-01: maps gate refusals onto the transport error surface.
fn gate_error(e: crate::core::gate::GateError) -> StorageError {
    use crate::core::gate::GateError;
    match e {
        GateError::BudgetExhausted => StorageError::TooLarge(
            "upload admission control: server upload byte budget exhausted".into(),
        ),
        GateError::TooManyUploads => {
            StorageError::Unavailable("too many concurrent uploads".into())
        }
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
        .map(|n| n.trim().to_string())
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

    // M-04: the shared validation layer — the ONLY place request-shape
    // rules live. Handlers extract, core validates.
    let expected_size = (q.expected_size.unwrap_or(0) > 0).then_some(q.expected_size.unwrap_or(0));
    let view = UploadRequestView {
        tenant: ctx.tenant_id,
        name: &name,
        mime: &mime,
        tags: &tags,
        expected_size,
        include_deleted: false,
        role: ctx.role,
    };
    validate_upload(&view, &state.0.cfg.limits)?;

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
    q: UploadQuery,
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

    // H-02: metadata fields are read with a hard per-field cap and the
    // field count is bounded — allocation never exceeds the cap, no matter
    // what the client streams (the old code buffered the WHOLE field via
    // `.text()` before the length check; unknown fields had no cap at all).
    let mut field_count: usize = 0;
    while let Some(mut field) = multipart.next_field().await? {
        field_count += 1;
        if field_count > MAX_META_FIELDS {
            return Err(StorageError::TooLarge(format!(
                "multipart metadata fields exceed the {MAX_META_FIELDS}-field cap"
            )));
        }
        match field.name().unwrap_or("") {
            "name" => {
                let v = read_bounded_field(&mut field, MAX_META_FIELD).await?;
                let v = String::from_utf8_lossy(&v).trim().to_string();
                if !v.is_empty() {
                    name = Some(v);
                }
            }
            "tags" => {
                let v = read_bounded_field(&mut field, MAX_META_FIELD).await?;
                tags = Some(String::from_utf8_lossy(&v).to_string());
            }
            "mime" | "content_type" => {
                let v = read_bounded_field(&mut field, MAX_META_FIELD).await?;
                let v = String::from_utf8_lossy(&v).trim().to_string();
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
                // Unknown fields: consume-and-discard under a hard cap —
                // never buffered into a String.
                drain_bounded(&mut field, MAX_UNKNOWN_FIELD).await?;
            }
        }
    }

    let (tmp, spooled_size) = spooled.ok_or_else(|| {
        StorageError::Validation("multipart body must contain a 'file' field".into())
    })?;
    state.0.validator.validate_size(spooled_size)?;

    let name = name.unwrap_or_else(|| {
        q.name
            .clone()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "untitled".to_string())
    });
    let mime = mime
        .or(file_mime)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| OCTET_STREAM.to_string());
    let mut tag_list = match tags {
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
    // Query-level tags merge with the field tags (order-preserving).
    for tag in q.tag_list() {
        if !tag_list.contains(&tag) {
            tag_list.push(tag);
        }
    }

    // M-04: the shared validation layer runs on the extracted metadata
    // BEFORE any pipeline work.
    let expected_size = (q.expected_size.unwrap_or(0) > 0).then_some(q.expected_size.unwrap_or(0));
    let view = UploadRequestView {
        tenant: ctx.tenant_id,
        name: &name,
        mime: &mime,
        tags: &tag_list,
        expected_size,
        include_deleted: false,
        role: ctx.role,
    };
    validate_upload(&view, &state.0.cfg.limits)?;

    let stream = Box::pin(spooled_file_stream(tmp));

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

/// H-02: reads a multipart field with a hard cap. Errors on the first byte
/// beyond `max` — the allocation never exceeds `max + one chunk`, no matter
/// how large the client streams.
async fn read_bounded_field(
    field: &mut axum::extract::multipart::Field<'_>,
    max: usize,
) -> Result<Vec<u8>, StorageError> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096.min(max));
    while let Some(chunk) = field.chunk().await.map_err(StorageError::from)? {
        if buf.len() + chunk.len() > max {
            return Err(StorageError::TooLarge(format!(
                "multipart metadata field exceeds the {max}-byte cap"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// H-02: consume-and-discard an unknown field under a hard cap. Keeps the
/// multipart framing sane for subsequent fields; never materializes the
/// body into a String.
async fn drain_bounded(
    field: &mut axum::extract::multipart::Field<'_>,
    max: usize,
) -> Result<(), StorageError> {
    let mut total = 0usize;
    while let Some(chunk) = field.chunk().await.map_err(StorageError::from)? {
        total += chunk.len();
        if total > max {
            return Err(StorageError::TooLarge(format!(
                "multipart field exceeds the {max}-byte cap"
            )));
        }
    }
    Ok(())
}
