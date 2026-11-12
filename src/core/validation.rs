//! M-04: the single upload-request validation layer shared by BOTH
//! transports.
//!
//! Contract (V5 Wave 2, executing V4 §5.5): REST and gRPC handlers reduce
//! to "extract fields from the wire type into [`UploadRequestView`], call
//! [`validate_upload`], map the error onto the wire". All name/tag/mime/
//! expected_size/role gating decisions live HERE — cross-transport drift
//! becomes structurally impossible, and CI rule `no-handler-validation`
//! (scripts/check_no_handler_validation.sh) keeps it that way.
//!
//! Sub-findings closed here:
//! * M-04a — `include_deleted` is admin-only on BOTH transports
//!   ([`can_list_deleted`] / [`ensure_can_list_deleted`]).
//! * M-04b — one name rule: 1..=255 bytes after trim, no C0/C1 control
//!   characters. (V4 §5.5 says "chars"; bytes are the conservative
//!   encoding-safe superset — a byte cap also caps chars.)
//! * L-06 — tag caps land with the same logic: <= 32 tags, each
//!   1..=64 bytes, no control characters.
//! * M-04c — chunk-list cap constant [`MAX_LISTED_CHUNKS`] (the gRPC
//!   GetManifest handler truncates at this cap and sets the additive
//!   `chunks_truncated` proto flag; the REST meta DTO already capped).
//! * M-04d — list totals: both transports call the same
//!   `IndexDb::list_filtered` counting code (see storage/index_db.rs).
//! * M-04e — `expected_size`, when present, is capped at
//!   `limits.max_file_size` by the same code on both transports.

use crate::config::{LimitsConfig, Role};
use crate::error::StorageError;
use uuid::Uuid;

/// Longest acceptable file name (bytes, after the handler's trim).
pub const MAX_NAME_BYTES: usize = 255;

/// Most tags a single upload may carry.
pub const MAX_TAG_COUNT: usize = 32;

/// Longest acceptable single tag (bytes, after trim).
pub const MAX_TAG_LEN: usize = 64;

/// Longest acceptable declared MIME type (bytes).
pub const MAX_MIME_LEN: usize = 128;

/// M-04c: hard cap on chunk lists served by read endpoints (REST meta
/// DTO and gRPC GetManifest alike). Replication paths are uncapped.
pub const MAX_LISTED_CHUNKS: usize = 1000;

/// Transport-agnostic view of an upload request. Handlers extract fields
/// from their wire type and construct this view; they do NOT validate
/// anything themselves.
pub struct UploadRequestView<'a> {
    /// Resolved target tenant (admin cross-tenant override already
    /// applied by the handler's tenant-selection code).
    pub tenant: Uuid,
    pub name: &'a str,
    pub mime: &'a str,
    pub tags: &'a [String],
    /// Client-declared content size, normalized so that 0 means
    /// "unknown" on both transports.
    pub expected_size: Option<u64>,
    /// List-request parameter; upload handlers always pass `false`.
    /// Checked here so the admin gate exists in exactly one place.
    pub include_deleted: bool,
    pub role: Role,
}

/// The single source of truth for upload-request validation. Called by
/// REST and gRPC handlers BEFORE any gate/pipeline work (a malformed
/// request must never reach admission control or the chunker).
pub fn validate_upload(
    v: &UploadRequestView<'_>,
    limits: &LimitsConfig,
) -> Result<(), StorageError> {
    // (a) name: 1..=255 bytes, no control characters (C0/C1).
    //     Fixes M-04b on BOTH transports.
    validate_name(v.name)?;

    // (b) tags: <= 32 tags, each 1..=64 bytes, no control characters.
    //     Fixes L-06 in the same move (it was the same logic, twice).
    validate_tags(v.tags)?;

    // (c) mime: length-capped here (<= 128 bytes); the content/magic-byte
    //     check is the pipeline head's job (H-03, already landed).
    validate_mime_len(v.mime)?;

    // (d) expected_size: when present, <= limits.max_file_size.
    //     Fixes M-04e on BOTH transports. Note the check must run before
    //     the H-01 upload gate so an over-cap declaration is a clean
    //     400/INVALID_ARGUMENT, not a budget RESOURCE_EXHAUSTED.
    if let Some(sz) = v.expected_size {
        if sz > limits.max_file_size {
            return Err(StorageError::Validation(format!(
                "expected_size {sz} exceeds max_file_size ({} bytes)",
                limits.max_file_size
            )));
        }
    }

    // (e) include_deleted: only admins may set it true.
    //     Fixes M-04a on BOTH transports.
    if v.include_deleted && !can_list_deleted(v.role) {
        return Err(StorageError::Forbidden(
            "include_deleted requires the admin role".into(),
        ));
    }

    Ok(())
}

