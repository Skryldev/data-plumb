//! 4. Content-addressed deduplication.

use crate::common;

#[tokio::test]
async fn dedup_identical_content() {
    let srv = common::spawn_server(common::base_config("dd").0).await;
    let token = srv.admin_token().await;
    // Compressible content -> fewer chunks, deterministic hashes.
    let data = vec![b'A'; 512 * 1024];

    let r1 = srv
        .upload_raw(
            &token,
            "first.bin",
            "application/octet-stream",
            &[],
            data.clone(),
        )
        .await;
    assert_eq!(r1.status(), 201);
    let up1: serde_json::Value = r1.json().await.unwrap();

    let r2 = srv
        .upload_raw(
            &token,
            "second.bin",
            "application/octet-stream",
            &[],
            data.clone(),
        )
        .await;
    assert_eq!(r2.status(), 201);
    let up2: serde_json::Value = r2.json().await.unwrap();

    // Same content -> same hash, different file ids, dedup hits on upload 2.
    assert_eq!(up1["content_hash"], up2["content_hash"]);
    assert_ne!(up1["file_id"], up2["file_id"]);
    assert_eq!(
        up2["dedup_chunks"], up1["chunk_count"],
        "second upload must dedup every chunk"
    );
    assert!(up2["dedup_bytes"].as_u64().unwrap() > 0);

    // Index reports one physical chunk per unique hash — uniform content
    // dedups even within a single upload.
    let (chunks, _bytes) = srv.state.0.index.chunk_stats().await.unwrap();
    assert_eq!(
        chunks, 1,
        "uniform 512KiB of 'A' must collapse to one chunk"
    );
    assert!(up1["chunk_count"].as_u64().unwrap() >= 1);

    // Deleting one copy keeps the other readable.
    let id1 = up1["file_id"].as_str().unwrap();
    let id2 = up2["file_id"].as_str().unwrap();
    let del = srv
        .client()
        .delete(format!("{}/api/v1/files/{id1}", srv.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);
    let got = srv.get_file(&token, id2).await;
    assert_eq!(got.status(), 200);
    assert_eq!(got.bytes().await.unwrap().as_ref(), data.as_slice());
}
