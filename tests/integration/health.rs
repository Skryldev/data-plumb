//! 1. Health & readiness probes.

use crate::common;

#[tokio::test]
async fn health_and_ready() {
    let cfg = common::base_config("health").0;
    let srv = common::spawn_server(cfg).await;
    let c = srv.client();

    let h = c.get(format!("{}/health", srv.base)).send().await.unwrap();
    assert_eq!(h.status(), 200);
    let body: serde_json::Value = h.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["uptime_secs"].is_u64());

    let r = c.get(format!("{}/ready", srv.base)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["database"], true);
    assert_eq!(body["storage"], true);

    // Metrics endpoint renders Prometheus text.
    let m = c.get(format!("{}/metrics", srv.base)).send().await.unwrap();
    assert_eq!(m.status(), 200);
    let text = m.text().await.unwrap();
    assert!(
        text.contains("file_storage_"),
        "metrics exposition missing families"
    );
}