/// Used by list endpoints on both transports. Fixes M-04a parity for the
/// read path as well.
pub fn can_list_deleted(role: Role) -> bool {
    role.is_admin()
}

/// List-endpoint gate: non-admins asking for deleted files are refused
/// outright (403 / PERMISSION_DENIED) on both transports — no silent
/// coercion, no silent honoring.
pub fn ensure_can_list_deleted(role: Role) -> Result<(), StorageError> {
    if can_list_deleted(role) {
        Ok(())
    } else {
        Err(StorageError::Forbidden(
            "include_deleted requires the admin role".into(),
        ))
    }
}

/// M-04e: pre-admission `expected_size` cap check. REST calls this BEFORE
/// the H-01 upload gate so an over-cap declaration is a clean
/// 400/INVALID_ARGUMENT instead of a budget 413/RESOURCE_EXHAUSTED; gRPC
/// gets the same outcome from `validate_upload` running before its gate.
pub fn validate_expected_size(
    expected_size: Option<u64>,
    limits: &LimitsConfig,
) -> Result<(), StorageError> {
    if let Some(sz) = expected_size {
        if sz > limits.max_file_size {
            return Err(StorageError::Validation(format!(
                "expected_size {sz} exceeds max_file_size ({} bytes)",
                limits.max_file_size
            )));
        }
    }
    Ok(())
}

/// Name rule (M-04b): 1..=255 bytes after the handler's trim; C0 (0x00-0x1F)
/// and C1 (0x7F-0x9F) control characters are rejected.
fn validate_name(name: &str) -> Result<(), StorageError> {
    if name.is_empty() {
        return Err(StorageError::Validation(
            "name must be 1..=255 bytes after trim".into(),
        ));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(StorageError::Validation(format!(
            "name must be 1..={MAX_NAME_BYTES} bytes after trim (got {})",
            name.len()
        )));
    }
    if has_control_chars(name) {
        return Err(StorageError::Validation(
            "name must not contain control characters".into(),
        ));
    }
    Ok(())
}

/// Tag rules (L-06): <= 32 tags, each 1..=64 bytes, no control characters.
fn validate_tags(tags: &[String]) -> Result<(), StorageError> {
    if tags.len() > MAX_TAG_COUNT {
        return Err(StorageError::Validation(format!(
            "at most {MAX_TAG_COUNT} tags are allowed (got {})",
            tags.len()
        )));
    }
    for tag in tags {
        if tag.is_empty() {
            return Err(StorageError::Validation("tags must be non-empty".into()));
        }
        if tag.len() > MAX_TAG_LEN {
            return Err(StorageError::Validation(format!(
                "tag must be 1..={MAX_TAG_LEN} bytes (got {})",
                tag.len()
            )));
        }
        if has_control_chars(tag) {
            return Err(StorageError::Validation(
                "tags must not contain control characters".into(),
            ));
        }
    }
    Ok(())
}

/// MIME length cap (M-04 helpers): declared types longer than 128 bytes
/// are refused; the magic-byte content check stays in the pipeline head.
fn validate_mime_len(mime: &str) -> Result<(), StorageError> {
    if mime.len() > MAX_MIME_LEN {
        return Err(StorageError::Validation(format!(
            "mime must be at most {MAX_MIME_LEN} bytes (got {})",
            mime.len()
        )));
    }
    if has_control_chars(mime) {
        return Err(StorageError::Validation(
            "mime must not contain control characters".into(),
        ));
    }
    Ok(())
}

