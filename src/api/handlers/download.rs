//! `GET /files/:id` (full), `GET /files/:id/range` (206) and metadata.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use bytes::Bytes;
use futures::StreamExt;
use uuid::Uuid;

use crate::api::middleware::tenant::check_file_access;
use crate::cache::http_cache;
use crate::error::StorageError;
use crate::security::audit::{Actor, AuditAction, AuditLog};
use crate::security::auth::AuthContext;
use crate::state::AppState;

/// Parsed `Range: bytes=...` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64, // inclusive
}

#[derive(Debug)]
pub enum RangeParseError {
    Invalid,
    Unsatisfiable(u64),
}

/// Parses the first range of a `bytes=` Range header per RFC 7233.
/// Multi-range requests are served as their first range (documented).
pub fn parse_range(header: &str, size: u64) -> Result<ByteRange, RangeParseError> {
    let spec = header
        .trim()
        .strip_prefix("bytes=")
        .ok_or(RangeParseError::Invalid)?;
    let first = spec.split(',').next().unwrap_or("").trim();
    let (start_s, end_s) = first.split_once('-').ok_or(RangeParseError::Invalid)?;
    let start_s = start_s.trim();
    let end_s = end_s.trim();

    if start_s.is_empty() {
        // suffix form: last N bytes
        let n: u64 = end_s.parse().map_err(|_| RangeParseError::Invalid)?;
        if n == 0 {
            return Err(RangeParseError::Invalid);
        }
        if size == 0 {
            return Err(RangeParseError::Unsatisfiable(0));
        }
        let start = size.saturating_sub(n);
        return Ok(ByteRange {
            start,
            end: size - 1,
        });
    }
    let start: u64 = start_s.parse().map_err(|_| RangeParseError::Invalid)?;
    let end = if end_s.is_empty() {
        size.saturating_sub(1)
    } else {
        end_s.parse::<u64>().map_err(|_| RangeParseError::Invalid)?
    };
    if start >= size {
        return Err(RangeParseError::Unsatisfiable(size));
    }
    if start > end {
        return Err(RangeParseError::Invalid);
    }
    Ok(ByteRange {
        start,
        end: end.min(size - 1),
    })
}

fn text_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

fn map_stream_err(e: StorageError) -> axum::Error {
    axum::Error::new(e)
}

/// GET /api/v1/files/:id — full download with ETag/304 support.
pub async fn download_file(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(file_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, StorageError> {
    let manifest = state.0.retrieval.manifest(&file_id).await?;
    check_file_access(&ctx, &manifest.tenant_id)?;

    let etag = http_cache::etag_for(&manifest);
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if http_cache::if_none_match_matches(inm, &etag) {
            audit_download(&state, &ctx, &manifest.name, "304").await;
            return Ok((
                StatusCode::NOT_MODIFIED,
                [
                    (header::ETAG, etag),
                    (header::CACHE_CONTROL, http_cache::CACHE_CONTROL.to_string()),
                ],
            )
                .into_response());
        }
    }

    let size = manifest.size_original;
    let started = std::time::Instant::now();
    let mime = manifest.mime.clone();
    let name = manifest.name.clone();
    let m = manifest.clone();
    let stream = state
        .0
        .retrieval
        .stream_full(m)
        .map(|r| r.map_err(map_stream_err));
    audit_download(&state, &ctx, &name, "200").await;
    crate::metrics::record_download(started.elapsed().as_secs_f64(), "ok", size, "mixed");
    Ok(build_stream_response(StatusCode::OK, &mime, &name, size, &etag, stream).await)
}

/// GET /api/v1/files/:id/range — 206 partial content.
pub async fn download_range(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(file_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, StorageError> {
    let manifest = state.0.retrieval.manifest(&file_id).await?;
    check_file_access(&ctx, &manifest.tenant_id)?;

    let size = manifest.size_original;
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| StorageError::Validation("Range header is required".into()))?;
    let br = match parse_range(range_header, size) {
        Ok(r) => r,
        Err(RangeParseError::Unsatisfiable(s)) => {
            return Ok(text_response(
                StatusCode::RANGE_NOT_SATISFIABLE,
                format!("requested range not satisfiable (size={s})"),
            )
            .tap_content_range(s));
        }
        Err(RangeParseError::Invalid) => {
            return Err(StorageError::Validation(format!(
                "unsupported Range header '{range_header}'"
            )))
        }
    };

    let length = br.end - br.start + 1;
    let etag = http_cache::etag_for(&manifest);
    let mime = manifest.mime.clone();
    let name = manifest.name.clone();
    let m = manifest.clone();
    let stream = state
        .0
        .retrieval
        .stream_range(m, br.start, br.end + 1)
        .map(|r| r.map_err(map_stream_err));
    audit_download(&state, &ctx, &name, "206").await;

    let started = std::time::Instant::now();
    let mut resp = build_stream_response(
        StatusCode::PARTIAL_CONTENT,
        &mime,
        &name,
        length,
        &etag,
        stream,
    )
    .await;
    resp.headers_mut().insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes {}-{}/{}", br.start, br.end, size))
            .map_err(|e| StorageError::Internal(format!("content-range: {e}")))?,
    );
    crate::metrics::record_download(started.elapsed().as_secs_f64(), "ok", length, "mixed");
    Ok(resp)
}

trait TapContentRange {
    fn tap_content_range(self, size: u64) -> Response;
}

impl TapContentRange for Response {
    fn tap_content_range(self, size: u64) -> Response {
        let mut r = self;
        if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
            r.headers_mut().insert(header::CONTENT_RANGE, v);
        }
        r
    }
}

