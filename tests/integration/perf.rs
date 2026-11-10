//! Performance smoke test (ignored by default):
//!   cargo test --test integration_test perf_smoke --release -- --ignored --nocapture
//!
//! Prints measured throughput numbers used in the README performance table.

use crate::common;

const SIZE: usize = 64 * 1024 * 1024; // 64 MiB

#[tokio::test]
#[ignore = "performance smoke; run with --ignored"]
async fn perf_smoke() {
    let srv = common::spawn_server(common::base_config("perf").0).await;
    let token = srv.admin_token().await;
    let data = common::pseudo_random(SIZE, 7);
    let c = srv.client();

    // ---- upload (multipart-free raw body) ----
    let started = std::time::Instant::now();
    let resp = c
        .post(format!(
            "{}/api/v1/files?name=perf.bin&mime=application/octet-stream",
            srv.base
        ))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/octet-stream")
        .body(data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
    let up: serde_json::Value = resp.json().await.unwrap();
    let upload_secs = started.elapsed().as_secs_f64();
    let file_id = up["file_id"].as_str().unwrap().to_string();

    // ---- download (cold-ish; L1 flushed by size) ----
    let started = std::time::Instant::now();
    let got = srv.get_file(&token, &file_id).await;
    assert_eq!(got.status(), 200);
    let body = got.bytes().await.unwrap();
    let download_secs = started.elapsed().as_secs_f64();
    assert_eq!(body.len(), SIZE);

    // ---- download again (warm caches) ----
    let started = std::time::Instant::now();
    let got = srv.get_file(&token, &file_id).await;
    assert_eq!(got.status(), 200);
    let warm = got.bytes().await.unwrap().len();
    let warm_secs = started.elapsed().as_secs_f64();
    assert_eq!(warm, SIZE);

    // ---- range ----
    let started = std::time::Instant::now();
    let r = c
        .get(format!("{}/api/v1/files/{file_id}/range", srv.base))
        .header("Authorization", format!("Bearer {token}"))
        .header("Range", "bytes=0-1048575")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    let range_secs = started.elapsed().as_secs_f64();

    let stats = srv.state.0.index.global_stats().await.unwrap();
    println!("==== perf_smoke results ({} MiB) ====", SIZE / 1024 / 1024);
    println!(
        "upload:        {:>8.1} MB/s   ({:.3} s)",
        (SIZE as f64 / 1024.0 / 1024.0) / upload_secs,
        upload_secs
    );
    println!(
        "download cold: {:>8.1} MB/s   ({:.3} s)",
        (SIZE as f64 / 1024.0 / 1024.0) / download_secs,
        download_secs
    );
    println!(
        "download warm: {:>8.1} MB/s   ({:.3} s)",
        (SIZE as f64 / 1024.0 / 1024.0) / warm_secs,
        warm_secs
    );
    println!("range 1MiB:    {:>8.1} ms", range_secs * 1000.0);
    println!(
        "dedup ratio:   {:>8.2}x   (logical={} stored={})",
        stats.bytes_logical as f64 / stats.bytes_stored.max(1) as f64,
        stats.bytes_logical,
        stats.bytes_stored
    );
}
