//! M-01 regression gates — the drift-reconciliation pass.
//!
//! These tests INVERT the audit's M-01 finding: crash-window leaks (blob +
//! refcount row, no manifest) are now RECLAIMED by the periodic/admin
//! reconcile pass. Rule 16 is asserted structurally: dry-run never
//! deletes; repair tombstones-then-sweeps with in-transaction
//! re-verification (the C-03 race defense).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use file_storage::config::{NodeRole, ReplicationConfig};
use file_storage::gc::{GarbageCollector, ReconcileMode};
use file_storage::replication::replicator::Replicator;
use file_storage::storage::block_store::BlockStore;
use file_storage::storage::index_db::{ChunkRegistration, IndexDb};
use file_storage::storage::manifest::{ChunkRef, Manifest};
use uuid::Uuid;

fn tenant() -> Uuid {
    "01930000-0000-7000-8000-000000000001".parse().unwrap()
}

struct Rig {
    index: Arc<IndexDb>,
    blocks: Arc<BlockStore>,
    gc: Arc<GarbageCollector>,
    _dir: tempfile::TempDir,
}

fn rig(orphan_min_age_secs: u64) -> Rig {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let root = dir.path().join("blocks");
    std::fs::create_dir_all(&root).unwrap();
    let index = Arc::new(IndexDb::open(&dir.path().join("index.redb")).unwrap());
    let blocks = Arc::new(BlockStore::new(&root, 2));
    let cfg = ReplicationConfig {
        enabled: false,
        ..Default::default()
    };
    let repl = Replicator::start(&cfg, index.clone()).unwrap();
    let gc = Arc::new(GarbageCollector::new(
        index.clone(),
        blocks.clone(),
        repl,
        file_storage::config::GcMode::Refcount,
        NodeRole::Primary,
        Duration::from_secs(0),
        Duration::from_secs(orphan_min_age_secs),
    ));
    Rig {
        index,
        blocks,
        gc,
        _dir: dir,
    }
}

fn manifest_of(name: &str, hash: [u8; 32], size: u64) -> Manifest {
    Manifest {
        file_id: Uuid::now_v7(),
        name: name.into(),
        tenant_id: tenant(),
        mime: "application/octet-stream".into(),
        size_original: size,
        size_stored: size,
        original_hash: hash,
        chunks: vec![ChunkRef {
            hash,
            plain_hash: hash,
            offset: 0,
            size_original: size,
            size_stored: size,
            compressed: false,
            encrypted: false,
        }],
        tags: vec![],
        created_at_ms: 0,
        updated_at_ms: 0,
        deleted_at_ms: None,
        version: 1,
    }
}

async fn register(rig: &Rig, hash: [u8; 32], size: usize) {
    rig.index
        .register_chunk_verified(
            ChunkRegistration {
                hash,
                size_stored: size as u64,
                on_disk_path: rig.blocks.relative_path(&hash),
            },
            {
                let b = rig.blocks.clone();
                move |p: &str| b.path_exists(p)
            },
        )
        .await
        .unwrap();
}

fn hash_of(seed: u8) -> [u8; 32] {
    file_storage::core::hasher::hash_bytes(&[seed; 64])
}

// -- gate 1: reconcile_reclaims_register_without_manifest ----------------------

#[tokio::test]
async fn reconcile_reclaims_register_without_manifest() {
    let rig = rig(0);
    let hash = hash_of(1);
    let data = vec![1u8; 256];
    // The C-03 crash window's leaked state: blob written, chunk row with
    // refcount 1, NO manifest (process killed before put_manifest).
    rig.blocks.write(&hash, &data).await.unwrap();
    register(&rig, hash, data.len()).await;
    assert!(rig.blocks.exists(&hash).await.unwrap());

    let report = rig.gc.reconcile(ReconcileMode::Repair).await.unwrap();
    assert!(
        report.orphan_chunks >= 1,
        "reconcile must classify the leaked blob as an orphan: {report:?}"
    );
    assert!(report.tombstoned >= 1, "leaked row tombstoned: {report:?}");
    assert!(
        report.reclaimed >= 1,
        "sweep reclaims the tombstoned blob: {report:?}"
    );
    assert!(
        !rig.blocks.exists(&hash).await.unwrap(),
        "leaked blob must be reclaimed"
    );
    let meta = rig.index.get_chunk_meta(&hash).await.unwrap();
    assert!(meta.is_none(), "leaked row must be finalized away");
    file_storage::metrics::init_recorder();
    let render = file_storage::metrics::render();
    assert!(
        render.contains("file_storage_gc_reconcile_leaked_total"),
        "leak metric must be reported"
    );
}

// -- gate 2: reconcile_spares_live_chunks --------------------------------------

