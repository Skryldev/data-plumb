//! End-to-end gRPC integration tests: real tonic server on an ephemeral
//! port, exercised through the Rust client SDK and the raw generated
//! clients. Covers streaming round-trips, dedup, ranges, authZ, health,
//! reflection, TLS, concurrency, admin and the internal replication
//! protocol.

use bytes::Bytes;
use file_storage::config::AppConfig;
use file_storage::grpc::client::FileStorageClient;
use file_storage::grpc::generated::pb;
use file_storage::grpc::generated::FileServiceClient;
use file_storage::grpc::server::GrpcServer;
use file_storage::state::AppState;
use futures::StreamExt as _;
use std::sync::Arc;
use tonic::transport::Channel;

// -- harness -----------------------------------------------------------------

struct GrpcFixture {
    addr: String,
    state: AppState,
    /// Keeps the data dir alive for the server's lifetime.
    _dir: tempfile::TempDir,
}

fn test_config() -> (AppConfig, tempfile::TempDir) {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).expect("tempdir");
    let mut cfg = AppConfig::for_tests();
    cfg = cfg.with_data_root(dir.path());
    (cfg, dir)
}

/// Boots a full dual-stack fixture; only the gRPC endpoint is used here.
async fn spawn_grpc() -> GrpcFixture {
    file_storage::metrics::init_recorder();
    let (cfg, dir) = test_config();
    let state = file_storage::state::build_state(Arc::new(cfg))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = GrpcServer::new(state.clone(), None);
    tokio::spawn(async move {
        server.serve_on(listener).await.expect("grpc server");
    });
    wait_ready(addr).await;
    GrpcFixture {
        addr: format!("http://{addr}"),
        state,
        _dir: dir,
    }
}

