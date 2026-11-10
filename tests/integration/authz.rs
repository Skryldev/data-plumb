//! 8. Authentication & authorization matrix.

use crate::common;

#[tokio::test]
async fn auth_flow_and_roles() {
    let srv = common::spawn_server(common::base_config("az").0).await;
    let c = srv.client();
    let base = &srv.base;

    // 401 without token.
    let r = c.get(format!("{base}/api/v1/files")).send().await.unwrap();
    assert_eq!(r.status(), 401);

    // 401 with garbage token.
    let r = c
        .get(format!("{base}/api/v1/files"))
        .header("Authorization", "Bearer not.a.jwt")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // 401 wrong scheme.
    let r = c
        .get(format!("{base}/api/v1/files"))
        .header("Authorization", "Basic dXNlcjpwYXNz")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // 401 wrong password.
    let r = c
        .post(format!("{base}/api/v1/auth/login"))
        .json(&serde_json::json!({"username": "admin", "password": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // 401 unknown user.
    let r = c
        .post(format!("{base}/api/v1/auth/login"))
        .json(&serde_json::json!({"username": "ghost", "password": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // Login ok, refresh rotation works, old refresh token rejected.
    let login = c
        .post(format!("{base}/api/v1/auth/login"))
        .json(&serde_json::json!({"username": "alice", "password": "alice123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 200);
    let pair: serde_json::Value = login.json().await.unwrap();
    let access = pair["access_token"].as_str().unwrap();
    let refresh = pair["refresh_token"].as_str().unwrap();

    let refreshed = c
        .post(format!("{base}/api/v1/auth/refresh"))
        .json(&serde_json::json!({"refresh_token": refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(refreshed.status(), 200);
    let new_pair: serde_json::Value = refreshed.json().await.unwrap();
    assert_ne!(new_pair["access_token"], access);

    // Old refresh token is rotated away.
    let reuse = c
        .post(format!("{base}/api/v1/auth/refresh"))
        .json(&serde_json::json!({"refresh_token": refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(reuse.status(), 401);

    // Alice (user) can upload.
    let r = srv
        .upload_raw(
            new_pair["access_token"].as_str().unwrap(),
            "a.bin",
            "application/octet-stream",
            &[],
            b"hello".to_vec(),
        )
        .await;
    assert_eq!(r.status(), 201);

    // Viewer can read but not upload/delete.
    let viewer = srv.login("viewer", "viewer123").await;
    let r = srv
        .upload_raw(
            &viewer,
            "v.bin",
            "application/octet-stream",
            &[],
            b"x".to_vec(),
        )
        .await;
    assert_eq!(r.status(), 403);
    let r = c
        .get(format!("{base}/api/v1/files"))
        .header("Authorization", format!("Bearer {viewer}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Cross-tenant access forbidden: viewer (tenant B) cannot read alice's
    // file? alice IS tenant B — upload as admin (tenant A) then access as alice.
    let admin = srv.admin_token().await;
    let up = srv
        .upload_raw(
            &admin,
            "admin-file.bin",
            "application/octet-stream",
            &[],
            b"secret".to_vec(),
        )
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let admin_file = up["file_id"].as_str().unwrap();
    let r = srv.get_file(&viewer, admin_file).await;
    assert_eq!(r.status(), 403);

    // Admin (tenant A) can access own file; alice cannot delete it.
    let r = c
        .delete(format!("{base}/api/v1/files/{admin_file}"))
        .header("Authorization", format!("Bearer {viewer}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // Tenant admin endpoints are admin-only.
    let r = c
        .post(format!("{base}/api/v1/tenants"))
        .header("Authorization", format!("Bearer {viewer}"))
        .json(&serde_json::json!({"name": "sneaky"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .post(format!("{base}/api/v1/tenants"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({"name": "newco"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // Internal endpoints reject bad api keys.
    let r = c
        .post(format!("{base}/internal/manifest"))
        .body(b"x" as &[u8])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = c
        .post(format!("{base}/internal/manifest"))
        .header("X-Api-Key", "wrong")
        .body(b"x" as &[u8])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}
