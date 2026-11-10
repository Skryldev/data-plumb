//! 10. Primary → replica replication E2E.

use crate::common;

#[tokio::test]
async fn replication_e2e() {
    // Replica first (it needs no target), then primary pointing at it.
    let mut replica_cfg = common::base_config("repl-r").0;
    replica_cfg.replication.enabled = false;
    replica_cfg.replication.api_key = "test-key".into();
    let replica = common::spawn_server(replica_cfg).await;

    let mut primary_cfg = common::base_config("repl-p").0;
    common::enable_replication(
        &mut primary_cfg,
        &replica.base,
        file_storage::config::ReplicationMode::Async,
        300,
    );
    let primary = common::spawn_server(primary_cfg).await;

    let token = primary.admin_token().await;
    let data = common::pseudo_random(512 * 1024, 20240);

    // Upload on primary.
    let up = primary
        .upload_raw(
            &token,
            "replicated.bin",
            "application/octet-stream",
            &["rep"],
            data.clone(),
        )
        .await;
    assert_eq!(up.status(), 201);
    let up: serde_json::Value = up.json().await.unwrap();
    let file_id = up["file_id"].as_str().unwrap().to_string();

    // Replica does not have it yet (or does after flush); poll until visible.
    let replica_client = replica.client();
    let replica_token = replica.admin_token().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        let r = replica.get_file(&replica_token, &file_id).await;
        if r.status() == 200 {
            let body = r.bytes().await.unwrap();
            assert_eq!(body.as_ref(), data.as_slice(), "replica bytes must match");
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(ok, "file must replicate to the replica within 10s");

    // Replicated manifest is listed on the replica as well.
    let list = replica_client
        .get(format!("{}/api/v1/files", replica.base))
        .header("Authorization", format!("Bearer {replica_token}"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["name"], "replicated.bin");

    // Sync-mode replication: upload blocks until ack, file immediately readable.
    let mut primary2_cfg = common::base_config("repl-p2").0;
    common::enable_replication(
        &mut primary2_cfg,
        &replica.base,
        file_storage::config::ReplicationMode::Sync,
        5_000,
    );
    let primary2 = common::spawn_server(primary2_cfg).await;
    let token2 = primary2.admin_token().await;
    let r = primary2
        .upload_raw(
            &token2,
            "sync.bin",
            "application/octet-stream",
            &[],
            b"sync-me".to_vec(),
        )
        .await;
    assert_eq!(
        r.status(),
        201,
        "sync upload must succeed with healthy replica"
    );
    let up2: serde_json::Value = r.json().await.unwrap();
    let r = replica
        .get_file(&replica_token, up2["file_id"].as_str().unwrap())
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"sync-me");
}

#[tokio::test]
async fn internal_endpoints_apply_bundles() {
    let mut replica_cfg = common::base_config("int-r").0;
    replica_cfg.replication.enabled = false;
    let replica = common::spawn_server(replica_cfg).await;

    // Craft a bundle by uploading to a standalone primary-less pipeline.
    let mut primary_cfg = common::base_config("int-p").0;
    primary_cfg.replication.enabled = false;
    let primary = common::spawn_server(primary_cfg).await;
    let token = primary.admin_token().await;
    let data = common::pseudo_random(128 * 1024, 606);
    let up = primary
        .upload_raw(
            &token,
            "bundle.bin",
            "application/octet-stream",
            &[],
            data.clone(),
        )
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let file_id: uuid::Uuid = up["file_id"].as_str().unwrap().parse().unwrap();

    // Pull the bundle from primary /internal/file/:id and push into replica.
    let bundle = primary
        .client()
        .get(format!("{}/internal/file/{file_id}", primary.base))
        .header("X-Api-Key", "test-internal-key")
        .send()
        .await
        .unwrap();
    assert_eq!(bundle.status(), 200);
    let bundle = bundle.bytes().await.unwrap();

    let applied = replica
        .client()
        .post(format!("{}/internal/file/{file_id}", replica.base))
        .header("X-Api-Key", "test-internal-key")
        .header("Content-Type", "application/octet-stream")
        .body(bundle)
        .send()
        .await
        .unwrap();
    assert_eq!(applied.status(), 201);

    // Replica now serves the file (tenant seeded by bundle apply).
    let rep_token = replica.admin_token().await;
    let r = replica.get_file(&rep_token, &file_id.to_string()).await;
    // The replica admin belongs to tenant A; bundle file belongs to tenant A too.
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap().as_ref(), data.as_slice());
}
