//! 9. Soft delete, purge and GC.

use crate::common;

#[tokio::test]
async fn delete_soft_then_purge_and_gc() {
    let mut cfg = common::base_config("del").0;
    cfg.retention.grace_period_secs = 1;
    cfg.retention.gc_interval_secs = 1;
    cfg.retention.orphan_min_age_secs = 1;
    let srv = common::spawn_server(cfg).await;
    let admin = srv.admin_token().await;
    let data = common::pseudo_random(256 * 1024, 99);

    let up = srv
        .upload_raw(
            &admin,
            "doomed.bin",
            "application/octet-stream",
            &[],
            data.clone(),
        )
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let id = up["file_id"].as_str().unwrap().to_string();

    // Soft delete -> 204, then 404 on reads.
    let del = srv
        .client()
        .delete(format!("{}/api/v1/files/{id}", srv.base))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);
    assert_eq!(srv.get_file(&admin, &id).await.status(), 404);
    assert!(
        srv.state
            .0
            .index
            .get_manifest(&uuid::Uuid::parse_str(&id).unwrap())
            .await
            .unwrap()
            .unwrap()
            .is_deleted(),
        "soft-deleted manifest must persist"
    );

    // Upload a second file sharing the same content; its chunks must survive
    // the first file's purge (refcounted).
    let up2 = srv
        .upload_raw(
            &admin,
            "twin.bin",
            "application/octet-stream",
            &[],
            data.clone(),
        )
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let id2 = up2["file_id"].as_str().unwrap().to_string();

    // Hard purge (admin only) the soft-deleted one.
    let purge = srv
        .client()
        .delete(format!("{}/api/v1/files/{id}?purge=true", srv.base))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .unwrap();
    assert_eq!(purge.status(), 204);
    assert!(
        srv.state
            .0
            .index
            .get_manifest(&uuid::Uuid::parse_str(&id).unwrap())
            .await
            .unwrap()
            .is_none(),
        "purged manifest must be gone"
    );
    // Twin still readable.
    assert_eq!(srv.get_file(&admin, &id2).await.status(), 200);

    // GC pass runs; nothing else to purge.
    let gc = srv
        .client()
        .post(format!("{}/api/v1/admin/gc", srv.base))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .unwrap();
    assert_eq!(gc.status(), 200);
    let report: serde_json::Value = gc.json().await.unwrap();
    assert_eq!(report["manifests_purged"], 0);
}
