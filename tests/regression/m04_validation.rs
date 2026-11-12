//! M-04 gate tests — one validation layer for both transports.
//!
//! These tests INVERT the audit's M-04a..M-04e drift findings:
//! identical malformed inputs must produce identical outcomes on REST
//! (HTTP status) and gRPC (tonic status code), the `include_deleted`
//! gate must be admin-only on BOTH transports, list totals must come
//! from the same counting code on both sides, REST gains the
//! `expected_size` query param capped like gRPC, and GetManifest must
//! truncate hostile chunk lists (M-04c).
//!
//! Red/green protocol: `parity_validation_matrix`,
//! `viewers_cannot_list_deleted_both_transports` and
//! `list_total_matches_rest` were run against the pre-fix tree and
//! FAILED there (REST accepted 256-char names, viewers could list
//! deleted files on REST, gRPC page-local totals). They must stay green
//! from now on.

#[path = "../integration/common.rs"]
mod common;

use common::TestServer;
use file_storage::grpc::client::FileStorageClient;
use file_storage::grpc::generated::pb;

// -- shared fixtures ---------------------------------------------------------

async fn rest_server() -> TestServer {
    let (cfg, dir) = common::base_config("m04");
    common::spawn_server_with_dir(cfg, dir).await
}

/// Attaches a gRPC server to an ALREADY-RUNNING REST server's state so
/// both transports see the same store (parity tests need one source of
/// truth). Returns the gRPC endpoint.
async fn grpc_attach(state: file_storage::state::AppState) -> String {
    file_storage::metrics::init_recorder();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = file_storage::grpc::server::GrpcServer::new(state, None);
    tokio::spawn(async move {
        server.serve_on(listener).await.expect("grpc server");
    });
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return format!("http://{addr}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("grpc server did not become ready");
}

/// Dual-transport stack: one AppState, REST + gRPC on ephemeral ports.
async fn spawn_stack() -> (TestServer, String) {
    let rest = rest_server().await;
    let addr = grpc_attach(rest.state.clone()).await;
    (rest, addr)
}

fn header(name: &str, mime: &str, tags: Vec<String>, expected_size: u64) -> pb::UploadHeader {
    pb::UploadHeader {
        name: name.to_string(),
        mime_type: mime.to_string(),
        expected_size,
        tenant_id: String::new(),
        tags,
    }
}

// -- gate 1: parity_validation_matrix ----------------------------------------

/// One parity case: (case_name, name, mime, tags, expected_size,
///  want_http_status, want_grpc_code).
type ParityCase<'a> = (
    &'a str,
    &'a str,
    &'a str,
    Vec<String>,
    u64,
    u16,
    tonic::Code,
);

/// One table, both transports, identical verdicts.
#[tokio::test]
async fn parity_validation_matrix() {
    let (rest, grpc_addr) = spawn_stack().await;
    let admin = rest.admin_token().await;
    let mut gc = FileStorageClient::connect(&grpc_addr).await.unwrap();
    gc.login("admin", "admin123").await.unwrap();

    let long_name = "n".repeat(256);
    let long_tag = "t".repeat(65);
    let long_mime = format!("application/{}", "x".repeat(120)); // 132 bytes > 128
    let over_size = (1024 * 1024 * 1024) as u64 + 1; // for_tests max_file_size + 1

    let cases: Vec<ParityCase> = vec![
        (
            "name_256_chars",
            &long_name,
            "application/octet-stream",
            vec![],
            0,
            400,
            tonic::Code::InvalidArgument,
        ),
        (
            "name_control_char",
            "bad\nname",
            "application/octet-stream",
            vec![],
            0,
            400,
            tonic::Code::InvalidArgument,
        ),
        (
            "tag_65_chars",
            "ok.bin",
            "application/octet-stream",
            vec![long_tag],
            0,
            400,
            tonic::Code::InvalidArgument,
        ),
        (
            "tags_33_entries",
            "ok.bin",
            "application/octet-stream",
            (0..33).map(|i| format!("t{i}")).collect(),
            0,
            400,
            tonic::Code::InvalidArgument,
        ),
        (
            "mime_129_chars",
            "ok.bin",
            &long_mime,
            vec![],
            0,
            400,
            tonic::Code::InvalidArgument,
        ),
        (
            "expected_size_over_cap",
            "ok.bin",
            "application/octet-stream",
            vec![],
            over_size,
            400,
            tonic::Code::InvalidArgument,
        ),
    ];

    for (case, name, mime, tags, expected_size, want_http, want_grpc) in &cases {
        // REST raw upload: malformed metadata must be rejected before any
        // body byte is processed.
        let mut query = vec![
            ("name", name.to_string()),
            ("mime", mime.to_string()),
            ("expected_size", expected_size.to_string()),
        ];
        if !tags.is_empty() {
            query.push(("tags", tags.join(",")));
        }
        let resp = rest
            .client()
            .post(format!("{}/api/v1/files", rest.base))
            .query(&query)
            .header("Authorization", format!("Bearer {admin}"))
            .header("Content-Type", "application/octet-stream")
            .body(b"tiny".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            *want_http,
            "REST case {case}: expected {want_http}, got {} ({})",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );

        // gRPC: same metadata through the upload header.
        let tag_refs: Vec<String> = tags.clone();
        let err = gc
            .upload(
                header(name, mime, tag_refs, *expected_size),
                std::io::Cursor::new(b"tiny".to_vec()),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            *want_grpc,
            "gRPC case {case}: expected {want_grpc}, got {err:?}"
        );
    }

    // Sanity: a well-formed request still succeeds on both transports
    // (the matrix guards against over-blocking).
    let resp = rest
        .client()
        .post(format!("{}/api/v1/files", rest.base))
        .query(&[
            ("name", "fine.bin".to_string()),
            ("mime", "application/octet-stream".to_string()),
        ])
        .header("Authorization", format!("Bearer {admin}"))
        .header("Content-Type", "application/octet-stream")
        .body(b"fine".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "well-formed REST upload");
    let got = gc
        .upload(
            header("fine-grpc.bin", "application/octet-stream", vec![], 4),
            std::io::Cursor::new(b"fine".to_vec()),
        )
        .await
        .unwrap();
    assert_eq!(got.manifest.unwrap().name, "fine-grpc.bin");
}