async fn build_stream_response<S>(
    status: StatusCode,
    mime: &str,
    name: &str,
    length: u64,
    etag: &str,
    stream: S,
) -> Response
where
    S: futures::Stream<Item = Result<Bytes, axum::Error>> + Send + 'static,
{
    let filename = sanitize_filename(name);
    let body = Body::from_stream(stream);
    (
        status,
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CONTENT_LENGTH, length.to_string()),
            (header::ETAG, etag.to_string()),
            (header::CACHE_CONTROL, http_cache::CACHE_CONTROL.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{filename}\""),
            ),
            (
                header::HeaderName::from_static("x-content-hash"),
                etag.trim_matches('"').to_string(),
            ),
        ],
        body,
    )
        .into_response()
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._- ".contains(*c))
        .take(128)
        .collect();
    if cleaned.is_empty() {
        "download.bin".into()
    } else {
        cleaned
    }
}

async fn audit_download(state: &AppState, ctx: &AuthContext, name: &str, status: &str) {
    AuditLog::new(state.0.index.clone())
        .record(
            AuditAction::Download,
            Actor::User(ctx.username.clone()),
            Some(ctx.tenant_id),
            name,
            "",
            status,
            "",
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_range() {
        let r = parse_range("bytes=0-", 100).unwrap();
        assert_eq!(r, ByteRange { start: 0, end: 99 });
    }

    #[test]
    fn closed_range() {
        let r = parse_range("bytes=10-19", 100).unwrap();
        assert_eq!(r, ByteRange { start: 10, end: 19 });
    }

    #[test]
    fn end_clamped_to_size() {
        let r = parse_range("bytes=90-999", 100).unwrap();
        assert_eq!(r, ByteRange { start: 90, end: 99 });
    }

    #[test]
    fn suffix_range() {
        let r = parse_range("bytes=-50", 100).unwrap();
        assert_eq!(r, ByteRange { start: 50, end: 99 });
        let r = parse_range("bytes=-500", 100).unwrap();
        assert_eq!(r, ByteRange { start: 0, end: 99 });
    }

    #[test]
    fn unsatisfiable() {
        assert!(matches!(
            parse_range("bytes=100-", 100),
            Err(RangeParseError::Unsatisfiable(100))
        ));
        assert!(matches!(
            parse_range("bytes=200-300", 100),
            Err(RangeParseError::Unsatisfiable(100))
        ));
        assert!(matches!(
            parse_range("bytes=-1", 0),
            Err(RangeParseError::Unsatisfiable(0))
        ));
    }

    #[test]
    fn invalid_headers() {
        assert!(matches!(
            parse_range("chunks=0-1", 10),
            Err(RangeParseError::Invalid)
        ));
        assert!(matches!(
            parse_range("bytes=", 10),
            Err(RangeParseError::Invalid)
        ));
        assert!(matches!(
            parse_range("bytes=abc-", 10),
            Err(RangeParseError::Invalid)
        ));
        assert!(matches!(
            parse_range("bytes=5-2", 10),
            Err(RangeParseError::Invalid)
        ));
        assert!(matches!(
            parse_range("bytes=-0", 10),
            Err(RangeParseError::Invalid)
        ));
    }

    #[test]
    fn multi_range_takes_first() {
        let r = parse_range("bytes=0-4,10-19", 100).unwrap();
        assert_eq!(r, ByteRange { start: 0, end: 4 });
    }

    #[test]
    fn zero_size_file() {
        assert!(matches!(
            parse_range("bytes=0-0", 0),
            Err(RangeParseError::Unsatisfiable(0))
        ));
    }

    #[test]
    fn filename_sanitized() {
        assert_eq!(sanitize_filename("ok file_v2.txt"), "ok file_v2.txt");
        assert_eq!(sanitize_filename("bad/../name"), "bad..name");
        assert_eq!(sanitize_filename(""), "download.bin");
    }
}
