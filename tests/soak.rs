//! Soak (V7-P1 promotion, rule: catches V6-03/V6-10-class drift
//! continuously). A bounded upload/delete/GC/reconcile loop with disk-growth
//! and RSS assertions.
//!
//! Default run: 60 s (CI slice, nightly job). Set `SOAK_SECONDS=86400` for
//! the full 24 h operator-dispatched run. `#[ignore]`d: nightly/soak only.

use file_storage::config::AppConfig;
use file_storage::gc::ReconcileMode;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn rss_bytes() -> Option<u64> {
    // Linux CI: /proc self status. Other platforms: skip (None).
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()
        .map(|kb| kb * 1024)
}

fn dir_usage(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                total += dir_usage(&p);
            } else if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "soak — nightly job (60 s slice) or operator dispatch (SOAK_SECONDS)"]
async fn soak_upload_delete_gc_reconcile_loop() {
    let seconds: u64 = std::env::var("SOAK_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);

    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let mut cfg = AppConfig::for_tests();
    cfg = cfg.with_data_root(dir.path());
    cfg.limits.max_in_flight_bytes = 256 * 1024 * 1024;
    cfg.limits.max_concurrent_uploads = 8;
    let state = Arc::new(
        file_storage::state::build_state(Arc::new(cfg))
            .await
            .unwrap(),
    );

    let token = "soak-token";
    let _ = token;
    // Auth is in-process here; drive the pipeline directly with admin's
    // tenant (the same path REST/gRPC use).
    let tenant: uuid::Uuid = "01930000-0000-7000-8000-000000000001".parse().unwrap();
    let start_rss = rss_bytes();
    let start_disk = dir_usage(dir.path());
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut op = 0u64;
    let mut uploads = 0u64;
    let mut deleted = 0u64;

    while Instant::now() < deadline {
        match op % 4 {
            0 | 1 => {
                let payload = vec![(op % 251) as u8; 64 * 1024];
                let name = format!("soak-{op}.bin");
                let outcome = state
                    .0
                    .pipeline
                    .process(
                        tenant,
                        &name,
                        "application/octet-stream",
                        vec![],
                        // stream::iter is Unpin (unlike once(async { .. })) —
                        // matches UploadPipeline's `S: Stream + Unpin` bound.
                        futures::stream::iter(vec![Ok::<_, file_storage::error::StorageError>(
                            bytes::Bytes::from(payload),
                        )]),
                        "soak",
                        "127.0.0.1",
                    )
                    .await;
                assert!(
                    outcome.is_ok(),
                    "soak upload failed at op {op}: {outcome:?}"
                );
                uploads += 1;
            }
            2 => {
                let manifests = state.0.index.all_manifests().await.unwrap();
                if let Some(m) = manifests.iter().find(|m| !m.is_deleted()) {
                    state
                        .0
                        .index
                        .soft_delete_manifest(&m.file_id)
                        .await
                        .unwrap();
                    deleted += 1;
                }
            }
            _ => {
                state.0.gc.run_once().await.unwrap();
                let _ = state.0.gc.reconcile(ReconcileMode::Repair).await.unwrap();
            }
        }
        op += 1;

        // Growth assertions every 2000 ops: RSS must not runaway (the fused
        // pipeline is O(chunk)); disk must be bounded by GC/reconcile.
        if op % 2000 == 0 {
            if let (Some(base), Some(now)) = (start_rss, rss_bytes()) {
                assert!(
                    now < base * 4 + 256 * 1024 * 1024,
                    "RSS runaway: {base} -> {now} after {op} ops"
                );
            }
            let disk = dir_usage(dir.path());
            assert!(
                disk < start_disk * 8 + 1024 * 1024 * 1024,
                "disk runaway: {start_disk} -> {disk} after {op} ops (GC/reconcile not reclaiming?)"
            );
        }
    }

    tracing::info!(
        ops = op,
        uploads,
        deleted,
        elapsed_s = seconds,
        "soak slice complete"
    );
    // Final coherence: a reconcile Repair leaves nothing dangling.
    let report = state.0.gc.reconcile(ReconcileMode::Repair).await.unwrap();
    tracing::info!(?report, "final reconcile");
    assert_eq!(
        report.phantom_manifests, 0,
        "soak must end with zero phantom chunk rows"
    );
}
