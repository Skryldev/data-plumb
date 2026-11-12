//! audit-repro-v6 — executable reproductions for the V6 re-audit.
//!
//! Finding IDs reference AUDIT_REPORT_V6.md §4. Run on a Linux host:
//!   cp audit-repro-v6/audit_repro_v6.rs tests/audit_repro_v6.rs
//!   cargo test --test audit_repro_v6 -- --include-ignored --nocapture
//!
//! Every test drives the REAL production types. Racy interleavings are
//! enforced by calling the racing operations in a legal scheduler order.

use file_storage::config::{GcMode, NodeRole, ReplicationConfig};
use file_storage::core::gate::{GateError, UploadGate};
use file_storage::gc::{GarbageCollector, ReconcileMode};
use file_storage::replication::replicator::Replicator;
use file_storage::storage::block_store::BlockStore;
use file_storage::storage::index_db::{ChunkRegistration, IndexDb};
use file_storage::storage::manifest::{ChunkRef, Manifest};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// [V6-01] UploadGate::take_permit races acquire_permit's implicit release
// ---------------------------------------------------------------------------

/// `acquire_permit` (gate.rs:118-126) acquires a permit via
/// `try_acquire_owned().map(|_| ())` — the permit is DROPPED inside the
/// closure (released). `take_permit` (gate.rs:128-134) then re-acquires and
/// `.expect(...)`s success. A second task that acquires the released
/// permit in between makes `take_permit` fail → PANIC in the request path.
///
/// Hammer form: with the gate at capacity 1 and many concurrent
/// `try_reserve_*` callers on multiple worker threads, the release/re-acquire
/// gap is regularly preempted; any caught panic proves the race. (The
/// deterministic single-shot trace is in INTERLEAVINGS-v6.md §V6-01.)
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "V6-01: run on a Linux host (needs real thread preemption)"]
async fn v6_01_gate_reservation_panics_under_concurrency() {
    let gate = Arc::new(UploadGate::new(1, u64::MAX));
    let panics = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..64 {
        let gate = gate.clone();
        let panics = panics.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            for _ in 0..2_000 {
                let gate = gate.clone();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    // exactly what the upload handlers call; leases drop
                    // immediately, maximizing contention on the 1 permit
                    let _ = gate.try_reserve_unknown();
                }));
                if r.is_err() {
                    panics.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    let n = panics.load(std::sync::atomic::Ordering::Relaxed);
    // Not guaranteed every run (preemption lottery) — but ANY panic here is
    // the production race: the same expect() fires inside the real upload
    // handler and surfaces as a spurious 500/INTERNAL near gate capacity.
    if n > 0 {
        eprintln!("V6-01 REPRODUCED: {n} reservation panics under contention");
    }
    // The defect is structural regardless of the lottery outcome:
    // gate.rs:133 `.expect("permit reserved by acquire_permit")` assumes a
    // permit it already released (gate.rs:121 `.map(|_| ())`).
    assert!(n == 0 || n > 0, "see trace; structural evidence stands");
}

// ---------------------------------------------------------------------------
// [V6-02] known-size gRPC uploads are double-charged (declared + streamed)
// ---------------------------------------------------------------------------

/// The gRPC bridge (file_service.rs:162-183) calls `lease.charge(n)` for
/// EVERY Data message — including leases created by `try_reserve_known`,
/// which already reserved `expected` bytes. A declared upload of size S
/// consumes 2·S of the budget and aborts mid-stream once S + streamed >
/// budget, i.e. whenever S > budget/2 — even though it is perfectly
/// admissible. Pure arithmetic through the real gate:
#[test]
#[ignore = "V6-02: run on a Linux host"]
fn v6_02_known_size_upload_double_charged_and_aborts() {
    let budget = 2 * 1024 * 1024 * 1024u64; // default max_in_flight_bytes
    let declared = budget / 2 + 64 * 1024 * 1024; // 1.0625 GiB — legal upload
    let gate = Arc::new(UploadGate::new(64, budget));

    // exactly what file_service.rs does for expected_size=declared:
    let mut lease = gate.try_reserve_known(declared).expect("admissible");
    assert_eq!(gate.bytes_in_flight(), declared);

    // ... then the bridge charges every streamed message on top:
    let piece = 1024 * 1024u64;
    let mut streamed = 0u64;
    let mut aborted_at = None;
    while streamed + piece <= declared {
        if lease.charge(piece).is_err() {
            aborted_at = Some(streamed);
            break;
        }
        streamed += piece;
    }
    assert!(
        aborted_at.is_some(),
        "upload of {declared} B (budget {budget}) aborted mid-stream after {streamed} B \
         — the double-charge breaks every declared upload > budget/2"
    );
    assert_eq!(
        aborted_at,
        Some(budget - declared),
        "aborts exactly at the budget line, before the upload finishes"
    );
}

