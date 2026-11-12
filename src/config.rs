//! Configuration: YAML file + `FS_` environment overrides (`__` = nesting).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

use crate::error::{internal, StorageError};

/// C-01: literals that must never authenticate a boot. Keep in sync with
/// `docs/src/operations/bootstrap.md`; extend when the repo ships new
/// example configs. Matching is a substring check on `auth.jwt_secret` and
/// `replication.api_key`.
pub const FORBIDDEN_SECRET_MARKERS: &[&str] = &[
    "change-me",
    "admin123",
    "alice123",
    "bob123",
    "viewer123",
    "internal-dev-api-key",
    "cluster-api-key",
    "insecure",
    "replace_with",
];

/// Build profile used by [`AppConfig::validate_for`]. Debug builds may use
/// plaintext passwords as a local-dev convenience; release builds require
/// argon2id PHC strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityProfile {
    Debug,
    Release,
}

/// Root configuration object, mirroring `config/default.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub database: DatabaseConfig,
    pub chunking: ChunkingConfig,
    pub compression: CompressionConfig,
    pub cache: CacheConfig,
    pub auth: AuthConfig,
    pub encryption: EncryptionConfig,
    pub rate_limit: RateLimitConfig,
    pub replication: ReplicationConfig,
    pub retention: RetentionConfig,
    pub limits: LimitsConfig,
    /// H-05/M-17: exposure policy for the ops endpoints.
    pub metrics: MetricsConfig,
    /// H-03: upload content validation policy.
    pub validation: ValidationConfig,
    pub logging: LoggingConfig,
    pub tenants: Vec<TenantSeed>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Port of the gRPC listener served alongside REST (same host).
    /// `0` binds an ephemeral port (useful in tests).
    pub grpc_port: u16,
    /// Optional TLS/mTLS for the gRPC listener (server identity + optional
    /// client-CA root for peer verification).
    pub grpc_tls: GrpcTlsConfig,
}

/// TLS material for the gRPC listener.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GrpcTlsConfig {
    pub enabled: bool,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// When set, enables mTLS: client certificates must chain to this CA.
    pub client_ca_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StorageConfig {
    pub root: PathBuf,
    pub temp_dir: PathBuf,
    pub shard_depth: u32,
    /// M-07: ceiling for the quarantine directory that holds untrusted
    /// blobs rejected by repair verification. Oldest-mtime files are
    /// evicted first. Not subject to tenant quota — it is an ops surface.
    #[serde(default = "default_quarantine_max_bytes")]
    pub quarantine_max_bytes: u64,
    /// M-07: quarantine files older than this are swept at boot.
    #[serde(default = "default_quarantine_retention_secs")]
    pub quarantine_retention_secs: u64,
}

fn default_quarantine_max_bytes() -> u64 {
    1024 * 1024 * 1024 // 1 GiB
}

fn default_quarantine_retention_secs() -> u64 {
    7 * 24 * 3600 // 7 days
}

