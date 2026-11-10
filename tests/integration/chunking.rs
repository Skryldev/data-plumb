//! 5. FastCDC chunking invariants over the API.

use crate::common;

#[tokio::test]
async fn large_file_chunking_invariants() {
    let srv = common::spawn_server(common::base_config("ch").0).await;
    let token = srv.admin_token().await;
    let data = common::pseudo_random(8 * 1024 * 1024, 4242); // 8 MiB

    let resp = srv
        .upload_raw(
            &token,
            "big.bin",
            "application/octet-stream",
            &[],
            data.clone(),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let up: serde_json::Value = resp.json().await.unwrap();
    let file_id = up["file_id"].as_str().unwrap().to_string();

    let meta = srv
        .client()
        .get(format!("{}/api/v1/files/{file_id}/meta", srv.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let meta: serde_json::Value = meta.json().await.unwrap();

    let chunks = meta["chunks"].as_array().expect("chunk list present");
    assert!(
        chunks.len() >= 8,
        "8 MiB must split into multiple chunks (got {})",
        chunks.len()
    );

    let (min, avg, max) = (8192usize, 32768usize, 131072usize);
    let mut offset = 0u64;
    for (i, c) in chunks.iter().enumerate() {
        let co = c["offset"].as_u64().unwrap();
        let size = c["size_original"].as_u64().unwrap() as usize;
        assert_eq!(co, offset, "chunk {i} offset must be contiguous");
        offset += size as u64;
        let is_last = i == chunks.len() - 1;
        assert!(size <= max, "chunk {i} exceeds max: {size}");
        if !is_last {
            assert!(size >= min, "non-final chunk {i} below min: {size}");
        }
        let _ = avg;
    }
    assert_eq!(offset, data.len() as u64);
    assert_eq!(meta["size"], data.len() as u64);

    // Whole-file hash matches client-side BLAKE3.
    let expected = blake3::hash(&data).to_hex().to_string();
    assert_eq!(meta["content_hash"], expected);

    // Download reassembles exactly.
    let got = srv.get_file(&token, &file_id).await;
    assert_eq!(got.bytes().await.unwrap().as_ref(), data.as_slice());
}

#[tokio::test]
async fn empty_file_roundtrip() {
    let srv = common::spawn_server(common::base_config("ch2").0).await;
    let token = srv.admin_token().await;
    let resp = srv
        .upload_raw(
            &token,
            "empty.bin",
            "application/octet-stream",
            &[],
            Vec::new(),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let up: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(up["size"], 0);
    assert_eq!(up["chunk_count"], 0);
    let got = srv.get_file(&token, up["file_id"].as_str().unwrap()).await;
    assert_eq!(got.status(), 200);
    assert!(got.bytes().await.unwrap().is_empty());
}
