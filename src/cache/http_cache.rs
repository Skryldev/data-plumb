//! HTTP caching helpers: ETag construction and `If-None-Match` evaluation
//! for conditional GETs (304 support).

use crate::storage::manifest::Manifest;

/// Strong ETag for a file: quoted BLAKE3 hex of the original content.
pub fn etag_for(m: &Manifest) -> String {
    format!("\"{}\"", hex::encode(m.original_hash))
}

/// Evaluates an `If-None-Match` header value against the resource ETag.
/// Handles `*`, single tags, comma-separated lists, and both quoted and
/// unquoted forms. Weak tags (`W/"..."`) match by opaque value.
pub fn if_none_match_matches(header_value: &str, etag: &str) -> bool {
    let etag = etag.trim();
    for candidate in header_value.split(',') {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            continue;
        }
        if candidate == "*" {
            return true;
        }
        let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
        if candidate == etag {
            return true;
        }
        // tolerate unquoted etag in request
        if !candidate.starts_with('"') && candidate == etag.trim_matches('"') {
            return true;
        }
    }
    false
}

/// Cache-Control header for immutable-after-write content.
pub const CACHE_CONTROL: &str = "private, max-age=300";

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn manifest() -> Manifest {
        Manifest {
            file_id: Uuid::now_v7(),
            name: "f".into(),
            tenant_id: Uuid::now_v7(),
            mime: "a/b".into(),
            size_original: 1,
            size_stored: 1,
            original_hash: [1u8; 32],
            chunks: vec![],
            tags: vec![],
            created_at_ms: 0,
            updated_at_ms: 0,
            deleted_at_ms: None,
            version: 1,
        }
    }

    #[test]
    fn etag_is_quoted_hex() {
        let etag = etag_for(&manifest());
        assert_eq!(etag, format!("\"{}\"", "01".repeat(32)));
    }

    #[test]
    fn single_match() {
        let etag = etag_for(&manifest());
        assert!(if_none_match_matches(&etag, &etag));
        assert!(if_none_match_matches(&etag, &etag));
        assert!(!if_none_match_matches("\"deadbeef\"", &etag));
        assert!(!if_none_match_matches("", &etag));
    }

    #[test]
    fn list_and_wildcard() {
        let etag = etag_for(&manifest());
        assert!(if_none_match_matches("*", &etag));
        assert!(if_none_match_matches(
            &format!("\"aaa\", {etag}, \"bbb\""),
            &etag
        ));
        assert!(if_none_match_matches(&format!("W/{etag}"), &etag));
        assert!(!if_none_match_matches("\"aaa\", \"bbb\"", &etag));
    }

    #[test]
    fn unquoted_request_etag_matches() {
        let etag = etag_for(&manifest());
        let bare = etag.trim_matches('"');
        assert!(if_none_match_matches(bare, &etag));
    }
}
