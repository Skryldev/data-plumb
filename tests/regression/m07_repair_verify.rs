//! M-07 regression gates — verify-then-persist on the replica repair
//! path, quarantine, and the ciphertext-only degraded posture.
//!
//! These tests INVERT the audit's M-07 finding: a hostile/compromised
//! upstream can no longer poison a replica. Untrusted bytes never reach
//! the CAS blob path or L2 without first hashing to the chunk
//! reference's `plain_hash`; mismatches are quarantined + alarmed.
//!
//! The upstream is a mock HTTP server speaking the real replication wire
//! protocol (`GET /internal/chunk/:hash`), so the tests run against the
//! real `RetrievalEngine` / `Replicator` stack.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::RwLock;

use file_storage::config::AppConfig;
use file_storage::security::encryption::Encryptor;
use file_storage::state::AppState;
use file_storage::storage::manifest::ChunkRef;

const TENANT: &str = "01930000-0000-7000-8000-000000000001";

// -- mock upstream ------------------------------------------------------------

/// Serves `GET /internal/chunk/:hash` from a test-controlled map — the
/// same endpoint `ReplicationClient::fetch_chunk` calls on a real primary.
struct MockUpstream {
    addr: String,
    served: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    fetches: Arc<AtomicUsize>,
}

async fn spawn_mock_upstream() -> MockUpstream {
    let served: Arc<RwLock<HashMap<String, Vec<u8>>>> = Arc::new(RwLock::new(HashMap::new()));
    let fetches = Arc::new(AtomicUsize::new(0));
    let served_rt = served.clone();
    let fetches_rt = fetches.clone();

    let app = axum::Router::new().route(
        "/internal/chunk/:hash",
        axum::routing::get(move |extractor: axum::extract::Path<String>| {
            let served = served_rt.clone();
            let fetches = fetches_rt.clone();
            async move {
                let hash = extractor.0;
                fetches.fetch_add(1, Ordering::Relaxed);
                match served.read().await.get(&hash) {
                    Some(b) => axum::response::Response::builder()
                        .status(200)
                        .body(axum::body::Body::from(b.clone()))
                        .unwrap(),
                    None => axum::response::Response::builder()
                        .status(404)
                        .body(axum::body::Body::from("not found"))
                        .unwrap(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    MockUpstream {
        addr: format!("http://{addr}"),
        served,
        fetches,
    }
}

// -- fixtures -----------------------------------------------------------------

fn replica_config(upstream: &str, root: &std::path::Path, encryption: bool) -> AppConfig {
    let mut cfg = AppConfig::for_tests();
    cfg = cfg.with_data_root(root);
    cfg.replication.enabled = true;
    cfg.replication.upstream_url = Some(upstream.to_string());
    cfg.replication.api_key = "test-internal-key-0123456789abcdef".into();
    cfg.encryption.enabled = encryption;
    cfg
}

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

fn tenant() -> uuid::Uuid {
    TENANT.parse().unwrap()
}

/// Extracts `name=value` from a rendered Prometheus exposition for a
/// metric whose label set is embedded in `label_substr`.
fn metric_value(render: &str, metric: &str, label_substr: &str) -> f64 {
    render
        .lines()
        .filter(|l| l.starts_with(metric) && l.contains(label_substr))
        .filter_map(|l| l.rsplit(' ').next())
        .find_map(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Uploads `data` through the REAL pipeline and returns the manifest.
async fn upload(
    state: &AppState,
    data: Vec<u8>,
    name: &str,
) -> file_storage::storage::manifest::Manifest {
    let stream = futures::stream::iter(vec![Ok(Bytes::from(data))]);
    state
        .0
        .pipeline
        .process(tenant(), name, "application/octet-stream", vec![], stream)
        .await
        .unwrap()
        .manifest
}

fn blob_path(state: &AppState, hash: &[u8; 32]) -> std::path::PathBuf {
    let (_, path) = state.0.blocks.shard_path(hash);
    path
}

// -- gate 1: repair_rejects_lying_upstream -------------------------------------

#[tokio::test]
async fn repair_rejects_lying_upstream() {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let mock = spawn_mock_upstream().await;
    let cfg = replica_config(&mock.addr, dir.path(), false);
    let state = file_storage::state::build_state(Arc::new(cfg))
        .await
        .unwrap();
    file_storage::metrics::init_recorder();

    // 1. Upload honest content, then destroy the local blob (simulates a
    //    lost/damaged replica).
    let data = pseudo_random(300_000, 7);
    let manifest = upload(&state, data.clone(), "victim.bin").await;
    let cr = manifest.chunks[0].clone();
    assert!(!cr.encrypted);
    assert!(
        !cr.compressed,
        "random data stays uncompressed (deterministic decode path)"
    );
    let path = blob_path(&state, &cr.hash);
    assert!(path.exists(), "blob exists after upload");
    let honest_blob = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    // 2. The upstream LIES: it serves decodable, size-consistent bytes
    //    with DIFFERENT content — the hash-mismatch branch fires (not the
    //    undecodable branch).
    let hostile = pseudo_random(cr.size_original as usize, 99);
    mock.served
        .write()
        .await
        .insert(hex::encode(cr.hash), hostile.clone());

    // 3. Repair attempt must FAIL with Corruption and never persist.
    let err = state
        .0
        .retrieval
        .load_plain(&tenant(), &cr)
        .await
        .unwrap_err();
    assert!(
        matches!(err, file_storage::error::StorageError::Corruption(_)),
        "caller must see Corruption, got {err:?}"
    );
    assert!(err.to_string().contains("repair verification failed"));
    assert!(
        mock.fetches.load(Ordering::Relaxed) >= 1,
        "upstream was consulted"
    );

    // 6. Quarantine holds the hostile bytes for ops inspection.
    let qdir = dir.path().join("quarantine");
    let qfile = qdir.join(format!("{}.bin", hex::encode(cr.hash)));
    let quarantined = std::fs::read(&qfile).expect("quarantine file exists");
    assert_eq!(
        quarantined, hostile,
        "quarantined bytes are the hostile ones"
    );

    // 3b. Garbage (undecodable) bytes are equally unverifiable — same
    //     refuse-to-persist outcome. (Content-addressed quarantine: this
    //     overwrites the entry above, hence the earlier content assert.)
    let garbage = vec![0xDEu8; honest_blob.len() + 11];
    mock.served
        .write()
        .await
        .insert(hex::encode(cr.hash), garbage);
    let err = state
        .0
        .retrieval
        .load_plain(&tenant(), &cr)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        file_storage::error::StorageError::Corruption(_)
    ));
    assert!(
        !path.exists(),
        "undecodable bytes must not be persisted either"
    );

    // 4. Disk NOT polluted: the hostile bytes never landed in the CAS.
    assert!(
        !path.exists(),
        "hostile bytes must not be persisted to the CAS"
    );

    // 5. L2 empty for that hash.
    assert!(
        !state.0.cache.chunks.peek_blob(&cr.hash),
        "L2 must not hold hostile bytes"
    );

    // 7. Alarm metric ticked with source="repair".
    let render = file_storage::metrics::render();
    let v = metric_value(&render, "file_storage_repair_hash_mismatch_total", "repair");
    assert!(
        v >= 1.0,
        "repair_hash_mismatch metric must tick, render:\n{render}"
    );
}

// -- gate 2: repair_persists_verified_bytes ------------------------------------

#[tokio::test]
async fn repair_persists_verified_bytes() {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let mock = spawn_mock_upstream().await;
    // Encryption ON: the keyed path — the replica holds the tenant key.
    let cfg = replica_config(&mock.addr, dir.path(), true);
    let state = file_storage::state::build_state(Arc::new(cfg))
        .await
        .unwrap();
    file_storage::metrics::init_recorder();

    let data = pseudo_random(300_000, 11);
    let manifest = upload(&state, data.clone(), "honest.bin").await;
    let cr = manifest.chunks[0].clone();
    let path = blob_path(&state, &cr.hash);
    let honest_blob = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    // Upstream is HONEST: serves the original stored-form bytes.
    mock.served
        .write()
        .await
        .insert(hex::encode(cr.hash), honest_blob.clone());

    // Repair succeeds and the caller gets the plaintext OF THIS CHUNK
    // (manifest.chunks[0] is one chunk of the multi-chunk file).
    let plain = state.0.retrieval.load_plain(&tenant(), &cr).await.unwrap();
    let expected = &data[cr.offset as usize..(cr.offset + cr.size_original) as usize];
    assert_eq!(plain.as_ref(), expected, "chunk plaintext round-trips");

    // Blob restored at the CAS path, byte-identical stored form.
    let restored = std::fs::read(&path).unwrap();
    assert_eq!(restored, honest_blob);

    // L2 populated; nothing quarantined; the mismatch alarm does NOT move.
    // (The counter is process-global, so capture-before/compare-after
    // rather than requiring zero.)
    let mismatch_before = metric_value(
        &file_storage::metrics::render(),
        "file_storage_repair_hash_mismatch_total",
        "repair",
    );
    assert!(
        state.0.cache.chunks.peek_blob(&cr.hash),
        "L2 holds the verified blob"
    );
    let qfile = dir
        .path()
        .join("quarantine")
        .join(format!("{}.bin", hex::encode(cr.hash)));
    assert!(!qfile.exists(), "honest repair must not quarantine");

    // Re-run the load from DISK again (L2 invalidated path is irrelevant;
    // a second honest load must stay quiet).
    std::fs::remove_file(&path).unwrap();
    mock.served
        .write()
        .await
        .insert(hex::encode(cr.hash), honest_blob.clone());
    let plain2 = state.0.retrieval.load_plain(&tenant(), &cr).await.unwrap();
    assert_eq!(plain2.as_ref(), expected);

    let mismatch_after = metric_value(
        &file_storage::metrics::render(),
        "file_storage_repair_hash_mismatch_total",
        "repair",
    );
    assert_eq!(
        mismatch_before, mismatch_after,
        "honest repair must not tick the mismatch alarm"
    );
}

// -- gate 3: repair_ciphertext_only_logs_and_metrics ---------------------------

#[tokio::test]
async fn repair_ciphertext_only_logs_and_metrics() {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let mock = spawn_mock_upstream().await;

    // Produce an encrypted chunk with a throwaway encryptor (the "primary").
    let keys_dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let enc = Encryptor::new(keys_dir.path().to_path_buf());
    let plain = pseudo_random(50_000, 13);
    let blob = enc.encrypt(&tenant(), &plain).unwrap();
    let plain_hash = *blake3::hash(&plain).as_bytes();
    let storage_hash = Encryptor::storage_key(&tenant(), &plain_hash);
    let cr = ChunkRef {
        hash: storage_hash,
        plain_hash,
        offset: 0,
        size_original: plain.len() as u64,
        size_stored: blob.len() as u64,
        compressed: false,
        encrypted: true,
    };

    // The replica has NO tenant key (encryption disabled) but an upstream.
    let cfg = replica_config(&mock.addr, dir.path(), false);
    let state = file_storage::state::build_state(Arc::new(cfg))
        .await
        .unwrap();
    file_storage::metrics::init_recorder();
    mock.served
        .write()
        .await
        .insert(hex::encode(cr.hash), blob.clone());

    // Legacy order: the blob IS persisted (cannot verify without the key),
    // the caller still cannot decrypt, the degraded metric ticks, and
    // quarantine is NOT used (unverifiable ≠ hostile).
    let err = state
        .0
        .retrieval
        .load_plain(&tenant(), &cr)
        .await
        .unwrap_err();
    assert!(
        matches!(err, file_storage::error::StorageError::Encryption(_)),
        "keyless replica must fail to decrypt, got {err:?}"
    );
    let path = blob_path(&state, &cr.hash);
    assert!(
        path.exists(),
        "ciphertext-only repair persists the blob (legacy order)"
    );
    let persisted = std::fs::read(&path).unwrap();
    assert_eq!(persisted, blob, "stored bytes are the upstream ciphertext");

    let render = file_storage::metrics::render();
    let v = metric_value(
        &render,
        "file_storage_repair_ciphertext_only_total",
        "repair_ciphertext_only",
    );
    assert!(
        v >= 1.0,
        "ciphertext-only metric must tick, render:\n{render}"
    );

    let qfile = dir
        .path()
        .join("quarantine")
        .join(format!("{}.bin", hex::encode(cr.hash)));
    assert!(
        !qfile.exists(),
        "ciphertext-only repair does not classify bytes as hostile"
    );
}

// -- gate 4: boot_sweep_quarantine ---------------------------------------------

#[tokio::test]
async fn boot_sweep_quarantine() {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let qdir = dir.path().join("quarantine");
    std::fs::create_dir_all(&qdir).unwrap();

    // Stale evidence (older than the retention window) and fresh evidence.
    let stale = qdir.join(format!("{}.bin", "1".repeat(64)));
    let fresh = qdir.join(format!("{}.bin", "2".repeat(64)));
    std::fs::write(&stale, b"old-hostile").unwrap();
    std::fs::write(&fresh, b"new-hostile").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    {
        use std::fs::FileTimes;
        let f = std::fs::File::options().append(true).open(&stale).unwrap();
        f.set_times(FileTimes::new().set_accessed(old).set_modified(old))
            .unwrap();
    }
    file_storage::metrics::init_recorder();

    // Boot: the sweep runs inside build_state, BEFORE listeners bind.
    // The test window uses a 60s retention (stale file is 1 day old).
    let mut cfg = replica_config("http://127.0.0.1:1", dir.path(), false);
    cfg.storage.quarantine_retention_secs = 60;
    let _state = file_storage::state::build_state(Arc::new(cfg))
        .await
        .unwrap();

    assert!(!stale.exists(), "stale quarantine evidence swept at boot");
    assert!(fresh.exists(), "fresh quarantine evidence survives");
    let render = file_storage::metrics::render();
    let v = metric_value(&render, "file_storage_startup_quarantine_swept", "");
    assert!(
        v >= 1.0,
        "startup_quarantine_swept metric must tick, render:\n{render}"
    );
}
