//! 12. Rate limiting (token bucket, keyed by peer IP).

use crate::common;

#[tokio::test]
async fn rate_limit_429() {
    let mut cfg = common::base_config("rl").0;
    cfg.rate_limit.enabled = true;
    cfg.rate_limit.per_second = 1;
    cfg.rate_limit.burst = 3;
    let srv = common::spawn_server(cfg).await;
    let c = srv.client();

    // Burn the 3-token burst with rapid logins (valid + invalid both count).
    for _ in 0..3 {
        let r = c
            .post(format!("{}/api/v1/auth/login", srv.base))
            .json(&serde_json::json!({"username": "admin", "password": "wrong"}))
            .send()
            .await
            .unwrap();
        assert!(
            r.status() == 401,
            "first 3 requests must pass the limiter (got {})",
            r.status()
        );
    }
    // 4th request inside the same second -> 429.
    let r = c
        .post(format!("{}/api/v1/auth/login", srv.base))
        .json(&serde_json::json!({"username": "admin", "password": "admin123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429, "burst exhaustion must yield 429");

    // Health/metrics endpoints are outside the limiter.
    assert_eq!(
        c.get(format!("{}/health", srv.base))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.get(format!("{}/metrics", srv.base))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}
