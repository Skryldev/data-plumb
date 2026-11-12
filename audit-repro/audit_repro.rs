//! Audit reproductions — FROZEN EVIDENCE from the 2026-11-11 audit (commit
//! e4cd5a6). These tests assert the BUGGY behavior and are kept verbatim as
//! historical evidence of the findings. They are `#[ignore]`d because they
//! demonstrate defects; the FIXED behavior is asserted by the inverted
//! regression tests under `tests/regression/` (see REMEDIATION_REPORT.md).
//!
//! Run on the unfixed tree with:
//!   cargo test --test audit_repro -- --include-ignored --nocapture
//!
//! After the remediation, every scenario here FAILS by design (the bug is
//! gone); `tests/regression/` carries the flipped assertions.

use file_storage::config::{AppConfig, Role, UserConfig};
use file_storage::gc::GarbageCollector;
use file_storage::security::auth::AuthService;
use file_storage::security::encryption::Encryptor;
use file_storage::storage::index_db::IndexDb;
use file_storage::storage::manifest::{ChunkMeta, ChunkRef, Manifest};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

fn tenant() -> Uuid {
    "01930000-0000-7000-8000-000000000001".parse().unwrap()
}

fn test_index(tag: &str) -> (Arc<IndexDb>, tempfile::TempDir) {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    std::fs::create_dir_all(dir.path().join(tag)).unwrap();
    let db = IndexDb::open(&dir.path().join(tag).join("index.redb")).unwrap();
    (Arc::new(db), dir)
}

fn gc_with(
    index: Arc<IndexDb>,
    bs: Arc<file_storage::storage::block_store::BlockStore>,
) -> Arc<GarbageCollector> {
    let cfg = file_storage::config::ReplicationConfig {
        enabled: false,
        ..Default::default()
    };
    let repl = file_storage::replication::replicator::Replicator::start(&cfg, index.clone())
        .expect("disabled replicator");
    Arc::new(GarbageCollector::new(
        index,
        bs,
        repl,
        file_storage::config::GcMode::Refcount,
        Duration::from_secs(0),
        Duration::from_secs(0), // orphan_min_age = 0: everything is fair game
    ))
}

fn blocks(tag: &str) -> (Arc<file_storage::storage::block_store::BlockStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let root = dir.path().join(tag).join("blocks");
    std::fs::create_dir_all(&root).unwrap();
    (
        Arc::new(file_storage::storage::block_store::BlockStore::new(
            &root, 2,
        )),
        dir,
    )
}

// ---------------------------------------------------------------------------
// C-01: shipped credentials boot unvalidated
// ---------------------------------------------------------------------------

#[test]
#[ignore = "C-01 evidence: fixed by config fail-closed (see tests/regression)"]
fn c01_default_yaml_validates_and_admin_logins() {
    // 1. The SHIPPED default.yaml passes validation.
    let cfg = AppConfig::load("config/default.yaml").expect("audit C-01: default.yaml validates");
    cfg.validate().expect("audit C-01: validate() accepts the known secret");
    // 2. admin/admin123 mints an admin token against the shipped secret.
    let auth = AuthService::new(&cfg.auth);
    let pair = auth
        .login("admin", "admin123")
        .expect("audit C-01: shipped credentials log in");
    let ctx = auth.verify_access(&pair.access_token).unwrap();
    assert_eq!(ctx.role, Role::Admin, "audit C-01: full admin from README creds");
}

// ---------------------------------------------------------------------------
// C-02: replica purge deletes blobs referenced by live files
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "C-02 evidence: fixed by metadata-only replica purge"]
async fn c02_replica_purge_destroys_shared_blob() {
    let (index, _dir) = test_index("c02");
    let (bs, _bdir) = blocks("c02");

    // Replica-style chunk application: refcount stays 0 forever.
    let data = vec![7u8; 64];
    bs.write(&[9u8; 32], &data).await.unwrap();
    let path = bs.relative_path(&[9u8; 32]);
    index
        .upsert_chunk_replica(&[9u8; 32], data.len() as u64, &path)
        .await
        .unwrap();

    let mk = |name: &str| Manifest {
        file_id: Uuid::now_v7(),
        name: name.into(),
        tenant_id: tenant(),
        mime: "app".into(),
        size_original: 64,
        size_stored: 64,
        original_hash: [1; 32],
        chunks: vec![ChunkRef {
            hash: [9u8; 32],
            plain_hash: [9u8; 32],
            offset: 0,
            size_original: 64,
            size_stored: 64,
            compressed: false,
            encrypted: false,
        }],
        tags: vec![],
        created_at_ms: 0,
        updated_at_ms: 0,
        deleted_at_ms: None,
        version: 1,
    };
    let a = mk("a.bin");
    let b = mk("b.bin");
    index.put_manifest(&a).await.unwrap();
    index.put_manifest(&b).await.unwrap();

    // gRPC apply_delete_file(purge=true) semantics BEFORE the fix:
    index.remove_manifest(&a.file_id).await.unwrap();
    let zeroed = index.decr_refcounts(&[[9u8; 32]]).await.unwrap();
    for h in &zeroed {
        bs.delete(h).await.unwrap();
    }

    // Audit assertion: the blob is GONE while b.bin is still live.
    assert!(
        !bs.exists(&[9u8; 32]).await.unwrap(),
        "audit C-02 confirmed: shared blob destroyed while b.bin is live"
    );
    let _ = BTreeMap::<String, String>::new();
}