// -- gate 2: viewers_cannot_list_deleted_both_transports ----------------------

#[tokio::test]
async fn viewers_cannot_list_deleted_both_transports() {
    let (rest, grpc_addr) = spawn_stack().await;

    // admin (tenant A) uploads + soft-deletes a file IN ITS OWN TENANT so
    // the admin include_deleted listing has something to find.
    let admin = rest.admin_token().await;
    let up = rest
        .upload_raw(
            &admin,
            "gone.bin",
            "application/octet-stream",
            &[],
            b"x".to_vec(),
        )
        .await;
    assert_eq!(up.status().as_u16(), 201);
    let file_id = up.json::<serde_json::Value>().await.unwrap()["file_id"]
        .as_str()
        .unwrap()
        .to_string();
    let del = rest
        .client()
        .delete(format!("{}/api/v1/files/{file_id}", rest.base))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .unwrap();
    assert!(
        del.status().is_success(),
        "soft delete must succeed (got {})",
        del.status()
    );

    // Viewer (tenant B) asks for deleted files on BOTH transports.
    let viewer = rest.login("viewer", "viewer123").await;
    let resp = rest
        .client()
        .get(format!("{}/api/v1/files", rest.base))
        .query(&[("include_deleted", "true")])
        .header("Authorization", format!("Bearer {viewer}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "viewer include_deleted must be PERMISSION_DENIED on REST"
    );

    let mut vc = FileStorageClient::connect(&grpc_addr).await.unwrap();
    vc.login("viewer", "viewer123").await.unwrap();
    let err = vc.list_files(50, 0, "", "", true).await.unwrap_err();
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "viewer include_deleted must be PERMISSION_DENIED on gRPC"
    );

    // Admins still may list deleted files on both transports.
    let resp = rest
        .client()
        .get(format!("{}/api/v1/files", rest.base))
        .query(&[("include_deleted", "true")])
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "admin include_deleted on REST");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total"].as_u64().unwrap(), 1, "deleted file listed");

    let mut ac = FileStorageClient::connect(&grpc_addr).await.unwrap();
    ac.login("admin", "admin123").await.unwrap();
    let listed = ac.list_files(50, 0, "", "", true).await.unwrap();
    assert_eq!(listed.total, 1, "admin sees the deleted file on gRPC too");

    // Plain listing without include_deleted stays open to viewers.
    let resp = rest
        .client()
        .get(format!("{}/api/v1/files", rest.base))
        .header("Authorization", format!("Bearer {viewer}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "viewer plain list");
}

// -- gate 3: list_total_matches_rest ------------------------------------------

#[tokio::test]
async fn list_total_matches_rest() {
    let (rest, grpc_addr) = spawn_stack().await;
    let alice = rest.login("alice", "alice123").await;

    for i in 0..3 {
        let resp = rest
            .upload_raw(
                &alice,
                &format!("f{i}.bin"),
                "application/octet-stream",
                if i < 2 { &["shared"] } else { &[] },
                vec![i as u8; 32],
            )
            .await;
        assert_eq!(resp.status().as_u16(), 201);
    }

    let mut gc = FileStorageClient::connect(&grpc_addr).await.unwrap();
    gc.login("alice", "alice123").await.unwrap();

    // Unfiltered, page smaller than the match set: total must be the same
    // tenant-wide count on both sides (M-04d: one counting code).
    let resp = rest
        .client()
        .get(format!("{}/api/v1/files", rest.base))
        .query(&[("limit", "2")])
        .header("Authorization", format!("Bearer {alice}"))
        .send()
        .await
        .unwrap();
    let rest_body: serde_json::Value = resp.json().await.unwrap();
    let rest_total = rest_body["total"].as_u64().unwrap();
    let grpc_total = gc.list_files(2, 0, "", "", false).await.unwrap().total as u64;
    assert_eq!(rest_total, 3, "REST total is tenant-wide");
    assert_eq!(
        rest_total, grpc_total,
        "M-04d: gRPC total must equal the REST total (same counting code)"
    );

    // Tag-filtered: both sides report the filtered count.
    let resp = rest
        .client()
        .get(format!("{}/api/v1/files", rest.base))
        .query(&[("tag", "shared"), ("limit", "1")])
        .header("Authorization", format!("Bearer {alice}"))
        .send()
        .await
        .unwrap();
    let rest_body: serde_json::Value = resp.json().await.unwrap();
    let rest_total = rest_body["total"].as_u64().unwrap();
    let grpc_total = gc
        .list_files(1, 0, "shared", "", false)
        .await
        .unwrap()
        .total as u64;
    assert_eq!(rest_total, 2, "tag-filtered total on REST");
    assert_eq!(rest_total, grpc_total, "tag-filtered total parity");
}

// -- gate 4: rest_accepts_expected_size_query ---------------------------------

#[tokio::test]
async fn rest_accepts_expected_size_query() {
    let rest = rest_server().await;
    let admin = rest.admin_token().await;

    // Accepted and honored: declared size matches the body.
    let resp = rest
        .client()
        .post(format!("{}/api/v1/files", rest.base))
        .query(&[
            ("name", "sized.bin".to_string()),
            ("mime", "application/octet-stream".to_string()),
            ("expected_size", "4".to_string()),
        ])
        .header("Authorization", format!("Bearer {admin}"))
        .header("Content-Type", "application/octet-stream")
        .body(b"abcd".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        201,
        "expected_size query param must be accepted"
    );

    // Over the cap → 400 (InvalidArgument parity with gRPC).
    let resp = rest
        .client()
        .post(format!("{}/api/v1/files", rest.base))
        .query(&[
            ("name", "toobig.bin".to_string()),
            ("mime", "application/octet-stream".to_string()),
            ("expected_size", "1073741825".to_string()), // max_file_size + 1
        ])
        .header("Authorization", format!("Bearer {admin}"))
        .header("Content-Type", "application/octet-stream")
        .body(b"abcd".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400, "over-cap expected_size");
}

// -- gate 5: getmanifest_truncates_chunks -------------------------------------

#[tokio::test]
async fn getmanifest_truncates_chunks() {
    // Small chunks so ~2 MB yields > 1000 chunks (FastCDC avg 1 KiB).
    let (cfg, dir) = common::base_config("m04-trunc");
    let mut cfg = cfg;
    cfg.chunking.min = 512;
    cfg.chunking.avg = 1024;
    cfg.chunking.max = 4096;
    let rest = common::spawn_server_with_dir(cfg, dir).await;
    let grpc = grpc_attach(rest.state.clone()).await;

    let admin = rest.admin_token().await;
    let payload = common::pseudo_random(2 * 1024 * 1024, 9);
    let up = rest
        .client()
        .post(format!("{}/api/v1/files", rest.base))
        .query(&[
            ("name", "many-chunks.bin".to_string()),
            ("mime", "application/octet-stream".to_string()),
        ])
        .header("Authorization", format!("Bearer {admin}"))
        .header("Content-Type", "application/octet-stream")
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(up.status().as_u16(), 201, "upload for truncation test");
    let body: serde_json::Value = up.json().await.unwrap();
    let chunk_count = body["chunk_count"].as_u64().unwrap();
    assert!(
        chunk_count > 1000,
        "test manifest must exceed the 1000-chunk cap (got {chunk_count})"
    );
    let file_id = body["file_id"].as_str().unwrap().to_string();

    // gRPC GetManifest: capped at 1000 with the additive flag set.
    let mut gc = FileStorageClient::connect(&grpc).await.unwrap();
    gc.login("admin", "admin123").await.unwrap();
    let m = gc.get_manifest(&file_id, true).await.unwrap();
    assert_eq!(m.chunks.len(), 1000, "gRPC chunk list must be capped");
    assert!(m.chunks_truncated, "chunks_truncated must be true");

    // Bulk listings report no truncation (no chunks requested).
    let listed = gc.list_files(50, 0, "", "", false).await.unwrap();
    assert!(listed.files.iter().all(|f| !f.chunks_truncated));

    // REST meta endpoint: same cap, same flag (pre-existing behavior made
    // structural).
    let resp = rest
        .client()
        .get(format!("{}/api/v1/files/{file_id}/meta", rest.base))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .unwrap();
    let meta: serde_json::Value = resp.json().await.unwrap();
    assert!(meta["chunks_truncated"].as_bool().unwrap());
    assert_eq!(meta["chunks"].as_array().unwrap().len(), 1000);
}
