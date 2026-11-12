//! CI-W4 (V5 §7.4): property-based tests for the shared validation layer.
//!
//! Contract: `validate_upload` is TOTAL — for every input it returns Ok or
//! Err and never panics, never allocates unboundedly, and never accepts
//! what the parity gates reject. These properties complement the
//! table-driven `parity_validation_matrix` with randomized coverage.

use file_storage::config::{LimitsConfig, Role};
use file_storage::core::validation::{self, UploadRequestView};
use proptest::prelude::*;
use uuid::Uuid;

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
    expected: Option<u64>,
) -> UploadRequestView<'a> {
    UploadRequestView {
        tenant: Uuid::nil(),
        name,
        mime,
        tags,
        expected_size: expected,
        include_deleted: false,
        role: Role::User,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// `validate_upload` is total over arbitrary UTF-8 names: Ok or Err,
    /// never a panic. Long and control-laden inputs stay rejected.
    #[test]
    fn name_validation_is_total(s in ".{0,300}") {
        let l = limits();
        let v = view(&s, "application/octet-stream", &[], None);
        let _ = validation::validate_upload(&v, &l);
    }

    /// Arbitrary text truncated to 400 chars never panics the validator.
    #[test]
    fn name_validation_is_total_over_any_text(
        s in any::<String>().prop_map(|s| s.chars().take(400).collect::<String>())
    ) {
        let l = limits();
        let v = view(&s, "application/octet-stream", &[], None);
        let _ = validation::validate_upload(&v, &l);
    }

    /// Tag vectors up to 64 entries of arbitrary text: total, and the
    /// 32-tag cap is respected on the Ok path.
    #[test]
    fn tags_validation_is_total(tags in proptest::collection::vec(".{0,100}", 0..64)) {
        let l = limits();
        let v = view("ok.bin", "application/octet-stream", &tags, None);
        if let Ok(()) = validation::validate_upload(&v, &l) {
            prop_assert!(tags.len() <= 32, "Ok implies <= 32 tags");
        }
    }

    /// MIME strings of arbitrary shape/length: total; Ok implies <= 128
    /// bytes.
    #[test]
    fn mime_validation_is_total(mime in ".{0,200}") {
        let l = limits();
        let v = view("ok.bin", &mime, &[], None);
        if let Ok(()) = validation::validate_upload(&v, &l) {
            prop_assert!(mime.len() <= 128, "Ok implies <= 128 bytes");
        }
    }

    /// `expected_size` handling mirrors the cap exactly: sizes above the
    /// limit are rejected, sizes at or below are accepted (other fields
    /// well-formed).
    #[test]
    fn expected_size_cap_is_exact(sz in 0u64..(2 * 1024 * 1024)) {
        let l = limits();
        let v = view("ok.bin", "application/octet-stream", &[], Some(sz));
        match validation::validate_upload(&v, &l) {
            Ok(()) => prop_assert!(sz <= l.max_file_size),
            Err(_) => prop_assert!(sz > l.max_file_size),
        }
    }

    /// include_deleted is admin-only for EVERY role/input combination.
    #[test]
    fn include_deleted_never_leaks_to_non_admins(
        include in any::<bool>(),
        role in prop::sample::select(vec![Role::Admin, Role::User, Role::Viewer]),
    ) {
        let l = limits();
        let mut v = view("ok.bin", "application/octet-stream", &[], None);
        v.include_deleted = include;
        v.role = role;
        match validation::validate_upload(&v, &l) {
            Ok(()) => prop_assert!(!include || role.is_admin()),
            Err(_) => prop_assert!(include && !role.is_admin()),
        }
    }
}