#[tokio::test]
async fn reconcile_spares_live_chunks() {
    let rig = rig(0);
    // shared: referenced by TWO manifests; solo: referenced by ONE.
    let shared = hash_of(2);
    let solo = hash_of(3);
    let data = vec![2u8; 128];
    rig.blocks.write(&shared, &data).await.unwrap();
    rig.blocks.write(&solo, &data).await.unwrap();
    register(&rig, shared, data.len()).await;
    register(&rig, solo, data.len()).await;
    let m1 = manifest_of("a.bin", shared, data.len() as u64);
    rig.index.put_manifest(&m1).await.unwrap();
    let m2 = {
        let mut m = manifest_of("b.bin", shared, data.len() as u64);
        m.chunks.push(ChunkRef {
            hash: solo,
            plain_hash: solo,
            offset: data.len() as u64,
            size_original: data.len() as u64,
            size_stored: data.len() as u64,
            compressed: false,
            encrypted: false,
        });
        m
    };
    rig.index.put_manifest(&m2).await.unwrap();

    for mode in [ReconcileMode::DryRun, ReconcileMode::Repair] {
        let report = rig.gc.reconcile(mode).await.unwrap();
        assert_eq!(
            report.orphan_chunks, 0,
            "live chunks are not orphans ({mode:?})"
        );
        assert_eq!(
            report.tombstoned, 0,
            "referenced rows are never tombstoned ({mode:?})"
        );
    }
    assert!(rig.blocks.exists(&shared).await.unwrap());
    assert!(rig.blocks.exists(&solo).await.unwrap());
}

// -- gate 3: reconcile_dry_run_never_deletes -----------------------------------

#[tokio::test]
async fn reconcile_dry_run_never_deletes() {
    let rig = rig(0);
    let hash = hash_of(4);
    let data = vec![3u8; 128];
    rig.blocks.write(&hash, &data).await.unwrap();
    register(&rig, hash, data.len()).await;

    let report = rig.gc.reconcile(ReconcileMode::DryRun).await.unwrap();
    assert!(
        report.orphan_chunks >= 1,
        "dry-run still REPORTS the orphan"
    );
    assert_eq!(report.tombstoned, 0, "dry-run never tombstones (rule 16)");
    assert_eq!(report.reclaimed, 0, "dry-run never deletes");
    // Blob and row untouched.
    assert!(
        rig.blocks.exists(&hash).await.unwrap(),
        "blob survives dry-run"
    );
    let meta = rig.index.get_chunk_meta(&hash).await.unwrap();
    assert!(meta.is_some(), "row survives dry-run");
    assert_eq!(meta.unwrap().refcount, 1, "refcount untouched by dry-run");
}

// -- gate 4: reconcile_in_txn_reverification_blocks_c03_race -------------------

#[tokio::test]
async fn reconcile_in_txn_reverification_blocks_c03_race() {
    let rig = rig(0);
    let hash = hash_of(5);
    let data = vec![4u8; 128];
    rig.blocks.write(&hash, &data).await.unwrap();
    register(&rig, hash, data.len()).await;

    // The caller's SNAPSHOT (taken before the manifest commits) says the
    // chunk is unreferenced...
    let mut referenced: HashSet<[u8; 32]> = HashSet::new();
    rig.index
        .collect_referenced_hashes(&mut referenced)
        .await
        .unwrap();
    assert!(!referenced.contains(&hash));

    // ...then a manifest COMMITS (the C-03 race: manifest lands between
    // snapshot and tombstone).
    let m = manifest_of("raced.bin", hash, data.len() as u64);
    rig.index.put_manifest(&m).await.unwrap();

    // The tombstone re-walks every manifest INSIDE its own write txn, so
    // the now-referenced chunk must NOT be tombstoned.
    let tombstoned = rig
        .index
        .tombstone_unreferenced_chunks(&referenced)
        .await
        .unwrap();
    assert!(
        tombstoned.is_empty(),
        "in-txn re-check must see the new manifest"
    );
    let meta = rig.index.get_chunk_meta(&hash).await.unwrap();
    assert_eq!(
        meta.unwrap().refcount,
        1,
        "referenced row keeps its refcount"
    );
    assert!(rig.blocks.exists(&hash).await.unwrap());

    // And a Repair reconcile after the fact reclaims nothing of the live
    // chunk.
    let report = rig.gc.reconcile(ReconcileMode::Repair).await.unwrap();
    assert_eq!(report.tombstoned, 0);
    assert!(rig.blocks.exists(&hash).await.unwrap());
}

// -- gate 5: reconcile_lock_excludes_concurrent_gc ------------------------------