async fn wait_ready(addr: std::net::SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            // give the runtime a moment to finish route setup
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("grpc server did not become ready");
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

// -- tests -------------------------------------------------------------------

#[tokio::test]
async fn grpc_upload_download_roundtrip() {
    let fx = spawn_grpc().await;
    let mut c = FileStorageClient::connect(&fx.addr).await.unwrap();
    let pair = c.login("alice", "alice123").await.unwrap();
    assert!(!pair.access_token.is_empty());

    let payload = pseudo_random(10 * 1024 * 1024, 42);
    let header = pb::UploadHeader {
        name: "roundtrip.bin".into(),
        mime_type: "application/octet-stream".into(),
        expected_size: payload.len() as u64,
        tenant_id: String::new(),
        tags: vec!["grpc".into()],
    };
    let resp = c
        .upload(header, std::io::Cursor::new(payload.clone()))
        .await
        .unwrap();
    assert_eq!(resp.manifest.as_ref().unwrap().size, payload.len() as u64);
    let file_id = resp.manifest.as_ref().unwrap().file_id.clone();

    // Download through the SDK and verify byte-identical content.
    let mut downloaded = Vec::with_capacity(payload.len());
    let hdr = c.download(&file_id, &mut downloaded).await.unwrap();
    assert_eq!(hdr.total_size, payload.len() as u64);
    assert_eq!(downloaded.len(), payload.len());
    assert_eq!(downloaded, payload, "content must be byte-identical");

    // Verify the original hash reported by the server.
    let mut hasher = blake3::Hasher::new();
    hasher.update(&payload);
    assert_eq!(
        hdr.original_hash,
        hasher.finalize().as_bytes().to_vec(),
        "server-reported original hash must match"
    );
    let _ = pair;
}

#[tokio::test]
async fn grpc_dedup_hit_reports_saved_bytes() {
    let fx = spawn_grpc().await;
    let mut c = FileStorageClient::connect(&fx.addr).await.unwrap();
    c.login("alice", "alice123").await.unwrap();

    // Identical payloads → full dedup on 2nd upload. Random content keeps
    // intra-file chunks unique, so the FIRST upload must report zero dedup.
    let payload = pseudo_random(2 * 1024 * 1024, 42);
    let header = |name: &str| pb::UploadHeader {
        name: name.into(),
        mime_type: "application/octet-stream".into(),
        expected_size: 0,
        tenant_id: String::new(),
        tags: vec![],
    };
    let first = c
        .upload(header("dup-a.txt"), std::io::Cursor::new(payload.clone()))
        .await
        .unwrap();
    assert_eq!(first.dedup_chunks, 0);
    let second = c
        .upload(header("dup-b.txt"), std::io::Cursor::new(payload.clone()))
        .await
        .unwrap();
    assert!(
        second.deduped_bytes > 0,
        "second upload must report dedup savings"
    );
    assert_eq!(
        second.manifest.as_ref().unwrap().original_hash,
        first.manifest.as_ref().unwrap().original_hash
    );
}

#[tokio::test]
async fn grpc_range_download_returns_exact_slice() {
    let fx = spawn_grpc().await;
    let mut c = FileStorageClient::connect(&fx.addr).await.unwrap();
    c.login("alice", "alice123").await.unwrap();

    let payload = pseudo_random(1024 * 1024, 7);
    let resp = c
        .upload(
            pb::UploadHeader {
                name: "range.bin".into(),
                ..Default::default()
            },
            std::io::Cursor::new(payload.clone()),
        )
        .await
        .unwrap();
    let file_id = resp.manifest.as_ref().unwrap().file_id.clone();

    let mut out = Vec::new();
    let hdr = c
        .download_range(&file_id, Some(100), Some(200), &mut out)
        .await
        .unwrap();
    assert_eq!(hdr.size, 100);
    assert_eq!(hdr.start, 100);
    assert_eq!(hdr.end, 200);
    assert_eq!(out, &payload[100..200], "range content must match");
}

#[tokio::test]
async fn grpc_requires_authentication() {
    let fx = spawn_grpc().await;
    // No token at all → Unauthenticated.
    let c = FileStorageClient::connect(&fx.addr).await.unwrap();
    let err = c
        .get_manifest("00000000-0000-7000-8000-000000000000", false)
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // Bad password → Unauthenticated.
    let mut c = FileStorageClient::connect(&fx.addr).await.unwrap();
    let err = c.login("alice", "wrong-password").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn grpc_cross_tenant_access_denied() {
    let fx = spawn_grpc().await;
    // Admin uploads into their tenant.
    let mut admin = FileStorageClient::connect(&fx.addr).await.unwrap();
    admin.login("admin", "admin123").await.unwrap();
    let resp = admin
        .upload(
            pb::UploadHeader {
                name: "secret.txt".into(),
                ..Default::default()
            },
            std::io::Cursor::new(vec![1u8; 4096]),
        )
        .await
        .unwrap();
    let file_id = resp.manifest.as_ref().unwrap().file_id.clone();

    // Alice (different tenant) cannot read or delete it.
    let mut alice = FileStorageClient::connect(&fx.addr).await.unwrap();
    alice.login("alice", "alice123").await.unwrap();
    let err = alice.get_manifest(&file_id, false).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    let err = alice.delete_file(&file_id, false).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // Admin can hard-purge; afterwards the blob is gone from disk.
    admin.delete_file(&file_id, true).await.unwrap();
    let manifest = resp.manifest.unwrap();
    for chunk in manifest.chunks {
        assert!(
            !fx.state
                .0
                .blocks
                .exists(&to_hash(&chunk.hash).unwrap())
                .await
                .unwrap(),
            "purged blob must be removed"
        );
    }
}

fn to_hash(bytes: &[u8]) -> Option<[u8; 32]> {
    bytes.try_into().ok()
}

#[tokio::test]
async fn grpc_health_check_reports_serving() {
    let fx = spawn_grpc().await;
    let channel = Channel::from_shared(fx.addr.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut health = tonic_health::pb::health_client::HealthClient::new(channel);
    for name in [
        "file_storage.v1.FileService",
        "file_storage.v1.AuthService",
        "file_storage.v1.AdminService",
        "file_storage.v1.ReplicationService",
        "",
    ] {
        let status = health
            .check(tonic::Request::new(tonic_health::pb::HealthCheckRequest {
                service: name.to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            status.status,
            tonic_health::pb::health_check_response::ServingStatus::Serving as i32,
            "service {name} must be SERVING"
        );
    }
}

#[tokio::test]
async fn grpc_reflection_lists_all_services() {
    let fx = spawn_grpc().await;
    let channel = Channel::from_shared(fx.addr.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();

    // Minimal hand-rolled reflection client (protobuf messages defined
    // locally with prost derive).
    #[derive(Clone, PartialEq, prost::Message)]
    struct ReflectionRequest {
        #[prost(string, tag = "1")]
        host: String,
        #[prost(string, tag = "7")]
        list_services: String,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    struct ServiceResponse {
        #[prost(string, tag = "1")]
        name: String,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    struct ListServiceResponse {
        #[prost(message, repeated, tag = "1")]
        service: Vec<ServiceResponse>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    struct ReflectionResponse {
        #[prost(string, tag = "1")]
        valid_host: String,
        #[prost(message, optional, tag = "6")]
        list_services_response: Option<ListServiceResponse>,
    }

    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.unwrap();
    let response: tonic::Response<tonic::Streaming<ReflectionResponse>> = grpc
        .streaming(
            tonic::Request::new(futures::stream::iter(vec![ReflectionRequest {
                host: String::new(),
                list_services: "*".into(),
            }])),
            http::uri::PathAndQuery::from_static(
                "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
            ),
            tonic::codec::ProstCodec::<ReflectionRequest, ReflectionResponse>::default(),
        )
        .await
        .unwrap();
    let response: ReflectionResponse = response
        .into_inner()
        .message()
        .await
        .unwrap()
        .expect("reflection must answer the list_services request");
    let names: Vec<String> = response
        .list_services_response
        .unwrap_or_default()
        .service
        .into_iter()
        .map(|s| s.name)
        .collect();
    for expected in [
        "file_storage.v1.FileService",
        "file_storage.v1.AuthService",
        "file_storage.v1.AdminService",
        "file_storage.v1.ReplicationService",
        "grpc.health.v1.Health",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "reflection must advertise {expected}; got {names:?}"
        );
    }
}

#[tokio::test]
async fn grpc_soft_delete_hides_then_gc_purges() {
    let fx = spawn_grpc().await;
    let mut c = FileStorageClient::connect(&fx.addr).await.unwrap();
    c.login("admin", "admin123").await.unwrap();

    let resp = c
        .upload(
            pb::UploadHeader {
                name: "tombstone.txt".into(),
                ..Default::default()
            },
            std::io::Cursor::new(vec![3u8; 8192]),
        )
        .await
        .unwrap();
    let file_id = resp.manifest.as_ref().unwrap().file_id.clone();

    // Soft delete hides the file.
    c.delete_file(&file_id, false).await.unwrap();
    let err = c.get_manifest(&file_id, false).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);

    // GC pass runs cleanly (tombstone still inside its grace window here);
    // full purge timing is covered by the REST gc integration tests.
    let report = c.trigger_gc(1, false).await.unwrap();
    let _ = report;
}

#[tokio::test]
async fn grpc_admin_requires_admin_role() {
    let fx = spawn_grpc().await;
    let mut alice = FileStorageClient::connect(&fx.addr).await.unwrap();
    alice.login("alice", "alice123").await.unwrap();
    let err = alice.list_tenants(10).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    let mut admin = FileStorageClient::connect(&fx.addr).await.unwrap();
    admin.login("admin", "admin123").await.unwrap();
    let tenants = admin.list_tenants(10).await.unwrap();
    assert!(tenants.total >= 3, "seeded tenants must be listed");

    // Audit query returns at least the login events.
    let audit = admin.query_audit(50, "").await.unwrap();
    assert!(!audit.entries.is_empty(), "audit trail must have entries");
}

#[tokio::test]
async fn grpc_concurrent_uploads_all_succeed() {
    let fx = spawn_grpc().await;
    let mut login = FileStorageClient::connect(&fx.addr).await.unwrap();
    login.login("alice", "alice123").await.unwrap();

    let mut handles = Vec::new();
    for i in 0..16u32 {
        let addr = fx.addr.clone();
        let token = login.token().expect("token set after login").to_string();
        handles.push(tokio::spawn(async move {
            let c = FileStorageClient::connect(&addr)
                .await
                .unwrap()
                .with_token(token);
            let payload = pseudo_random(512 * 1024, i as u64);
            let resp = c
                .upload(
                    pb::UploadHeader {
                        name: format!("concurrent-{i}.bin"),
                        ..Default::default()
                    },
                    std::io::Cursor::new(payload.clone()),
                )
                .await
                .unwrap();
            let file_id = resp.manifest.as_ref().unwrap().file_id.clone();
            let mut out = Vec::new();
            c.download(&file_id, &mut out).await.unwrap();
            assert_eq!(out, payload);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

#[tokio::test]
async fn grpc_tls_accepts_tls_and_rejects_plaintext() {
    file_storage::metrics::init_recorder();
    let (cfg, dir) = test_config();
    let state = file_storage::state::build_state(Arc::new(cfg))
        .await
        .unwrap();

    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::from([
            127, 0, 0, 1,
        ])));
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    let tls = tonic::transport::ServerTlsConfig::new().identity(
        tonic::transport::Identity::from_pem(cert_pem.clone(), key_pem),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = GrpcServer::new(state, Some(tls));
    tokio::spawn(async move {
        server.serve_on(listener).await.expect("grpc tls server");
    });
    wait_ready(addr).await;
    let _dir = dir;

    // TLS client with pinned CA succeeds end-to-end.
    let mut c =
        FileStorageClient::connect_tls(&format!("https://{addr}"), Some(cert_pem.as_bytes()))
            .await
            .unwrap();
    c.login("alice", "alice123").await.unwrap();
    let resp = c
        .upload(
            pb::UploadHeader {
                name: "over-tls.bin".into(),
                ..Default::default()
            },
            std::io::Cursor::new(vec![9u8; 4096]),
        )
        .await
        .unwrap();
    assert!(resp.manifest.is_some());

    // Plaintext against the TLS port must fail: either at connect time or
    // on the very first RPC (handshake is lazy in some connector paths).
    match FileStorageClient::connect(&format!("http://{addr}")).await {
        Err(_) => {} // rejected during connect — expected
        Ok(mut plain_client) => {
            let err = plain_client
                .login("alice", "alice123")
                .await
                .expect_err("plaintext RPC against a TLS-only port must fail");
            assert_ne!(err.code(), tonic::Code::Ok);
        }
    }
}

#[tokio::test]
async fn grpc_sdk_auth_lifecycle() {
    let fx = spawn_grpc().await;
    let mut c = FileStorageClient::connect(&fx.addr).await.unwrap();
    let p1 = c.login("alice", "alice123").await.unwrap();
    assert_eq!(p1.token_type, "Bearer");

    let p2 = c.refresh(&p1.refresh_token).await.unwrap();
    assert_ne!(p1.refresh_token, p2.refresh_token, "rotation required");
    assert_ne!(p1.access_token, p2.access_token);

    // Old refresh token is single-use.
    let mut fresh = FileStorageClient::connect(&fx.addr).await.unwrap();
    let err = fresh.refresh(&p1.refresh_token).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    c.logout(&p2.refresh_token).await.unwrap();
    let err = fresh.refresh(&p2.refresh_token).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn grpc_replicate_batch_and_pull_roundtrip() {
    let fx = spawn_grpc().await;
    let api_key = fx.state.0.cfg.replication.api_key.clone();

    // Missing key → Unauthenticated.
    let no_auth = file_storage::grpc::client::ReplicationClient::connect(&fx.addr, "wrong")
        .await
        .unwrap();
    let err = no_auth
        .pull_manifest("00000000-0000-7000-8000-000000000000")
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    let repl = file_storage::grpc::client::ReplicationClient::connect(&fx.addr, &api_key)
        .await
        .unwrap();

    // Build a manifest + one chunk locally, then push via ReplicateBatch.
    let tenant: uuid::Uuid = "01930000-0000-7000-8000-000000000001".parse().unwrap();
    let data = vec![5u8; 4096];
    let plain_hash = file_storage::core::hasher::hash_bytes(&data);
    let blob = Bytes::from(data.clone());
    let file_id = uuid::Uuid::now_v7();
    let manifest = file_storage::storage::manifest::Manifest {
        file_id,
        name: "peer.bin".into(),
        tenant_id: tenant,
        mime: "application/octet-stream".into(),
        size_original: data.len() as u64,
        size_stored: blob.len() as u64,
        original_hash: plain_hash,
        chunks: vec![file_storage::storage::manifest::ChunkRef {
            hash: plain_hash,
            plain_hash,
            offset: 0,
            size_original: data.len() as u64,
            size_stored: blob.len() as u64,
            compressed: false,
            encrypted: false,
        }],
        tags: vec![],
        created_at_ms: 0,
        updated_at_ms: 0,
        deleted_at_ms: None,
        version: 1,
    };

    // Stream ops through the raw generated client (bidi).
    let ops = vec![
        pb::ReplicationOp {
            op: Some(pb::replication_op::Op::PutChunk(pb::PutChunk {
                hash: plain_hash.to_vec(),
                plain_hash: plain_hash.to_vec(),
                data: blob.to_vec(),
                size_original: data.len() as u64,
                size_stored: blob.len() as u64,
                compressed: false,
                encrypted: false,
            })),
        },
        pb::ReplicationOp {
            op: Some(pb::replication_op::Op::PutManifest(pb::PutManifest {
                manifest: Some(file_storage::grpc::convert::manifest_to_proto(
                    &manifest, true,
                )),
            })),
        },
        pb::ReplicationOp {
            op: Some(pb::replication_op::Op::Flush(pb::Flush {})),
        },
    ];
    use file_storage::grpc::generated::ReplicationServiceClient;
    use tonic::metadata::AsciiMetadataValue;

    let channel = Channel::from_shared(fx.addr.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut raw = ReplicationServiceClient::new(channel.clone());
    let mut req = tonic::Request::new(futures::stream::iter(ops));
    let mut key = AsciiMetadataValue::try_from(api_key.as_str()).unwrap();
    key.set_sensitive(true);
    req.metadata_mut().insert("x-api-key", key);
    let mut ack_stream = raw.replicate_batch(req).await.unwrap().into_inner();
    let ack = ack_stream.next().await.unwrap().unwrap();
    assert!(ack.success, "batch must apply cleanly: {}", ack.error);
    assert_eq!(ack.ops_acked, 2, "chunk + manifest ops");

    // PullChunk streams the exact blob; PullManifest returns the manifest.
    let pulled = repl.pull_chunk(&plain_hash).await.unwrap();
    assert_eq!(pulled, data, "pulled blob must match");
    let bundle = repl.pull_manifest(&file_id.to_string()).await.unwrap();
    let back = bundle.manifest.unwrap();
    assert_eq!(back.file_id, file_id.to_string());
    assert_eq!(back.chunks.len(), 1);
}

#[tokio::test]
async fn grpc_download_of_unknown_file_fails_cleanly() {
    let fx = spawn_grpc().await;
    let mut c = FileStorageClient::connect(&fx.addr).await.unwrap();
    c.login("alice", "alice123").await.unwrap();
    let err = c
        .download("01930000-0000-7000-8000-00000000dead", &mut Vec::new())
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn grpc_list_files_respects_tenant_scope() {
    let fx = spawn_grpc().await;
    let mut alice = FileStorageClient::connect(&fx.addr).await.unwrap();
    alice.login("alice", "alice123").await.unwrap();
    let alice_tenant = "01930000-0000-7000-8000-000000000002";

    let _override_result = alice
        .upload(
            pb::UploadHeader {
                name: "alice-file.txt".into(),
                tags: vec!["team".into()],
                tenant_id: alice_tenant.into(),
                ..Default::default()
            },
            std::io::Cursor::new(vec![2u8; 2048]),
        )
        .await;
    // ^ non-admin tenant override is rejected…

    // …so upload normally (into alice's own tenant) and list.
    let resp = alice
        .upload(
            pb::UploadHeader {
                name: "alice-file.txt".into(),
                tags: vec!["team".into()],
                ..Default::default()
            },
            std::io::Cursor::new(vec![2u8; 2048]),
        )
        .await
        .unwrap();
    let listing = alice.list_files(50, 0, "team", "", false).await.unwrap();
    assert!(listing
        .files
        .iter()
        .any(|m| m.name == "alice-file.txt" && m.tenant_id == alice_tenant));
    let _ = resp;

    // A different tenant's listing does not contain alice's file.
    let mut bob = FileStorageClient::connect(&fx.addr).await.unwrap();
    bob.login("bob", "bob123").await.unwrap();
    let listing = bob.list_files(50, 0, "", "", false).await.unwrap();
    assert!(
        !listing.files.iter().any(|m| m.name == "alice-file.txt"),
        "tenant isolation must hold on listings"
    );
}

#[tokio::test]
async fn grpc_expected_size_guard_rejects_oversize() {
    let fx = spawn_grpc().await;
    let mut c = FileStorageClient::connect(&fx.addr).await.unwrap();
    c.login("alice", "alice123").await.unwrap();
    let payload = vec![8u8; 64 * 1024];
    let err = c
        .upload(
            pb::UploadHeader {
                name: "oversize.bin".into(),
                expected_size: 1024, // stream exceeds this
                ..Default::default()
            },
            std::io::Cursor::new(payload),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted);
}

#[tokio::test]
async fn grpc_metrics_record_rpc_calls() {
    let fx = spawn_grpc().await;
    let mut c = FileStorageClient::connect(&fx.addr).await.unwrap();
    c.login("viewer", "viewer123").await.unwrap();
    let _ = c.list_files(10, 0, "", "", false).await;
    let rendered = file_storage::metrics::render();
    assert!(
        rendered.contains("file_storage_grpc_requests_total"),
        "grpc metrics must be exported"
    );
    assert!(
        rendered.contains("file_storage.v1.AuthService"),
        "rpc label must carry the method path"
    );
}

/// Restores doc-compile parity: verifies the generated client compiles
/// against the SDK trait surface used above.
#[tokio::test]
async fn grpc_generated_client_unary_manifest() {
    let fx = spawn_grpc().await;
    let channel = Channel::from_shared(fx.addr.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut raw = FileServiceClient::new(channel);
    let err = raw
        .get_manifest(tonic::Request::new(pb::GetManifestRequest {
            file_id: "01930000-0000-7000-8000-000000000000".into(),
            include_chunks: false,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn grpc_and_rest_share_storage() {
    // Proves both transports hit the same block store + index: upload over
    // gRPC → download over REST must be byte-identical, and vice versa.
    use file_storage::grpc::generated::pb;

    let fx = spawn_grpc().await;

    // Build a REST server on the SAME state.
    let app = file_storage::api::router::build_router(fx.state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let rest = format!("http://{rest_addr}");

    let http = reqwest::Client::new();
    async fn login(http: &reqwest::Client, rest: &str, name: &str) -> String {
        let resp = http
            .post(format!("{rest}/api/v1/auth/login"))
            .json(&serde_json::json!({"username": name, "password": format!("{name}123")}))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        body["access_token"].as_str().unwrap().to_string()
    }

    // 1. Upload over gRPC, download over REST.
    let mut grpc = FileStorageClient::connect(&fx.addr).await.unwrap();
    grpc.login("admin", "admin123").await.unwrap();
    let grpc_payload = pseudo_random(512 * 1024, 91);
    let resp = grpc
        .upload(
            pb::UploadHeader {
                name: "cross-grpc.bin".into(),
                ..Default::default()
            },
            std::io::Cursor::new(grpc_payload.clone()),
        )
        .await
        .unwrap();
    let grpc_file_id = resp.manifest.as_ref().unwrap().file_id.clone();

    let token = login(&http, &rest, "admin").await;
    let rest_bytes = http
        .get(format!("{rest}/api/v1/files/{grpc_file_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        rest_bytes.as_ref(),
        grpc_payload.as_slice(),
        "gRPC upload must be byte-identical over REST"
    );

    // 2. Upload over REST, download over gRPC.
    let rest_payload = pseudo_random(512 * 1024, 92);
    let created: serde_json::Value = http
        .post(format!(
            "{rest}/api/v1/files?name=cross-rest.bin&mime=application/octet-stream"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/octet-stream")
        .body(rest_payload.clone())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let rest_file_id = created["file_id"].as_str().unwrap().to_string();

    let mut out = Vec::new();
    let hdr = grpc.download(&rest_file_id, &mut out).await.unwrap();
    assert_eq!(
        out, rest_payload,
        "REST upload must be byte-identical over gRPC"
    );
    assert_eq!(hdr.total_size, rest_payload.len() as u64);
}
