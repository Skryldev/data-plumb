//! C-02 regression: replica deletes are metadata-only; blobs are reclaimed
//! exclusively by reachability GC; cross-protocol purge parity.

use file_storage::config::{AppConfig, GcMode, NodeRole};
use file_storage::grpc::generated::pb;
use file_storage::grpc::generated::ReplicationServiceClient;
use file_storage::grpc::server::GrpcServer;
use file_storage::state::AppState;
use futures::StreamExt as _;
use std::sync::Arc;
use std::time::Duration;
use tonic::metadata::AsciiMetadataValue;
use tonic::transport::Channel;
use tonic::Request;
use uuid::Uuid;

fn tenant() -> Uuid {
    "01930000-0000-7000-8000-000000000001".parse().unwrap()
}

/// Spawns a replica-role node (gRPC + REST) on ephemeral ports.
async fn spawn_replica() -> (String, String, AppState, tempfile::TempDir) {
    file_storage::metrics::init_recorder();
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let mut cfg = AppConfig::for_tests();
    cfg = cfg.with_data_root(dir.path());
    cfg.replication.role = NodeRole::Replica;
    cfg.retention.gc_mode = GcMode::Reachability;
    cfg.retention.orphan_min_age_secs = 0;
    cfg.retention.grace_period_secs = 0;
    let state = file_storage::state::build_state(Arc::new(cfg))
        .await
        .unwrap();

    let app = file_storage::api::router::build_router(state.clone());
    let rest_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            rest_listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();
    let server = GrpcServer::new(state.clone(), None);
    tokio::spawn(async move {
        server.serve_on(grpc_listener).await.expect("grpc server");
    });
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(grpc_addr).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (
        format!("http://{rest_addr}"),
        format!("http://{grpc_addr}"),
        state,
        dir,
    )
}

