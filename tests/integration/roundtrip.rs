//! 2. Raw upload → byte-identical download roundtrip.

use crate::common;

#[tokio::test]
async fn upload_download_roundtrip() {
    let srv = common::spawn_server(common::base_config("rt").0).await;
    let token = srv.admin_token().await;
    let data = common::pseudo_random(1024 * 1024, 12345);

    let resp = srv
        .upload_raw(
            &token,
            "report.bin",
            "application/octet-stream",
            &["q1", "audit"],
            data.clone(),
        )
        .await;
    assert_eq!(
        resp.status(),
        201,
        "upload failed: {}",
        resp.text().await.unwrap()
    );
    let up: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(up["size"], data.len() as u64);
    assert_eq!(up["name"], "report.bin");
    let file_id = up["file_id"].as_str().unwrap().to_string();

    let got = srv.get_file(&token, &file_id).await;
    assert_eq!(got.status(), 200);
    assert_eq!(got.headers()["content-type"], "application/octet-stream");
    assert_eq!(got.headers()["content-length"], data.len().to_string());
    let body = got.bytes().await.unwrap();
    assert_eq!(
        body.as_ref(),
        data.as_slice(),
        "downloaded bytes must match"
    );

    // Meta endpoint agrees.
    let meta = srv
        .client()
        .get(format!("{}/api/v1/files/{file_id}/meta", srv.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(meta.status(), 200);
    let meta: serde_json::Value = meta.json().await.unwrap();
    assert_eq!(meta["chunk_count"], up["chunk_count"]);
    assert_eq!(meta["size"], data.len() as u64);
    assert_eq!(meta["tags"][0], "q1");

    // Content hash equals independently computed BLAKE3.
    let expected = blake3::hash(&data).to_hex().to_string();
    assert_eq!(meta["content_hash"], expected);
}