#[tokio::test]
async fn reconcile_lock_excludes_concurrent_gc() {
    let rig = rig(0);
    // Seed a leaked chunk so the reconcile has real work to do.
    let hash = hash_of(6);
    let data = vec![5u8; 128];
    rig.blocks.write(&hash, &data).await.unwrap();
    register(&rig, hash, data.len()).await;

    // Start a reconcile in the background; while it holds the in-process
    // busy mutex, a concurrent GC sweep must BLOCK until it finishes
    // (design choice recorded: in-process callers queue; cross-process
    // callers get Unavailable from the flock).
    let gc = rig.gc.clone();
    let reconciling = tokio::spawn(async move { gc.reconcile(ReconcileMode::Repair).await });

    // Give the reconcile a moment to take the lock, then run a sweep.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sweep = rig.gc.clone();
    let sweeping = tokio::spawn(async move { sweep.run_once().await });

    let (r1, r2) = tokio::join!(reconciling, sweeping);
    r1.unwrap().unwrap();
    r2.unwrap()
        .expect("concurrent GC must complete once the reconcile releases the lock");
}

// -- gate 6: reconcile_admin_trigger_audited ------------------------------------

#[tokio::test]
async fn reconcile_admin_trigger_audited() {
    file_storage::metrics::init_recorder();
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let mut cfg = file_storage::config::AppConfig::for_tests();
    cfg = cfg.with_data_root(dir.path());
    let state = file_storage::state::build_state(Arc::new(cfg))
        .await
        .unwrap();
    let app = file_storage::api::router::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Non-admin (viewer) is refused on the REST trigger (M-04a parity).
    async fn login_at(client: &reqwest::Client, base: &str, u: &str, p: &str) -> reqwest::Response {
        client
            .post(format!("{base}/api/v1/auth/login"))
            .json(&serde_json::json!({"username": u, "password": p}))
            .send()
            .await
            .unwrap()
    }
    let vtok: serde_json::Value = login_at(&client, &base, "viewer", "viewer123")
        .await
        .json()
        .await
        .unwrap();
    let vtok = vtok["access_token"].as_str().unwrap().to_string();
    let denied = client
        .post(format!("{base}/api/v1/admin/gc"))
        .query(&[("reconcile", "true")])
        .header("Authorization", format!("Bearer {vtok}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        denied.status().as_u16(),
        403,
        "non-admin reconcile is refused"
    );

    // Admin trigger is audited with actor + mode.
    let atok: serde_json::Value = login_at(&client, &base, "admin", "admin123")
        .await
        .json()
        .await
        .unwrap();
    let atok = atok["access_token"].as_str().unwrap().to_string();
    let ok = client
        .post(format!("{base}/api/v1/admin/gc"))
        .query(&[("reconcile", "true"), ("dry_run", "true")])
        .header("Authorization", format!("Bearer {atok}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok.status().as_u16(),
        200,
        "admin dry-run reconcile succeeds"
    );
    let body: serde_json::Value = ok.json().await.unwrap();
    assert_eq!(
        body["tombstoned"].as_u64().unwrap(),
        0,
        "dry-run tombstones nothing"
    );
    assert!(
        body["scanned_blobs"].as_u64().is_some(),
        "report fields present"
    );

    let events = state.0.audit.recent(100).await.unwrap();
    let gc_rows: Vec<_> = events
        .iter()
        .filter(|e| {
            e.action == "storage.gc" && e.actor == "admin" && e.detail.contains("reconcile")
        })
        .collect();
    assert_eq!(gc_rows.len(), 1, "exactly one audited reconcile trigger");

    // gRPC parity: admin can trigger the reconcile through the SDK and the
    // report round-trips over the wire (additive GCResult fields).
    let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gaddr = grpc_listener.local_addr().unwrap();
    let server = file_storage::grpc::server::GrpcServer::new(state.clone(), None);
    tokio::spawn(async move {
        server.serve_on(grpc_listener).await.expect("grpc server");
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    // One live upload so the reconcile report has real content to scan.
    let stream = futures::stream::iter(vec![Ok(bytes::Bytes::from(vec![8u8; 4096]))]);
    state
        .0
        .pipeline
        .process(
            "01930000-0000-7000-8000-000000000001".parse().unwrap(),
            "reconcile-probe.bin",
            "application/octet-stream",
            vec![],
            stream,
        )
        .await
        .unwrap();
    use file_storage::grpc::client::FileStorageClient;
    let mut c = FileStorageClient::connect(&format!("http://{gaddr}"))
        .await
        .unwrap();
    c.login("admin", "admin123").await.unwrap();
    let res = c.trigger_reconcile(true).await.unwrap();
    assert_eq!(res.tombstoned, 0);
    assert!(res.scanned_blobs >= 1, "gRPC reconcile report populated");
    assert_eq!(res.orphan_chunks, 0, "live upload is not an orphan");
}
