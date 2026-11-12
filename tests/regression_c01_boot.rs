//! C-01 regression: the binary must refuse to boot with shipped/known
//! credentials. Uses the release binary when present, else the debug one —
//! the shipped `default.yaml` is rejected in BOTH profiles because its
//! jwt_secret matches a shipped literal.

use std::process::Command;
use std::time::Instant;

#[test]
fn c01_boot_refuses_default_yaml() {
    // Prefer the release binary (production profile semantics), fall back
    // to the debug binary — the shipped config fails validation either way.
    let candidates = ["target/release/file-storage", "target/debug/file-storage"];
    let bin = candidates
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .expect("build the binary first (cargo build)");
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();

    let started = Instant::now();
    let out = Command::new(bin)
        .args([
            "--config",
            "config/default.yaml",
            // isolated data dirs so nothing is written even on partial boot
        ])
        .env("FS_STORAGE__ROOT", dir.path().join("b"))
        .env("FS_STORAGE__TEMP_DIR", dir.path().join("t"))
        .env("FS_DATABASE__PATH", dir.path().join("i.redb"))
        .env("FS_ENCRYPTION__KEYS_DIR", dir.path().join("k"))
        .output()
        .expect("spawn binary");
    let elapsed = started.elapsed();

    assert!(
        !out.status.success(),
        "C-01: boot with the shipped default.yaml must fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("auth.jwt_secret") || stderr.contains("shipped/known literal"),
        "stderr must name the failing secret: {stderr}"
    );
    // Refusal must be fast (validation, not a half-booted service).
    assert!(elapsed.as_secs() < 30, "refusal took too long: {elapsed:?}");
}