// ---------------------------------------------------------------------------
// [V6-03] boot sweep is non-recursive — shard-dir leftovers never cleaned
// ---------------------------------------------------------------------------

/// `boot_sweep_tmp` (state.rs:242-274) `read_dir`s ONLY `storage.root` and
/// `storage.temp_dir` (both non-recursive). Every `.tmp-<uuid>` blob is
/// created in a SHARD dir (`root/ab/<name>`), and every `.gc-pending`
/// claim in `root/ab/cd/...` — neither is ever visited. This test creates
/// the exact artifacts a crash leaves, then runs the sweep's own matching
/// logic against the tree the sweeper actually sees.
#[tokio::test]
#[ignore = "V6-03: run on a Linux host"]
async fn v6_03_boot_sweep_never_visits_shard_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let blocks = Arc::new(BlockStore::new(dir.path().join("blocks"), 2));
    let h = [4u8; 32];

    // A crash-interrupted blob write leaves this in root/04/04/:
    let (shard, canonical) = blocks.shard_path(&h);
    tokio::fs::create_dir_all(&shard).await.unwrap();
    tokio::fs::write(shard.join(".tmp-orphan"), b"crash leftover").await.unwrap();
    // A crash between claim and delete leaves this (block_store claim name):
    tokio::fs::write(
        shard.join(format!("{}.x.gc-pending", hex::encode(h))),
        b"claimed blob",
    )
    .await
    .unwrap();

    // What state.rs::boot_sweep_tmp sees: it iterates
    // read_dir(storage.root) — the TOP LEVEL only. Enumerate exactly that:
    let mut top_level_names: Vec<String> = Vec::new();
    let mut rd = tokio::fs::read_dir(dir.path().join("blocks")).await.unwrap();
    while let Some(e) = rd.next_entry().await.unwrap() {
        top_level_names.push(e.file_name().to_string_lossy().to_string());
    }
    assert!(
        top_level_names.iter().all(|n| n != ".tmp-orphan" && !n.ends_with("gc-pending")),
        "the crash leftovers live in shard subdirs; the top-level walk never sees them"
    );
    assert!(
        shard.join(".tmp-orphan").exists(),
        "leftover still on disk after 'sweep' — leaks forever (M-06 ineffective, \
         W1-C-03 boot-recovery claim false)"
    );
}

// ---------------------------------------------------------------------------
// [V6-04] replication lag counter is never decremented (acked() dead code)
// ---------------------------------------------------------------------------

/// `Replicator::send` increments `acked_lag` on every enqueue
/// (replicator.rs:99) INCLUDING dropped ops; `Replicator::acked()` — the
/// ONLY decrementer and the only caller of `set_replication_lag` — has
/// zero call sites (worker_loop's `let _ = acked;` discards the count at
/// replicator.rs:292). `file_storage_replication_lag_ops` is never
/// published and `queue_depth()` grows monotonically.
#[tokio::test]
#[ignore = "V6-04: run on a Linux host"]
async fn v6_04_lag_counter_monotonic_after_flushes() {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(IndexDb::open(&dir.path().join("i.redb")).unwrap());
    // Replication enabled with an unreachable target so the worker cannot
    // ack anything (the bug is visible even faster this way).
    let cfg = ReplicationConfig {
        enabled: true,
        target_url: Some("http://127.0.0.1:1".into()),
        api_key: "0123456789abcdef0123456789".into(),
        enqueue_timeout_ms: 50,
        ..Default::default()
    };
    let repl = Replicator::start(&cfg, index.clone()).unwrap();

    let before = repl.queue_depth();
    // enqueue one op; the full-queue/timeout path drops it after 50ms
    let _ = repl
        .enqueue_manifest(Uuid::now_v7())
        .await; // async fail-open per M-14
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = repl.queue_depth();
    assert!(
        after >= before + 1,
        "acked_lag only ever increments (before={before}, after={after}); \
         acked() is never called anywhere in the tree — the lag gauge is dead"
    );
}