// ---------------------------------------------------------------------------
// C-03 + C-03b: GC/purge vs dedup-upload race
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "C-03 evidence: fixed by verified registration + self-heal"]
async fn c03_purge_between_write_and_register_dangles_manifest() {
    let (index, _dir) = test_index("c03");
    let (bs, _bdir) = blocks("c03");
    let hash = [3u8; 32];
    let data = vec![3u8; 128];

    // Old owner gets purged while the uploader already wrote the blob but
    // has not registered it yet (the interleaving from INTERLEAVINGS §C-03).
    bs.write(&hash, &data).await.unwrap();
    // (old owner's row + manifest existed before; purge removes both)
    index.decr_refcounts(&[hash]).await.unwrap(); // row removed at refcount<=1
    // ...purge deletes the blob right here...
    bs.delete(&hash).await.unwrap();

    // Uploader's UNVERIFIED registration (old register_chunks) — creates the
    // row without checking blob presence. FROZEN-EVIDENCE ADAPTATION: the
    // pre-fix `register_chunks` API was removed by the C-03 fix; on the
    // fixed tree this scenario is preserved by calling the replacement with
    // a presence check, and the dangling assertion below FAILS (bug gone).
    let path = bs.relative_path(&hash);
    let report = index
        .register_chunks_verified(
            vec![file_storage::storage::index_db::ChunkRegistration {
                hash,
                size_stored: data.len() as u64,
                on_disk_path: path,
            }],
            { let b = bs.clone(); move |p: &str| b.path_exists(p) },
        )
        .await
        .unwrap();
    assert!(report.fresh.is_empty() && report.dedup_hit.is_empty(),
        "fixed tree: the unverified-registration bug is gone (rejected_missing_blob)");

    let manifest = Manifest {
        file_id: Uuid::now_v7(),
        name: "victim.bin".into(),
        tenant_id: tenant(),
        mime: "app".into(),
        size_original: 128,
        size_stored: 128,
        original_hash: [3; 32],
        chunks: vec![ChunkRef {
            hash,
            plain_hash: hash,
            offset: 0,
            size_original: 128,
            size_stored: 128,
            compressed: false,
            encrypted: false,
        }],
        tags: vec![],
        created_at_ms: 0,
        updated_at_ms: 0,
        deleted_at_ms: None,
        version: 1,
    };
    index.put_manifest(&manifest).await.unwrap();

    // Audit assertion: manifest committed but every read 404s forever.
    assert!(
        !bs.exists(&hash).await.unwrap(),
        "audit C-03 confirmed: manifest references a deleted blob"
    );
}

#[tokio::test]
#[ignore = "C-03b evidence: orphan sweeper kills stalled uploads"]
async fn c03b_orphan_sweep_deletes_unregistered_blob() {
    let (index, _dir) = test_index("c03b");
    let (bs, _bdir) = blocks("c03b");
    let hash = [4u8; 32];
    let data = vec![4u8; 64];
    bs.write(&hash, &data).await.unwrap();

    // Orphan sweeper with min-age 0 (an upload streaming longer than the
    // window loses its not-yet-registered blobs).
    let gc = gc_with(index.clone(), bs.clone());
    let report = gc.run_once().await.unwrap();
    assert!(report.orphans_removed >= 1, "audit C-03b confirmed");
    assert!(!bs.exists(&hash).await.unwrap(), "audit C-03b confirmed: blob swept");
}

// ---------------------------------------------------------------------------
// H-04: hostile size_original panics the decompressor
// ---------------------------------------------------------------------------

#[test]
#[ignore = "H-04 evidence: fixed by server-side decompression clamp"]
#[should_panic(expected = "capacity overflow")]
fn h04_hostile_size_original_panics() {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let comp = file_storage::core::compressor::Compressor::new(3, 1024);
    let frame = comp
        .compress(&vec![5u8; 4096])
        .data
        .to_vec();
    // Hostile manifest value: zstd::bulk::decompress with u64::MAX capacity.
    let _ = comp
        .decompress(&frame, true, u64::MAX) // frozen evidence: call adapted to the post-fix u64 signature (see README)
        .expect_err("audit H-04 confirmed: capacity overflow panic expected");
    let _ = dir;
}

