//! 6. Range requests (206 / 416).

use crate::common;

#[tokio::test]
async fn range_requests() {
    let srv = common::spawn_server(common::base_config("rg").0).await;
    let token = srv.admin_token().await;
    let data = common::pseudo_random(1024 * 1024, 31337);
    let resp = srv
        .upload_raw(
            &token,
            "range.bin",
            "application/octet-stream",
            &[],
            data.clone(),
        )
        .await;
    let up: serde_json::Value = resp.json().await.unwrap();
    let id = up["file_id"].as_str().unwrap().to_string();
    let url = format!("{}/api/v1/files/{id}/range", srv.base);
    let c = srv.client();

    // closed range
    let r = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Range", "bytes=100-199")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(
        r.headers()["content-range"],
        format!("bytes 100-199/{}", data.len())
    );
    assert_eq!(r.headers()["content-length"], "100");
    assert_eq!(r.bytes().await.unwrap().as_ref(), &data[100..200]);

    // open-ended
    let r = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Range", "bytes=1048566-")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.unwrap().as_ref(), &data[1048566..]);

    // suffix
    let r = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Range", "bytes=-50")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.unwrap().as_ref(), &data[1048576 - 50..]);

    // unsatisfiable -> 416 with Content-Range: bytes */size
    let r = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Range", "bytes=2000000-")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 416);
    assert_eq!(
        r.headers()["content-range"],
        format!("bytes */{}", data.len())
    );

    // invalid unit -> 400
    let r = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Range", "chunks=0-1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // multi-range: first range served (documented behaviour)
    let r = c
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Range", "bytes=0-4,100-104")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.unwrap().as_ref(), &data[0..5]);
}

#[tokio::test]
async fn range_on_cross_chunk_boundary() {
    let srv = common::spawn_server(common::base_config("rg2").0).await;
    let token = srv.admin_token().await;
    let data = common::pseudo_random(600_000, 5);
    let resp = srv
        .upload_raw(
            &token,
            "b.bin",
            "application/octet-stream",
            &[],
            data.clone(),
        )
        .await;
    let up: serde_json::Value = resp.json().await.unwrap();
    let meta = srv
        .client()
        .get(format!(
            "{}/api/v1/files/{}/meta",
            srv.base,
            up["file_id"].as_str().unwrap()
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let meta: serde_json::Value = meta.json().await.unwrap();
    assert!(
        meta["chunk_count"].as_u64().unwrap() >= 2,
        "test needs multiple chunks"
    );

    // Slice spanning several chunks.
    let start = 200_000usize;
    let end = 400_001usize;
    let r = srv
        .client()
        .get(format!(
            "{}/api/v1/files/{}/range",
            srv.base,
            up["file_id"].as_str().unwrap()
        ))
        .header("Authorization", format!("Bearer {token}"))
        .header("Range", format!("bytes={start}-{}", end - 1))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.unwrap().as_ref(), &data[start..end]);
}
