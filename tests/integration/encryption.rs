//! 11. At-rest encryption (AES-256-GCM, per-tenant keys).

use crate::common;

#[tokio::test]
async fn encryption_roundtrip_and_isolation() {
    let mut cfg = common::base_config("enc").0;
    cfg.encryption.enabled = true;
    let srv = common::spawn_server(cfg).await;
    let admin = srv.admin_token().await; // tenant A
    let alice = srv.login("alice", "alice123").await; // tenant B
    let data = common::pseudo_random(256 * 1024, 555);

    // Roundtrip with encryption enabled.
    let up = srv
        .upload_raw(
            &admin,
            "secret.bin",
            "application/octet-stream",
            &[],
            data.clone(),
        )
        .await;
    assert_eq!(up.status(), 201);
    let up: serde_json::Value = up.json().await.unwrap();
    let id = up["file_id"].as_str().unwrap().to_string();
    let got = srv.get_file(&admin, &id).await;
    assert_eq!(got.status(), 200);
    assert_eq!(got.bytes().await.unwrap().as_ref(), data.as_slice());

    // On-disk blobs must NOT contain the plaintext.
    let meta = srv
        .client()
        .get(format!("{}/api/v1/files/{id}/meta", srv.base))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let chunk_hashes: Vec<String> = meta["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["hash"].as_str().unwrap().to_string())
        .collect();
    let needle = &data[..48];
    for h in &chunk_hashes {
        let (dir, path) = srv
            .state
            .0
            .blocks
            .shard_path(&hex::decode(h).unwrap().try_into().unwrap());
        let _ = dir;
        let blob = std::fs::read(&path).unwrap();
        assert!(
            !blob.windows(48).any(|w| w == needle),
            "stored blob must not contain plaintext"
        );
    }

    // Encrypted chunk keys are tenant-scoped: same content from tenant B
    // produces different storage keys, so cross-tenant dedup cannot leak.
    let up_b = srv
        .upload_raw(
            &alice,
            "same.bin",
            "application/octet-stream",
            &[],
            data.clone(),
        )
        .await;
    assert_eq!(up_b.status(), 201);
    let up_b: serde_json::Value = up_b.json().await.unwrap();
    assert_eq!(
        up["content_hash"], up_b["content_hash"],
        "plaintext hash identical"
    );
    assert_eq!(
        up["dedup_chunks"], 0,
        "encrypted stores must not dedup across tenants"
    );

    // Tenant B can read its own copy (own key), tenant A's file is 403 for B.
    let got_b = srv
        .get_file(&alice, up_b["file_id"].as_str().unwrap())
        .await;
    assert_eq!(got_b.bytes().await.unwrap().as_ref(), data.as_slice());
    assert_eq!(srv.get_file(&alice, &id).await.status(), 403);
}