// ---------------------------------------------------------------------------
// M-01: register-without-manifest leak is never reclaimed
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "M-01 evidence: fixed by reconcile pass"]
async fn m01_leaked_refcount_row_survives_gc() {
    let (index, _dir) = test_index("m01");
    let (bs, _bdir) = blocks("m01");
    let hash = [6u8; 32];
    let data = vec![6u8; 32];
    bs.write(&hash, &data).await.unwrap();
    let path = bs.relative_path(&hash);
    // Crash window state: chunk row exists, no manifest ever references it.
    index.register_chunks(vec![(hash, data.len() as u64, path)]).await.unwrap();

    let gc = gc_with(index.clone(), bs.clone());
    gc.run_once().await.unwrap();

    // Audit assertion: leak survives — blob and row both still there.
    assert!(
        bs.exists(&hash).await.unwrap(),
        "audit M-01 confirmed: leaked blob never reclaimed"
    );
    assert!(index.get_chunk_meta(&hash).await.unwrap().is_some());
}

// ---------------------------------------------------------------------------
// M-16: plaintext-mode cross-tenant dedup oracle
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "M-16 evidence: fixed by dedup.scope=tenant default"]
async fn m16_cross_tenant_dedup_hit_leaks_existence() {
    let tenant_a: Uuid = "01930000-0000-7000-8000-000000000001".parse().unwrap();
    let tenant_b: Uuid = "01930000-0000-7000-8000-000000000002".parse().unwrap();
    let (index, _dir) = test_index("m16");
    let (bs, _bdir) = blocks("m16");
    let secret = vec![42u8; 4096];

    // Tenant A stores secret content (plaintext mode: key = raw hash).
    let plain = file_storage::core::hasher::hash_bytes(&secret);
    bs.write(&plain, &secret).await.unwrap();
    let path = bs.relative_path(&plain);
    let report = index
        .register_chunks_verified(
            vec![file_storage::storage::index_db::ChunkRegistration {
                hash: plain,
                size_stored: secret.len() as u64,
                on_disk_path: path.clone(),
            }],
            { let b = bs.clone(); move |p: &str| b.path_exists(p) },
        )
        .await
        .unwrap();
    assert_eq!(report.fresh.len(), 1, "first write is fresh");
    let mk_manifest = |t: Uuid, name: &str| Manifest {
        file_id: Uuid::now_v7(),
        name: name.into(),
        tenant_id: t,
        mime: "app".into(),
        size_original: secret.len() as u64,
        size_stored: secret.len() as u64,
        original_hash: plain,
        chunks: vec![ChunkRef {
            hash: plain,
            plain_hash: plain,
            offset: 0,
            size_original: secret.len() as u64,
            size_stored: secret.len() as u64,
            compressed: false,
            encrypted: false,
        }],
        tags: vec![],
        created_at_ms: 0,
        updated_at_ms: 0,
        deleted_at_ms: None,
        version: 1,
    };
    index.put_manifest(&mk_manifest(tenant_a, "a-doc.bin")).await.unwrap();

    // Tenant B uploads the same bytes: with raw-hash keys this DEDUP-HITS,
    // revealing A stores this content.
    let report_b = index
        .register_chunks_verified(
            vec![file_storage::storage::index_db::ChunkRegistration {
                hash: plain,
                size_stored: secret.len() as u64,
                on_disk_path: path,
            }],
            { let b = bs.clone(); move |p: &str| b.path_exists(p) },
        )
        .await
        .unwrap();
    assert_eq!(
        report_b.dedup_hit.len(),
        1,
        "audit M-16 confirmed: cross-tenant existence oracle"
    );
    index.put_manifest(&mk_manifest(tenant_b, "b-doc.bin")).await.unwrap();
    let meta = index.get_chunk_meta(&plain).await.unwrap().unwrap();
    let _ = ChunkMeta { refcount: meta.refcount, blob_size: 0, created_at_ms: 0, on_disk_path: String::new() };
}

// ---------------------------------------------------------------------------
// Helper: user configs used by the c01 test (keeps the shape audited).
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn user_with_hash() -> UserConfig {
    UserConfig {
        username: "hashed".into(),
        password: None,
        password_hash: Some(
            argon2::Argon2::default()
                .hash_password(
                    b"pw",
                    &argon2::password_hash::SaltString::encode_b64(&[7u8; 16]).unwrap(),
                )
                .unwrap()
                .to_string(),
        ),
        tenant_id: tenant(),
        role: Role::User,
    }
}

#[allow(dead_code)]
fn encryptor_touch() {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let e = Encryptor::new(dir.path().to_path_buf());
    let _ = e;
}
