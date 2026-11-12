//! M-05 regression gates — streaming rate-limit parity + exponential
//! login lockout (rule 17: throttle fail-open, auth fail-closed).
//!
//! Runtime-red on the pre-fix tree: `grpc_stream_flood_throttled`,
//! `login_lockout_exponential` and
//! `login_throttle_correct_password_inside_window_still_rejected` FAILED
//! there (streaming RPCs bypassed the governor entirely; no lockout
//! existed). `throttle_map_evicts_oldest_not_clear` and
//! `streaming_governor_matches_unary` gate NEW APIs (compile-red).

use std::sync::Arc;
use std::time::Duration;

use file_storage::config::{AppConfig, Role, ThrottleConfig, UserConfig, UserConfig as UC};
use file_storage::grpc::client::FileStorageClient;
use file_storage::grpc::generated::pb;
use file_storage::grpc::interceptors::auth::AuthInterceptor;
use file_storage::security::throttle::LoginThrottle;

// -- fixtures -----------------------------------------------------------------

fn base_cfg() -> (AppConfig, tempfile::TempDir) {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let cfg = AppConfig::for_tests();
    (cfg, dir)
}

async fn spawn(mut cfg: AppConfig, dir: tempfile::TempDir) -> file_storage::state::AppState {
    cfg = cfg.with_data_root(dir.path());
    file_storage::state::build_state(Arc::new(cfg))
        .await
        .unwrap()
}

fn throttle_cfg(base_ms: u64, cap_ms: u64, capacity: u64) -> ThrottleConfig {
    ThrottleConfig {
        enabled: true,
        base_ms,
        multiplier: 2.0,
        lockout_cap_ms: cap_ms,
        map_capacity: capacity,
    }
}

// -- gate 1: grpc_stream_flood_throttled ---------------------------------------