fn manifest_with(
    name: &str,
    hash: [u8; 32],
    size: u64,
) -> file_storage::storage::manifest::Manifest {
    use file_storage::storage::manifest::{ChunkRef, Manifest};
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

fn ops_for(
    manifest_a: &file_storage::storage::manifest::Manifest,
    manifest_b: &file_storage::storage::manifest::Manifest,
    blob_bytes: Vec<u8>,
    hash: [u8; 32],
) -> Vec<pb::ReplicationOp> {
    let mk_pm = |m: &file_storage::storage::manifest::Manifest| pb::ReplicationOp {
        op: Some(pb::replication_op::Op::PutManifest(pb::PutManifest {
            manifest: Some(file_storage::grpc::convert::manifest_to_proto(m, true)),
        })),
    };
    let size = blob_bytes.len() as u64;
    vec![
        pb::ReplicationOp {
            op: Some(pb::replication_op::Op::PutChunk(pb::PutChunk {
                hash: hash.to_vec(),
                plain_hash: hash.to_vec(),
                data: blob_bytes,
                size_original: size,
                size_stored: size,
                compressed: false,
                encrypted: false,
            })),
        },
        mk_pm(manifest_a),
        mk_pm(manifest_b),
        pb::ReplicationOp {
            op: Some(pb::replication_op::Op::Flush(pb::Flush {})),
        },
    ]
}

async fn send_batch(addr: &str, api_key: &str, ops: Vec<pb::ReplicationOp>) -> pb::ReplicationAck {
    let channel = Channel::from_shared(addr.to_string())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut raw = ReplicationServiceClient::new(channel);
    let mut req = Request::new(futures::stream::iter(ops));
    let mut key = AsciiMetadataValue::try_from(api_key).unwrap();
    key.set_sensitive(true);
    req.metadata_mut().insert("x-api-key", key);
    let mut acks = raw.replicate_batch(req).await.unwrap().into_inner();
    acks.next().await.unwrap().unwrap()
}

/// Audit gate (C-02), inverted: purging manifest A over gRPC must leave
/// manifest B fully readable and the shared blob intact; only after B is
/// deleted does the reachability GC reclaim the blob.
#[tokio::test]
async fn replication_grpc_purge_shared_chunk() {
    let (rest, grpc, state, _dir) = spawn_replica().await;
    let api_key = state.0.cfg.replication.api_key.clone();

    let data = vec![7u8; 512];
    let hash = file_storage::core::hasher::hash_bytes(&data);
    let a = manifest_with("a.bin", hash, data.len() as u64);
    let b = manifest_with("b.bin", hash, data.len() as u64);

    let ack = send_batch(&grpc, &api_key, ops_for(&a, &b, data.clone(), hash)).await;
    assert!(ack.success, "batch apply failed: {}", ack.error);

    // Both manifests reference the shared chunk.
    assert!(state.0.blocks.exists(&hash).await.unwrap());

    // Purge A via the gRPC replication protocol.
    let ack = send_batch(
        &grpc,
        &api_key,
        vec![
            pb::ReplicationOp {
                op: Some(pb::replication_op::Op::DeleteFile(pb::DeleteFile {
                    file_id: a.file_id.to_string(),
                    purge: true,
                })),
            },
            pb::ReplicationOp {
                op: Some(pb::replication_op::Op::Flush(pb::Flush {})),
            },
        ],
    )
    .await;
    assert!(ack.success);

    // INVARIANT (the audit's bug): B still reads its bytes; blob intact.
    assert!(
        state.0.blocks.exists(&hash).await.unwrap(),
        "C-02: gRPC purge deleted a blob still referenced by b.bin"
    );
    let bundle = file_storage::grpc::client::ReplicationClient::connect(&grpc, &api_key)
        .await
        .unwrap()
        .pull_manifest(&b.file_id.to_string())
        .await
        .unwrap();
    assert_eq!(bundle.manifest.unwrap().file_id, b.file_id.to_string());

    // Delete B too, then reachability GC reclaims the blob.
    let ack = send_batch(
        &grpc,
        &api_key,
        vec![
            pb::ReplicationOp {
                op: Some(pb::replication_op::Op::DeleteFile(pb::DeleteFile {
                    file_id: b.file_id.to_string(),
                    purge: true,
                })),
            },
            pb::ReplicationOp {
                op: Some(pb::replication_op::Op::Flush(pb::Flush {})),
            },
        ],
    )
    .await;
    assert!(ack.success);
    tokio::time::sleep(Duration::from_millis(10)).await;
    let report = state.0.gc.run_once().await.unwrap();
    assert!(
        report.orphans_removed >= 1,
        "reachability GC must reclaim the unreferenced blob"
    );
    assert!(!state.0.blocks.exists(&hash).await.unwrap());
    let _ = rest;
}

/// Cross-protocol parity: HTTP /internal purge and gRPC DeleteFile purge
/// produce identical final state.
#[tokio::test]
async fn purge_semantics_http_equals_grpc() {
    let (rest, grpc, state, _dir) = spawn_replica().await;
    let api_key = state.0.cfg.replication.api_key.clone();
    let http = reqwest::Client::new();

    // Two independent replica states, same ops.
    let mut finals = Vec::new();
    for mode in ["http", "grpc"] {
        let data = vec![3u8; 128];
        let hash = file_storage::core::hasher::hash_bytes(&data);
        let name_a = format!("a-{mode}.bin");
        let name_b = format!("b-{mode}.bin");
        let a = manifest_with(&name_a, hash, data.len() as u64);
        let b = manifest_with(&name_b, hash, data.len() as u64);

        match mode {
            "http" => {
                // PUT chunk via /internal/chunk, manifests via /internal/manifest
                let r = http
                    .post(format!(
                        "{rest}/internal/chunk?hash={}&plain_hash={}&size_original={}&size_stored={}",
                        hex::encode(hash),
                        hex::encode(hash),
                        data.len(),
                        data.len()
                    ))
                    .header("X-Api-Key", &api_key)
                    .body(data.clone())
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), 201);
                for m in [&a, &b] {
                    let bytes = file_storage::grpc::convert::manifest_msgpack(m).unwrap();
                    let r = http
                        .post(format!("{rest}/internal/manifest"))
                        .header("X-Api-Key", &api_key)
                        .body(bytes)
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(r.status(), 201);
                }
                // purge via HTTP: manifest-only
                let r = http
                    .delete(format!("{rest}/internal/manifest/{}", a.file_id))
                    .header("X-Api-Key", &api_key)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), 204);
            }
            "grpc" => {
                let ack = send_batch(&grpc, &api_key, ops_for(&a, &b, data.clone(), hash)).await;
                assert!(ack.success);
                let ack = send_batch(
                    &grpc,
                    &api_key,
                    vec![
                        pb::ReplicationOp {
                            op: Some(pb::replication_op::Op::DeleteFile(pb::DeleteFile {
                                file_id: a.file_id.to_string(),
                                purge: true,
                            })),
                        },
                        pb::ReplicationOp {
                            op: Some(pb::replication_op::Op::Flush(pb::Flush {})),
                        },
                    ],
                )
                .await;
                assert!(ack.success);
            }
            _ => unreachable!(),
        }

        // Final state for THIS iteration: b live, a gone, blob present.
        let manifests = state.0.index.all_manifests().await.unwrap();
        let a_live = manifests
            .iter()
            .any(|m| m.name == name_a && !m.is_deleted());
        let b_live = manifests
            .iter()
            .any(|m| m.name == name_b && !m.is_deleted());
        let blob_present = state.0.blocks.exists(&hash).await.unwrap();
        finals.push((a_live, b_live, blob_present));
    }

    for (mode, (a_live, b_live, blob)) in finals.iter().enumerate() {
        let mode = ["http", "grpc"][mode];
        assert!(!a_live, "{mode}: purged manifest must be gone");
        assert!(b_live, "{mode}: surviving manifest must stay live");
        assert!(blob, "{mode}: shared blob must survive a single purge");
    }
    assert_eq!(
        finals[0].1, finals[1].1,
        "blob survival must not depend on transport"
    );
}

/// C-02 guard: a replica must refuse refcount-mode GC outright.
#[tokio::test]
async fn replica_refuses_refcount_gc() {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let index = Arc::new(
        file_storage::storage::index_db::IndexDb::open(&dir.path().join("i.redb")).unwrap(),
    );
    let blocks = Arc::new(file_storage::storage::block_store::BlockStore::new(
        dir.path().join("blocks"),
        2,
    ));
    let cfg = ReplicationConfig::default();
    let repl =
        file_storage::replication::replicator::Replicator::start(&cfg, index.clone()).unwrap();
    let gc = file_storage::gc::GarbageCollector::new(
        index.clone(),
        blocks.clone(),
        repl,
        GcMode::Refcount,
        NodeRole::Replica,
        Duration::from_secs(0),
        Duration::from_secs(0),
    );
    // Plant a would-be victim: blob + zero-ref row (what refcount GC eats).
    blocks.write(&[8u8; 32], &[8u8; 32]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let report = gc.run_once().await.unwrap();
    assert_eq!(
        report.chunks_removed + report.orphans_removed,
        0,
        "refcount GC must be a no-op on a replica"
    );
    assert!(blocks.exists(&[8u8; 32]).await.unwrap());
}

use file_storage::config::ReplicationConfig;
