//! Integration test harness: spawns real HTTP servers on ephemeral ports.
#![allow(dead_code)]

use file_storage::config::{AppConfig, ReplicationMode, Role, UserConfig};
use file_storage::state::AppState;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

pub const TENANT_A: &str = "01930000-0000-7000-8000-000000000001";
pub const TENANT_B: &str = "01930000-0000-7000-8000-000000000002";

pub struct TestServer {
    pub base: String,
    pub state: AppState,
    /// Keeps the data dir alive for the lifetime of the server.
    pub _dir: Option<tempfile::TempDir>,
}

impl TestServer {
    pub fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap()
    }

    pub async fn login(&self, username: &str, password: &str) -> String {
        let resp = self
            .client()
            .post(format!("{}/api/v1/auth/login", self.base))
            .json(&serde_json::json!({"username": username, "password": password}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "login failed for {username}");
        let body: serde_json::Value = resp.json().await.unwrap();
        body["access_token"].as_str().unwrap().to_string()
    }

    pub async fn admin_token(&self) -> String {
        self.login("admin", "admin123").await
    }

    pub async fn upload_raw(
        &self,
        token: &str,
        name: &str,
        mime: &str,
        tags: &[&str],
        body: Vec<u8>,
    ) -> reqwest::Response {
        let mut query = vec![("name", name.to_string()), ("mime", mime.to_string())];
        if !tags.is_empty() {
            query.push(("tags", tags.join(",")));
        }
        self.client()
            .post(format!("{}/api/v1/files", self.base))
            .query(&query)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", mime)
            .body(body)
            .send()
            .await
            .unwrap()
    }

    pub async fn get_file(&self, token: &str, file_id: &str) -> reqwest::Response {
        self.client()
            .get(format!("{}/api/v1/files/{file_id}", self.base))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap()
    }
}

/// Builds a config rooted at a fresh temp dir.
pub fn base_config(tag: &str) -> (AppConfig, tempfile::TempDir) {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).expect("tempdir");
    let tenant_a: Uuid = TENANT_A.parse().unwrap();
    let tenant_b: Uuid = TENANT_B.parse().unwrap();
    let tenant_c: Uuid = "01930000-0000-7000-8000-000000000003".parse().unwrap();
    let mut cfg = AppConfig::for_tests();
    cfg = cfg.with_data_root(dir.path());
    // M-05: the shared harness performs deliberate failed logins (authz
    // scenarios); the per-username lockout would cross-contaminate tests
    // on one server. The throttle gates (tests/regression/m05_throttle.rs)
    // enable it explicitly on their own servers.
    cfg.auth.login_throttle.enabled = false;
    cfg.auth.users = vec![
        UserConfig {
            username: "admin".into(),
            password: Some("admin123".into()),
            password_hash: None,
            tenant_id: tenant_a,
            role: Role::Admin,
        },
        UserConfig {
            username: "alice".into(),
            password: Some("alice123".into()),
            password_hash: None,
            tenant_id: tenant_b,
            role: Role::User,
        },
        UserConfig {
            username: "viewer".into(),
            password: Some("viewer123".into()),
            password_hash: None,
            tenant_id: tenant_b,
            role: Role::Viewer,
        },
    ];
    cfg.tenants = vec![
        file_storage::config::TenantSeed {
            id: tenant_a,
            name: "default".into(),
        },
        file_storage::config::TenantSeed {
            id: tenant_b,
            name: "acme".into(),
        },
        file_storage::config::TenantSeed {
            id: tenant_c,
            name: "globex".into(),
        },
    ];
    let _ = tag;
    (cfg, dir)
}

/// Boots a full HTTP server on 127.0.0.1:0. `dir` is kept alive by the
/// returned handle.
pub async fn spawn_server_with_dir(cfg: AppConfig, dir: tempfile::TempDir) -> TestServer {
    file_storage::metrics::init_recorder();
    let state = file_storage::state::build_state(Arc::new(cfg))
        .await
        .unwrap();
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
    TestServer {
        base: format!("http://{addr}"),
        state,
        _dir: Some(dir),
    }
}

/// Convenience: builds a fresh config + dir and boots the server.
pub async fn spawn_server(mut cfg: AppConfig) -> TestServer {
    let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    cfg = cfg.with_data_root(dir.path());
    spawn_server_with_dir(cfg, dir).await
}

/// Deterministic pseudo-random bytes.
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

/// Builder-style helper: enables async replication toward `target`.
pub fn enable_replication(cfg: &mut AppConfig, target: &str, mode: ReplicationMode, flush_ms: u64) {
    cfg.replication.enabled = true;
    cfg.replication.mode = mode;
    cfg.replication.target_url = Some(target.to_string());
    cfg.replication.flush_interval_ms = flush_ms;
    cfg.replication.api_key = "test-key".into();
}