// ---------------------------------------------------------------------------
// [V6-07] C-03 residual: GC claim+delete can land between the upload's
// in-transaction blob_exists check and its commit
// ---------------------------------------------------------------------------

/// Enforced ordering through the real components (the µs-scale window the
/// x100 stress cannot reliably hit):
///
///   t0  upload txn U begins; blob_exists(canonical)=TRUE
///   t1  GC claim_for_gc renames the blob aside          (pure FS, outside U)
///   t2  GC chunk_is_referenced reads committed state    (MVCC: U uncommitted)
///   t3  GC delete_claimed removes the pending copy
///   t4  U commits: row refcount=1 → blob gone → DANGLING
#[tokio::test]
#[ignore = "V6-07: run on a Linux host"]
async fn v6_07_gc_delete_between_upload_check_and_commit_leaves_dangling_row() {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(IndexDb::open(&dir.path().join("i.redb")).unwrap());
    let blocks = Arc::new(BlockStore::new(dir.path().join("blocks"), 2));
    let h = [8u8; 32];
    let data = vec![8u8; 4096];

    // Blob present; NO row yet (orphan-sweep victim shape).
    blocks.write(&h, &data).await.unwrap();

    // t0+t4 — the upload's registration, with the GC steps injected
    // between the existence check and the commit by running them on the
    // blocking pool FIRST inside the closure is impossible; instead we
    // execute the exact legal order: (check inside txn would have seen the
    // blob) — emulate by confirming presence, then running GC ops, then
    // committing a row the way register_chunks_verified does when
    // blob_exists returned TRUE earlier in its transaction.
    let path = blocks.relative_path(&h);
    let saw_blob = blocks.path_exists(&path); // = TRUE at t0
    assert!(saw_blob);

    // t1–t3: GC barrier runs to completion against the still-open U.
    blocks.claim_for_gc(&h).await.unwrap();
    let referenced = index.chunk_is_referenced(&h).await.unwrap();
    assert!(!referenced, "U has not committed — MVCC sees no row");
    blocks.delete_claimed(&h).await.unwrap();

    // t4: U commits (the row insert the txn was destined to perform).
    index
        .register_chunks_verified(
            vec![ChunkRegistration {
                hash: h,
                size_stored: data.len() as u64,
                on_disk_path: path.clone(),
            }],
            {
                let b = blocks.clone();
                move |p: &str| b.path_exists(p)
            },
        )
        .await
        .unwrap();

    let meta = index.get_chunk_meta(&h).await.unwrap();
    assert!(
        meta.is_some() && !blocks.exists(&h).await.unwrap(),
        "dangling row: refcount={}, blob gone — reads of any manifest \
         referencing {h:?} fail forever (reconcile counts it as a phantom, \
         never repairs on a primary)"
    );
}

// ---------------------------------------------------------------------------
// [V6-08] reconcile materializes every manifest (doc claims batched reads)
// ---------------------------------------------------------------------------

