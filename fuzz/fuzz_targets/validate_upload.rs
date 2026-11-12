//! CI-W4 fuzz skeleton: `validate_upload` must be total — any UTF-8 input
//! returns Ok or Err, never panics. Run out-of-band:
//! `cargo +nightly fuzz run validate_upload -- -max_len=4096`.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let limits = file_storage::config::LimitsConfig {
            max_file_size: 1024 * 1024,
            max_body: 1024 * 1024,
            max_in_flight_bytes: 1024 * 1024,
            max_concurrent_uploads: 4,
        };
        let v = file_storage::core::validation::UploadRequestView {
            tenant: uuid::Uuid::nil(),
            name: s,
            mime: "application/octet-stream",
            tags: &[],
            expected_size: None,
            include_deleted: false,
            role: file_storage::config::Role::User,
        };
        let _ = file_storage::core::validation::validate_upload(&v, &limits);
    }
});