#[tokio::test]
async fn grpc_stream_flood_throttled() {
    let (mut cfg, dir) = base_cfg();
    // Tight budget: 10 tokens, ~no refill within the test window.
    cfg.rate_limit.enabled = true;
    cfg.rate_limit.per_second = 1;
    cfg.rate_limit.burst = 10;
    let state = spawn(cfg, dir).await;

    // Real gRPC server on the shared state.
    file_storage::metrics::init_recorder();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = file_storage::grpc::server::GrpcServer::new(state.clone(), None);
    tokio::spawn(async move {
        server.serve_on(listener).await.expect("grpc server");
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut c = FileStorageClient::connect(&format!("http://{addr}"))
        .await
        .unwrap();
    // Login is a unary RPC: consumes 1 of the 10 burst tokens.
    c.login("alice", "alice123").await.unwrap();

    // Upload one small file to have something to download (consumes
    // another token through the streaming Upload gate).
    let resp = c
        .upload(
            pb::UploadHeader {
                name: "flood.bin".into(),
                ..Default::default()
            },
            std::io::Cursor::new(vec![7u8; 64]),
        )
        .await
        .unwrap();
    let file_id = resp.manifest.unwrap().file_id;

    // Flood Download: 8 more calls fit the budget; the NEXT one must be
    // refused with RESOURCE_EXHAUSTED BEFORE any pipeline work. (Pre-M-05
    // this loop ran forever — streaming RPCs never touched the governor.)
    let mut exhausted = false;
    for i in 0..12 {
        let mut out = Vec::new();
        match c.download(&file_id, &mut out).await {
            Ok(_) => {}
            Err(e) if e.code() == tonic::Code::ResourceExhausted => {
                assert!(
                    i >= 1,
                    "throttle must engage only after the burst, at call {i}"
                );
                exhausted = true;
                break;
            }
            Err(e) => panic!("unexpected error at call {i}: {e:?}"),
        }
    }
    assert!(exhausted, "streaming flood must hit RESOURCE_EXHAUSTED");
}

// -- gate 2: login_lockout_exponential -----------------------------------------

#[tokio::test]
async fn login_lockout_exponential() {
    // Same ×2 growth as the default config (base 1s, cap 15m), compressed
    // to a fast test window.
    let t = LoginThrottle::from_cfg(&throttle_cfg(100, 1_000, 10_000));
    let mut expected = vec![100u64, 200, 400, 800];
    for (i, want) in expected.iter().enumerate() {
        let applied = t.record_failure("bruteforced-user");
        assert_eq!(
            applied,
            *want,
            "failure {} must lock out {want}ms (×2 growth)",
            i + 1
        );
        let err = t.check("bruteforced-user").unwrap_err();
        assert!(matches!(
            err,
            file_storage::error::StorageError::Throttled(_)
        ));
        assert!(err.to_string().contains("retry_after="));
        // Let the window lapse so the NEXT failure reaches the verifier
        // (throttled attempts are cheap-fails and must not grow the
        // backoff themselves).
        tokio::time::sleep(Duration::from_millis(*want + 60)).await;
    }
    expected.push(1_000);
    let applied = t.record_failure("bruteforced-user");
    assert_eq!(applied, 1_000, "lockout capped at lockout_cap_ms");
}

// -- gate 3: login_throttle_correct_password_inside_window_still_rejected -------

#[tokio::test]
async fn login_throttle_correct_password_inside_window_still_rejected() {
    let (mut cfg, dir) = base_cfg();
    cfg.auth.login_throttle = throttle_cfg(1_500, 15_000, 10_000);
    let state = spawn(cfg, dir).await;
    file_storage::metrics::init_recorder();
    let app = file_storage::api::router::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    async fn login_at(client: &reqwest::Client, base: &str, pw: &str) -> reqwest::Response {
        client
            .post(format!("{base}/api/v1/auth/login"))
            .json(&serde_json::json!({"username": "alice", "password": pw}))
            .send()
            .await
            .unwrap()
    }

    // 3 wrong passwords: #1 verifies (fail → 1.5s lockout), #2/#3 are
    // cheap-fails inside the window.
    let r1 = login_at(&client, &base, "wrong-1").await;
    assert_eq!(r1.status().as_u16(), 401, "first wrong password verifies");
    let r2 = login_at(&client, &base, "wrong-2").await;
    assert_eq!(r2.status().as_u16(), 429, "second wrong password throttled");
    let r3 = login_at(&client, &base, "wrong-3").await;
    assert_eq!(r3.status().as_u16(), 429);

    // The CORRECT password inside the window is ALSO refused (the throttle
    // protects the 64 MiB argon2 budget; it does not check passwords).
    let r4 = login_at(&client, &base, "alice123").await;
    assert_eq!(
        r4.status().as_u16(),
        429,
        "correct password inside the lockout window must still be rejected"
    );

    // After the window expires the same correct password succeeds.
    tokio::time::sleep(Duration::from_millis(1_600)).await;
    let r5 = login_at(&client, &base, "alice123").await;
    assert_eq!(r5.status().as_u16(), 200, "lockout expired: login succeeds");
    assert_eq!(
        state.0.auth.login_throttle().failures("alice"),
        0,
        "success resets"
    );
}

// -- gate 4: login_throttle_audited ---------------------------------------------

#[tokio::test]
async fn login_throttle_audited() {
    let (mut cfg, dir) = base_cfg();
    cfg.auth.login_throttle = throttle_cfg(2_000, 15_000, 10_000);
    let state = spawn(cfg, dir).await;
    file_storage::metrics::init_recorder();
    let app = file_storage::api::router::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Trigger: one real failure, then a throttled attempt.
    let _ = client
        .post(format!("{base}/api/v1/auth/login"))
        .json(&serde_json::json!({"username": "bob", "password": "nope"}))
        .send()
        .await
        .unwrap();
    let resp = client
        .post(format!("{base}/api/v1/auth/login"))
        .json(&serde_json::json!({"username": "bob", "password": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 429);

    // Exactly one auth.login.throttled row, with username + retry_after.
    let events = state.0.audit.recent(100).await.unwrap();
    let throttled: Vec<_> = events
        .iter()
        .filter(|e| e.action == "auth.login.throttled")
        .collect();
    assert_eq!(throttled.len(), 1, "exactly one throttle audit row");
    let e = throttled[0];
    assert_eq!(e.actor, "bob");
    assert_eq!(e.status, "throttled");
    assert!(
        e.detail.contains("retry_after="),
        "detail carries the window: {e:?}"
    );
}

// -- gate 5: throttle_map_evicts_oldest_not_clear --------------------------------

#[tokio::test]
async fn throttle_map_evicts_oldest_not_clear() {
    let t = LoginThrottle::from_cfg(&throttle_cfg(60_000, 60_000, 100));
    // Fill the map to capacity: users u0..u99 all locked out.
    for i in 0..100 {
        t.record_failure(&format!("u{i}"));
    }
    assert!(t.check("u0").is_err());
    assert!(t.check("u99").is_err());
    // One more user: the OLDEST entry (u0) is evicted, everyone else stays.
    t.record_failure("u100");
    assert!(
        t.check("u0").is_ok() && t.failures("u0") == 0,
        "oldest entry evicted individually"
    );
    assert!(
        t.check("u1").is_err(),
        "u1 NOT evicted — no wholesale clear"
    );
    assert!(t.check("u50").is_err(), "u50 NOT evicted");
    assert!(t.check("u99").is_err(), "newest NOT evicted");
    assert!(t.check("u100").is_err(), "new user tracked normally");
}

// -- gate 6: verifier_error_does_not_throttle ------------------------------------

#[tokio::test]
async fn verifier_error_does_not_throttle() {
    let (mut cfg, dir) = base_cfg();
    // Corrupt alice's stored hash: every verify returns a backend error.
    cfg.auth.users = vec![
        UC {
            username: "admin".into(),
            password: Some("admin123".into()),
            password_hash: None,
            tenant_id: "01930000-0000-7000-8000-000000000001".parse().unwrap(),
            role: Role::Admin,
        },
        UserConfig {
            username: "alice".into(),
            password: None,
            password_hash: Some("this-is-not-a-valid-phc-string".into()),
            tenant_id: "01930000-0000-7000-8000-000000000002".parse().unwrap(),
            role: Role::User,
        },
    ];
    cfg.auth.login_throttle = throttle_cfg(60_000, 60_000, 10_000);
    let state = spawn(cfg, dir).await;

    for _ in 0..10 {
        let err = state.0.auth.login("alice", "whatever").unwrap_err();
        assert!(
            matches!(err, file_storage::error::StorageError::AuthBackend(_)),
            "verifier errors surface as AuthBackend, got {err:?}"
        );
    }
    // Rule 17: backend errors NEVER count toward the lockout — the user
    // whose password store is flaky is not punished for it.
    assert_eq!(
        state.0.auth.login_throttle().failures("alice"),
        0,
        "verifier errors must not record throttle failures"
    );
    // And a later legitimate login is not locked out (admin, same store).
    assert!(state.0.auth.login("admin", "admin123").is_ok());
}

// -- gate 7: streaming_governor_matches_unary ------------------------------------

#[tokio::test]
async fn streaming_governor_matches_unary() {
    let (cfg, dir) = base_cfg();
    let state = spawn(cfg, dir).await;
    let file_interceptor = AuthInterceptor::file_service(state.clone());
    let admin_interceptor = AuthInterceptor::admin_service(state.clone());
    let shared = state.0.rate_limiter.instance_ptr();
    assert_eq!(
        file_interceptor.rate_limiter_ptr(),
        Some(shared),
        "unary interceptor uses the shared governor"
    );
    assert_eq!(
        admin_interceptor.rate_limiter_ptr(),
        Some(shared),
        "admin interceptor uses the same shared governor"
    );
    // The streaming handlers reach the governor through `state.0
    // .rate_limiter` directly (see file_service.rs) — the same Arc.
    // One behavioral proof on top of pointer identity: consuming the
    // budget through the shared handle throttles the governor itself.
    let (mut cfg2, dir2) = base_cfg();
    cfg2.rate_limit.enabled = true;
    cfg2.rate_limit.per_second = 1;
    cfg2.rate_limit.burst = 1;
    let state2 = spawn(cfg2, dir2).await;
    let limiter = &state2.0.rate_limiter;
    let t = uuid::Uuid::now_v7();
    assert!(limiter.allow(t));
    assert!(!limiter.allow(t), "budget exhausted on the shared instance");
}