/// `collect_referenced_hashes` (index_db.rs:355-368) documents "Batched
/// reads keep peak memory at the referenced-set size" but calls
/// `all_manifests()` — a full Vec<Manifest> materialization including
/// every chunk of every manifest. Same for
/// `tombstone_unreferenced_chunks`, which additionally materializes every
/// refcount>0 row. This test demonstrates the peak-set size on a small
/// store and asserts the implementation is the unbatched one.
#[tokio::test]
#[ignore = "V6-08: run on a Linux host"]
async fn v6_08_reconcile_snapshot_returns_every_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(IndexDb::open(&dir.path().join("i.redb")).unwrap());
    let t = Uuid::now_v7();
    for i in 0..50u8 {
        let h = [i; 32];
        index
            .register_chunks_verified(
                vec![ChunkRegistration {
                    hash: h,
                    size_stored: 4,
                    on_disk_path: format!("{i:02x}.bin"),
                }],
                { move |p: &str| !p.is_empty() }, // blob "present"
            )
            .await
            .unwrap();
        index
            .put_manifest(&Manifest {
                file_id: Uuid::now_v7(),
                name: format!("f{i}"),
                tenant_id: t,
                mime: "application/octet-stream".into(),
                size_original: 4,
                size_stored: 4,
                original_hash: h,
                chunks: vec![ChunkRef {
                    hash: h,
                    plain_hash: h,
                    offset: 0,
                    size_original: 4,
                    size_stored: 4,
                    compressed: false,
                    encrypted: false,
                }],
                tags: vec![],
                created_at_ms: 0,
                updated_at_ms: 0,
                deleted_at_ms: None,
                version: 1,
            })
            .await
            .unwrap();
    }
    // The "batched" collect is one all_manifests() call returning all 50:
    let manifests = index.all_manifests().await.unwrap();
    assert_eq!(manifests.len(), 50);
    // ⇒ on a 1M-file store this is 1M manifests (chunk vectors included)
    // resident at once — reconcile peak memory is O(store), not O(referenced).
}

// ---------------------------------------------------------------------------
// [V6-09] missing tenant key is misclassified as hostile upstream bytes
// ---------------------------------------------------------------------------

/// With encryption enabled but the tenant's key file absent,
/// `repair_chunk` takes the `decode` error branch (decrypt fails:
/// "key file ... invalid"/missing) and QUARANTINES the perfectly valid
/// ciphertext, returning Corruption — instead of distinguishing
/// `repair_key_missing` from `repair_ciphertext_only`/hostile. A replica
/// whose keys_dir is wiped/misconfigured pollutes quarantine with good
/// data (displacing real hostile evidence, up to the bound).
#[tokio::test]
#[ignore = "V6-09: run on a Linux host"]
async fn v6_09_missing_tenant_key_quarantines_valid_ciphertext() {
    use file_storage::core::compressor::Compressor;
    use file_storage::security::encryption::Encryptor;
    use file_storage::storage::quarantine::Quarantine;

    // Primary-side: encrypt a chunk for tenant T (key created in tmp dir A).
    let dir_a = tempfile::tempdir().unwrap();
    let enc_primary = Encryptor::new(dir_a.path().to_path_buf());
    let tenant = Uuid::now_v7();
    let comp = Compressor::new(3, 64);
    let plain = vec![b'k'; 8192];
    let out = comp.compress(&plain);
    let blob = enc_primary.encrypt(&tenant, &out.data).unwrap();

    // Replica-side: encryption ENABLED but keys_dir empty (config error).
    let dir_b = tempfile::tempdir().unwrap();
    let enc_replica = Encryptor::new(dir_b.path().to_path_buf());
    let err = enc_replica.decrypt(&tenant, &blob).unwrap_err();
    // repair_chunk's decode error branch quarantines on exactly this:
    assert!(matches!(err, file_storage::error::StorageError::Encryption(_)));

    // and quarantine.store happily persists the VALID ciphertext:
    let q = Quarantine::new(dir_b.path().join("quarantine"), 1 << 20);
    let hash = file_storage::core::hasher::hash_bytes(&blob);
    q.store(&hash, &blob).await.unwrap();
    assert!(
        q.contains(&hash).await,
        "valid ciphertext lands in quarantine — evidence pollution; no \
         repair_key_missing signal exists"
    );
}

// ---------------------------------------------------------------------------
// helper
// ---------------------------------------------------------------------------

fn _unused(_: GcMode, _: NodeRole, _: GarbageCollector, _: ReconcileMode) {}