impl StorageConfig {
    /// M-07: quarantine lives OUTSIDE the CAS blob walk (a sibling of the
    /// blocks dir) so reconcile/GC never sees quarantined bytes as blobs.
    pub fn quarantine_dir(&self) -> PathBuf {
        self.root
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("quarantine")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DatabaseConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChunkingConfig {
    pub min: usize,
    pub avg: usize,
    pub max: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompressionConfig {
    pub level: i32,
    pub min_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CacheConfig {
    pub l1_max_bytes: u64,
    pub l1_ttl_secs: u64,
    pub l2_max_bytes: u64,
    pub l2_ttl_secs: u64,
    /// Deprecated in favor of `manifest_max_bytes` (H-01): kept parsed for
    /// config compatibility, no longer drives the L3 bound.
    pub manifest_capacity: u64,
    pub manifest_ttl_secs: u64,
    /// H-01: byte-based L3 budget (weighed by manifest size).
    #[serde(default = "default_manifest_max_bytes")]
    pub manifest_max_bytes: u64,
}

fn default_manifest_max_bytes() -> u64 {
    64 * 1024 * 1024 // 64 MiB
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub access_ttl_secs: u64,
    pub refresh_ttl_secs: u64,
    pub users: Vec<UserConfig>,
    /// M-05: per-username exponential login lockout. Security control —
    /// defaulted ON (fail-closed on secrets-adjacent paths).
    #[serde(default)]
    pub login_throttle: ThrottleConfig,
}

/// M-05: exponential backoff for repeated login failures. `check` runs
/// BEFORE the argon2 verify (cheap-fail protects the 64 MiB budget);
/// lockout doubles per consecutive failure up to `lockout_cap_ms` and
/// resets on success. The throttle itself is fail-OPEN (a cache miss or
/// internal error must never block a legitimate login) while the
/// verifier underneath stays fail-CLOSED (rule 17).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThrottleConfig {
    pub enabled: bool,
    /// Lockout after the FIRST failure.
    pub base_ms: u64,
    /// Multiplier applied per additional consecutive failure.
    pub multiplier: f64,
    /// Never lock out longer than this.
    pub lockout_cap_ms: u64,
    /// Bound on tracked usernames (oldest-evicted, never wholesale-cleared).
    pub map_capacity: u64,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_ms: 1_000,
            multiplier: 2.0,
            lockout_cap_ms: 15 * 60 * 1_000,
            map_capacity: 10_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    pub username: String,
    /// Plaintext password — development convenience only.
    #[serde(default)]
    pub password: Option<String>,
    /// Argon2id PHC string — preferred for production.
    #[serde(default)]
    pub password_hash: Option<String>,
    pub tenant_id: Uuid,
    pub role: Role,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
    Viewer,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
            Role::Viewer => "viewer",
        }
    }

    pub fn can_write(&self) -> bool {
        matches!(self, Role::Admin | Role::User)
    }

    pub fn is_admin(&self) -> bool {
        matches!(self, Role::Admin)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EncryptionConfig {
    pub enabled: bool,
    pub keys_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub per_second: u32,
    pub burst: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReplicationConfig {
    pub enabled: bool,
    pub mode: ReplicationMode,
    /// Cluster role of THIS node (C-02): replicas must never run refcount
    /// GC or refcount-based purges — reclamation is reachability-only.
    #[serde(default)]
    pub role: NodeRole,
    pub target_url: Option<String>,
    pub upstream_url: Option<String>,
    pub api_key: String,
    pub queue_capacity: usize,
    pub batch_size: usize,
    pub flush_interval_ms: u64,
    pub sync_timeout_ms: u64,
    /// M-14: bounded enqueue wait before the fail-open drop / sync failure.
    #[serde(default = "default_enqueue_timeout_ms")]
    pub enqueue_timeout_ms: u64,
}

fn default_enqueue_timeout_ms() -> u64 {
    5_000
}

/// Node role in the replication topology (C-02 guard).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    #[default]
    Primary,
    Replica,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplicationMode {
    Sync,
    #[default]
    Async,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RetentionConfig {
    pub gc_mode: GcMode,
    pub grace_period_secs: u64,
    pub gc_interval_secs: u64,
    pub orphan_min_age_secs: u64,
    /// M-01: seconds between drift-reconciliation passes (0 disables).
    /// The pass tombstones refcount>0 rows with zero manifest references
    /// and lets the normal sweep reclaim them — the permanent guarantee
    /// behind the transaction-sized C-03 crash window.
    #[serde(default = "default_reconcile_interval_secs")]
    pub reconcile_interval_secs: u64,
}

fn default_reconcile_interval_secs() -> u64 {
    3600
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GcMode {
    /// Primary mode: reference counting + soft-delete grace period.
    #[default]
    Refcount,
    /// Replica mode: periodic reachability scan over manifests.
    Reachability,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LimitsConfig {
    pub max_file_size: u64,
    pub max_body: usize,
    /// H-01: global byte budget for in-flight upload reservations.
    #[serde(default = "default_in_flight_bytes")]
    pub max_in_flight_bytes: u64,
    /// H-01: global cap on concurrent upload pipeline tasks.
    #[serde(default = "default_max_concurrent_uploads")]
    pub max_concurrent_uploads: usize,
}

fn default_in_flight_bytes() -> u64 {
    2 * 1024 * 1024 * 1024 // 2 GiB
}

fn default_max_concurrent_uploads() -> usize {
    64
}

/// H-05/M-17: how `/metrics` (and other ops endpoints) may be reached.
/// Default `loopback` — fail-closed; `public` is an explicit operator
/// opt-in that logs a startup warning naming the risk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricsExposure {
    /// Only loopback peers may fetch /metrics.
    #[default]
    Loopback,
    /// Any peer presenting the configured api key.
    ApiKey,
    /// Public (dev only) — startup warning.
    Public,
}

/// H-03: content validation policy. Default `enforce` (fail-closed) — the
/// pipeline checks the declared MIME against the sniffed magic bytes of the
/// first plaintext chunk BEFORE persisting anything. `off` is an explicit
/// operator opt-out (logged at boot); the safe serving headers apply
/// regardless (defense in depth).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValidationMode {
    #[default]
    Enforce,
    Off,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ValidationConfig {
    #[serde(default)]
    pub mode: ValidationMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetricsConfig {
    #[serde(default)]
    pub exposure: MetricsExposure,
    /// Required when exposure = api_key (constant-time compared).
    #[serde(default)]
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoggingConfig {
    pub format: String,
    pub level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantSeed {
    pub id: Uuid,
    pub name: String,
}

impl AppConfig {
    /// Loads YAML from `path`, then applies `FS_`-prefixed env overrides.
    pub fn load(path: &str) -> Result<Self, StorageError> {
        let settings = config::Config::builder()
            .add_source(config::File::new(path, config::FileFormat::Yaml))
            .add_source(
                config::Environment::with_prefix("FS")
                    .prefix_separator("_")
                    .separator("__")
                    .try_parsing(true)
                    .ignore_empty(true),
            )
            .build()
            .map_err(internal)?;
        settings.try_deserialize::<AppConfig>().map_err(internal)
    }

    /// Baseline config used by unit/integration tests. Callers override paths,
    /// ports and toggles through the returned struct.
    pub fn for_tests() -> Self {
        let tenant_a: Uuid = "01930000-0000-7000-8000-000000000001".parse().unwrap();
        let tenant_b: Uuid = "01930000-0000-7000-8000-000000000002".parse().unwrap();
        let tenant_c: Uuid = "01930000-0000-7000-8000-000000000003".parse().unwrap();
        AppConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 0,
                grpc_port: 0,
                grpc_tls: GrpcTlsConfig::default(),
            },
            storage: StorageConfig {
                root: PathBuf::from("data/blocks"),
                temp_dir: PathBuf::from("data/tmp"),
                shard_depth: 2,
                quarantine_max_bytes: default_quarantine_max_bytes(),
                quarantine_retention_secs: default_quarantine_retention_secs(),
            },
            database: DatabaseConfig {
                path: PathBuf::from("data/index.redb"),
            },
            chunking: ChunkingConfig {
                min: 8192,
                avg: 32768,
                max: 131072,
            },
            compression: CompressionConfig {
                level: 3,
                min_size: 1024,
            },
            cache: CacheConfig {
                l1_max_bytes: 64 * 1024 * 1024,
                l1_ttl_secs: 300,
                l2_max_bytes: 128 * 1024 * 1024,
                l2_ttl_secs: 3600,
                manifest_capacity: 10_000,
                manifest_ttl_secs: 60,
                manifest_max_bytes: 64 * 1024 * 1024,
            },
            auth: AuthConfig {
                // C-01: >= 32 chars, no shipped literal. Test-only value.
                jwt_secret: "test-only-secret-0123456789abcdef-0123456789abcdef".into(),
                access_ttl_secs: 900,
                refresh_ttl_secs: 604_800,
                login_throttle: ThrottleConfig::default(),
                users: vec![
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
                        username: "bob".into(),
                        password: Some("bob123".into()),
                        password_hash: None,
                        tenant_id: tenant_c,
                        role: Role::User,
                    },
                    UserConfig {
                        username: "viewer".into(),
                        password: Some("viewer123".into()),
                        password_hash: None,
                        tenant_id: tenant_b,
                        role: Role::Viewer,
                    },
                ],
            },
            encryption: EncryptionConfig {
                enabled: false,
                keys_dir: PathBuf::from("data/keys"),
            },
            rate_limit: RateLimitConfig {
                enabled: true,
                per_second: 5_000,
                burst: 10_000,
            },
            replication: ReplicationConfig {
                enabled: false,
                mode: ReplicationMode::Async,
                role: crate::config::NodeRole::Primary,
                target_url: None,
                upstream_url: None,
                api_key: "test-internal-key-0123456789abcdef".into(),
                queue_capacity: 10_000,
                batch_size: 64,
                flush_interval_ms: 5_000,
                sync_timeout_ms: 5_000,
                enqueue_timeout_ms: 5_000,
            },
            retention: RetentionConfig {
                gc_mode: GcMode::Refcount,
                grace_period_secs: 86_400,
                gc_interval_secs: 300,
                orphan_min_age_secs: 3_600,
                reconcile_interval_secs: 0, // tests: reconcile is admin-triggered
            },
            limits: LimitsConfig {
                max_file_size: 1024 * 1024 * 1024,
                max_body: 1024 * 1024 * 1024,
                max_in_flight_bytes: 512 * 1024 * 1024,
                max_concurrent_uploads: 32,
            },
            metrics: MetricsConfig::default(),
            validation: ValidationConfig::default(),
            logging: LoggingConfig {
                format: "pretty".into(),
                level: "info".into(),
            },
            tenants: vec![
                TenantSeed {
                    id: tenant_a,
                    name: "default".into(),
                },
                TenantSeed {
                    id: tenant_b,
                    name: "acme".into(),
                },
                TenantSeed {
                    id: tenant_c,
                    name: "globex".into(),
                },
            ],
        }
    }

    /// Cross-field sanity checks. Fails fast at boot instead of corrupting data.
    ///
    /// C-01: fail-closed on secrets. Uses the build's own profile
    /// (release builds reject plaintext passwords outright); see
    /// [`validate_for`](Self::validate_for) for the profile-explicit form.
    pub fn validate(&self) -> Result<(), StorageError> {
        let profile = if cfg!(debug_assertions) {
            SecurityProfile::Debug
        } else {
            SecurityProfile::Release
        };
        self.validate_for(profile)
    }

    /// C-01: profile-explicit validation reporting ALL violations at once.
    ///
    /// * Every shipped/known literal is rejected in every profile.
    /// * `auth.jwt_secret` needs >= 32 random bytes, `replication.api_key` >= 24.
    /// * Plaintext `password` is a Debug-profile convenience; Release requires
    ///   argon2id `password_hash` PHC strings.
    pub fn validate_for(&self, profile: SecurityProfile) -> Result<(), StorageError> {
        let mut errs: Vec<String> = Vec::new();

        // -- secrets ------------------------------------------------------
        let jwt = &self.auth.jwt_secret;
        if jwt.len() < 32 {
            errs.push(format!(
                "auth.jwt_secret: {} chars, need >= 32 random bytes (openssl rand -hex 32)",
                jwt.len()
            ));
        }
        if FORBIDDEN_SECRET_MARKERS.iter().any(|m| jwt.contains(m)) {
            errs.push(
                "auth.jwt_secret: matches a shipped/known literal; generate a fresh secret"
                    .to_string(),
            );
        }
        let api = &self.replication.api_key;
        if api.len() < 24 {
            errs.push(format!(
                "replication.api_key: {} chars, need >= 24",
                api.len()
            ));
        }
        if FORBIDDEN_SECRET_MARKERS.iter().any(|m| api.contains(m)) {
            errs.push("replication.api_key: matches a shipped/known literal".to_string());
        }

        // -- users --------------------------------------------------------
        for (i, u) in self.auth.users.iter().enumerate() {
            match (&u.password, &u.password_hash) {
                (Some(_), Some(_)) => errs.push(format!(
                    "auth.users[{i}] '{}': set exactly one of `password` (debug) or `password_hash`",
                    u.username
                )),
                (Some(_), None) if profile == SecurityProfile::Release => errs.push(format!(
                    "auth.users[{i}] '{}': plaintext `password` is forbidden in release builds; \
                     use `password_hash` (argon2id PHC string)",
                    u.username
                )),
                (None, None) => errs.push(format!(
                    "auth.users[{i}] '{}': no credential configured",
                    u.username
                )),
                (None, Some(h)) if !h.starts_with("$argon2id$") => errs.push(format!(
                    "auth.users[{i}] '{}': password_hash must be an argon2id PHC string",
                    u.username
                )),
                _ => {}
            }
        }
        if self.auth.users.is_empty() {
            errs.push("at least one auth user is required".to_string());
        }

        // -- structural checks (unchanged semantics, collected) -----------
        if self.chunking.min == 0
            || self.chunking.avg < self.chunking.min
            || self.chunking.max < self.chunking.avg
        {
            errs.push("chunking must satisfy min <= avg <= max and min > 0".to_string());
        }
        if self.chunking.max > 16 * 1024 * 1024 {
            errs.push("chunking.max must be <= 16 MiB".to_string());
        }
        if !(1..=22).contains(&self.compression.level) {
            errs.push("compression.level must be in 1..=22".to_string());
        }
        if self.replication.enabled {
            if self.replication.target_url.is_none() {
                errs.push(
                    "replication.target_url is required when replication is enabled".to_string(),
                );
            }
            if self.replication.batch_size == 0 {
                errs.push("replication.batch_size must be > 0".to_string());
            }
        }
        if self.storage.shard_depth > 4 {
            errs.push("storage.shard_depth must be <= 4".to_string());
        }
        if self.server.grpc_tls.enabled
            && (self.server.grpc_tls.cert_path.as_os_str().is_empty()
                || self.server.grpc_tls.key_path.as_os_str().is_empty())
        {
            errs.push("server.grpc_tls requires cert_path and key_path".to_string());
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(StorageError::Validation(errs.join("; ")))
        }
    }

    /// Points all state directories at a fresh temporary root (tests).
    pub fn with_data_root(mut self, root: &std::path::Path) -> Self {
        self.storage.root = root.join("blocks");
        self.storage.temp_dir = root.join("tmp");
        self.database.path = root.join("index.redb");
        self.encryption.keys_dir = root.join("keys");
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- C-01 gates -------------------------------------------------------

    #[test]
    fn c01_config_rejects_every_shipped_literal() {
        for marker in FORBIDDEN_SECRET_MARKERS {
            let mut cfg = AppConfig::for_tests();
            cfg.auth.jwt_secret = format!("operator-supplied-{marker}-padding-0123456789");
            assert!(
                cfg.validate_for(SecurityProfile::Release).is_err(),
                "jwt_secret containing shipped literal '{marker}' must not boot"
            );
            let mut cfg = AppConfig::for_tests();
            cfg.replication.api_key = format!("operator-supplied-{marker}-padding-0123456789");
            assert!(
                cfg.validate_for(SecurityProfile::Release).is_err(),
                "api_key containing shipped literal '{marker}' must not boot"
            );
        }
    }

    #[test]
    fn c01_config_rejects_plaintext_password_in_release() {
        let cfg = AppConfig::for_tests();
        // Debug profile accepts plaintext (local-dev convenience).
        cfg.validate_for(SecurityProfile::Debug)
            .expect("debug profile allows plaintext passwords");
        // Release profile rejects it.
        let err = cfg
            .validate_for(SecurityProfile::Release)
            .expect_err("release profile must reject plaintext passwords");
        assert!(
            err.to_string().contains("forbidden in release builds"),
            "message must name the rule: {err}"
        );
    }

    #[test]
    fn c01_config_reports_all_violations() {
        let mut cfg = AppConfig::for_tests();
        cfg.auth.jwt_secret = "short".into();
        cfg.replication.api_key = "change-me-in-production".into();
        let err = cfg.validate_for(SecurityProfile::Release).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("auth.jwt_secret"),
            "both violations reported: {msg}"
        );
        assert!(
            msg.contains("replication.api_key"),
            "both violations reported: {msg}"
        );
    }

    #[test]
    fn c01_config_rejects_short_secrets() {
        let mut cfg = AppConfig::for_tests();
        cfg.auth.jwt_secret = "x".repeat(31);
        assert!(cfg.validate_for(SecurityProfile::Release).is_err());
        let mut cfg = AppConfig::for_tests();
        cfg.replication.api_key = "x".repeat(23);
        assert!(cfg.validate_for(SecurityProfile::Release).is_err());
    }

    #[test]
    fn c01_config_rejects_non_argon2id_hash() {
        let mut cfg = AppConfig::for_tests();
        cfg.auth.users[0].password = None;
        cfg.auth.users[0].password_hash = Some("$argon2i$v=19$...".into());
        assert!(cfg.validate_for(SecurityProfile::Release).is_err());
    }

    #[test]
    fn for_tests_passes_validation() {
        let cfg = AppConfig::for_tests();
        cfg.validate().expect("default test config must validate");
    }

    #[test]
    fn reject_bad_chunking() {
        let mut cfg = AppConfig::for_tests();
        cfg.chunking.avg = 1024;
        cfg.chunking.min = 4096;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn reject_short_jwt_secret() {
        let mut cfg = AppConfig::for_tests();
        cfg.auth.jwt_secret = "short".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn replication_requires_target() {
        let mut cfg = AppConfig::for_tests();
        cfg.replication.enabled = true;
        cfg.replication.target_url = None;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn user_needs_credential() {
        let mut cfg = AppConfig::for_tests();
        cfg.auth.users[0].password = None;
        cfg.auth.users[0].password_hash = None;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn env_override_applies() {
        unsafe {
            std::env::set_var("FS_SERVER__PORT", "9999");
            std::env::set_var("FS_ENCRYPTION__ENABLED", "true");
        }
        let cfg = AppConfig::for_tests();
        let settings = config::Config::builder()
            .add_source(config::Config::try_from(&cfg).expect("serialize"))
            .add_source(
                config::Environment::with_prefix("FS")
                    .prefix_separator("_")
                    .separator("__")
                    .try_parsing(true)
                    .ignore_empty(true),
            )
            .build()
            .unwrap();
        let merged: AppConfig = settings.try_deserialize().unwrap();
        assert_eq!(merged.server.port, 9999);
        assert!(merged.encryption.enabled);
        unsafe {
            std::env::remove_var("FS_SERVER__PORT");
            std::env::remove_var("FS_ENCRYPTION__ENABLED");
        }
    }
}
