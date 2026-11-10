//! 3. Multipart upload with name/tags/mime fields.

use crate::common;

#[tokio::test]
async fn multipart_upload_with_metadata() {
    let srv = common::spawn_server(common::base_config("mp").0).await;
    let token = srv.admin_token().await;

    let data = common::pseudo_random(300_000, 777);
    let part = reqwest::multipart::Part::bytes(data.clone())
        .file_name("raw-upload.bin")
        .mime_str("application/x-custom")
        .unwrap();

    let form = reqwest::multipart::Form::new()
        .text("name", "invoice-2024.pdf")
        .text("tags", "finance,q3,important")
        .text("mime", "application/pdf")
        .part("file", part);

    let resp = srv
        .client()
        .post(format!("{}/api/v1/files", srv.base))
        .header("Authorization", format!("Bearer {token}"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
    let up: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(up["name"], "invoice-2024.pdf");
    assert_eq!(up["mime"], "application/pdf");
    assert_eq!(up["size"], data.len() as u64);

    // Listed by tag.
    let list = srv
        .client()
        .get(format!("{}/api/v1/files?tag=finance", srv.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let list: serde_json::Value = list.json().await.unwrap();
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["name"], "invoice-2024.pdf");

    // Listed by exact name.
    let list = srv
        .client()
        .get(format!("{}/api/v1/files?name=invoice-2024.pdf", srv.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let list: serde_json::Value = list.json().await.unwrap();
    assert_eq!(list["total"], 1);

    // Download roundtrip.
    let file_id = up["file_id"].as_str().unwrap();
    let got = srv.get_file(&token, file_id).await;
    assert_eq!(got.bytes().await.unwrap().as_ref(), data.as_slice());
}

#[tokio::test]
async fn multipart_without_file_field_rejected() {
    let srv = common::spawn_server(common::base_config("mp2").0).await;
    let token = srv.admin_token().await;
    let form = reqwest::multipart::Form::new().text("name", "no-file");
    let resp = srv
        .client()
        .post(format!("{}/api/v1/files", srv.base))
        .header("Authorization", format!("Bearer {token}"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "validation_failed");
}
