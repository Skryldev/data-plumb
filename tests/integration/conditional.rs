//! 7. Conditional GET (ETag / 304).

use crate::common;

#[tokio::test]
async fn conditional_get_304() {
    let srv = common::spawn_server(common::base_config("et").0).await;
    let token = srv.admin_token().await;
    let data = common::pseudo_random(64 * 1024, 2718);

    let resp = srv
        .upload_raw(&token, "etag.bin", "application/octet-stream", &[], data)
        .await;
    let up: serde_json::Value = resp.json().await.unwrap();
    let id = up["file_id"].as_str().unwrap().to_string();
    let url = format!("{}/api/v1/files/{id}", srv.base);
    let c = srv.client();

    // First GET returns 200 + strong ETag.
    let r = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"'));
    assert_eq!(etag.len(), 66); // quoted 64-hex

    // If-None-Match with matching etag -> 304, no body.
    let r = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 304);
    assert_eq!(r.bytes().await.unwrap().len(), 0);

    // W/ prefix also matches.
    let r = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("If-None-Match", format!("W/{etag}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 304);

    // Mismatched etag -> 200 with full body.
    let r = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("If-None-Match", "\"deadbeef\"")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // ETag equals the content hash (content addressing).
    assert_eq!(
        etag.trim_matches('"'),
        blake3::hash(&common::pseudo_random(64 * 1024, 2718))
            .to_hex()
            .to_string()
    );
}