/// True when `s` contains C0 or C1 control characters.
fn has_control_chars(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '\u{0000}'..='\u{001F}' | '\u{007F}'..='\u{009F}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> LimitsConfig {
        LimitsConfig {
            max_file_size: 1024 * 1024,
            max_body: 1024 * 1024,
            max_in_flight_bytes: 1024 * 1024,
            max_concurrent_uploads: 4,
        }
    }

    fn view<'a>(
        name: &'a str,
        mime: &'a str,
        tags: &'a [String],
        expected_size: Option<u64>,
        role: Role,
    ) -> UploadRequestView<'a> {
        UploadRequestView {
            tenant: Uuid::nil(),
            name,
            mime,
            tags,
            expected_size,
            include_deleted: false,
            role,
        }
    }

    #[test]
    fn accepts_well_formed_request() {
        let tags = vec!["a".to_string(), "b-2".to_string()];
        let v = view("photo.png", "image/png", &tags, Some(10), Role::User);
        assert!(validate_upload(&v, &limits()).is_ok());
    }

    #[test]
    fn rejects_name_at_256_bytes() {
        let name = "n".repeat(256);
        let v = view(&name, "application/octet-stream", &[], None, Role::User);
        let err = validate_upload(&v, &limits()).unwrap_err();
        assert!(matches!(err, StorageError::Validation(_)));
        // Boundary: exactly 255 bytes is fine.
        let name = "n".repeat(255);
        let v = view(&name, "application/octet-stream", &[], None, Role::User);
        assert!(validate_upload(&v, &limits()).is_ok());
    }

    #[test]
    fn rejects_empty_name() {
        let v = view("", "application/octet-stream", &[], None, Role::User);
        assert!(validate_upload(&v, &limits()).is_err());
    }

    #[test]
    fn rejects_control_characters_in_name_and_tags() {
        for bad in ["a\nb", "a\u{7f}b", "a\u{9f}b", "a\u{1f}b"] {
            let v = view(bad, "application/octet-stream", &[], None, Role::User);
            assert!(validate_upload(&v, &limits()).is_err(), "name {bad:?}");
        }
        let tags = vec!["a\u{1}b".to_string()];
        let v = view(
            "ok.bin",
            "application/octet-stream",
            &tags,
            None,
            Role::User,
        );
        assert!(validate_upload(&v, &limits()).is_err());
    }

    #[test]
    fn rejects_tag_count_over_32_and_len_over_64() {
        let tags: Vec<String> = (0..33).map(|i| format!("t{i}")).collect();
        let v = view(
            "ok.bin",
            "application/octet-stream",
            &tags,
            None,
            Role::User,
        );
        assert!(validate_upload(&v, &limits()).is_err());

        let tags = vec!["t".repeat(65)];
        let v = view(
            "ok.bin",
            "application/octet-stream",
            &tags,
            None,
            Role::User,
        );
        assert!(validate_upload(&v, &limits()).is_err());

        // Boundary: 32 tags of 64 bytes each are fine.
        let tags: Vec<String> = (0..32).map(|_| "t".repeat(64)).collect();
        let v = view(
            "ok.bin",
            "application/octet-stream",
            &tags,
            None,
            Role::User,
        );
        assert!(validate_upload(&v, &limits()).is_ok());
    }

    #[test]
    fn rejects_mime_over_128_bytes() {
        let mime = format!("application/{}", "x".repeat(120)); // 132 bytes
        let v = view("ok.bin", &mime, &[], None, Role::User);
        assert!(validate_upload(&v, &limits()).is_err());
        // Boundary: exactly 128 bytes is fine.
        let mime = format!("application/{}", "x".repeat(116)); // 128 bytes
        let v = view("ok.bin", &mime, &[], None, Role::User);
        assert!(validate_upload(&v, &limits()).is_ok());
    }

    #[test]
    fn rejects_expected_size_over_limit() {
        let v = view(
            "ok.bin",
            "application/octet-stream",
            &[],
            Some(1024 * 1024 + 1),
            Role::User,
        );
        let err = validate_upload(&v, &limits()).unwrap_err();
        assert!(matches!(err, StorageError::Validation(_)));
    }

    #[test]
    fn include_deleted_requires_admin() {
        let mut v = view(
            "ok.bin",
            "application/octet-stream",
            &[],
            None,
            Role::Viewer,
        );
        v.include_deleted = true;
        assert!(matches!(
            validate_upload(&v, &limits()),
            Err(StorageError::Forbidden(_))
        ));
        let admin = view("ok.bin", "application/octet-stream", &[], None, Role::Admin);
        assert!(validate_upload(&admin, &limits()).is_ok());
        assert!(can_list_deleted(Role::Admin));
        assert!(!can_list_deleted(Role::Viewer));
        assert!(!can_list_deleted(Role::User));
        assert!(ensure_can_list_deleted(Role::Admin).is_ok());
        assert!(ensure_can_list_deleted(Role::Viewer).is_err());
    }
}
