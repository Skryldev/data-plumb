//! Configuration: YAML file + `FS_` environment overrides (`__` = nesting).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

use crate::error::{internal, StorageError};

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
    pub manifest_capacity: u64,
    pub manifest_ttl_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub access_ttl_secs: u64,
    pub refresh_ttl_secs: u64,
    pub users: Vec<UserConfig>,
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
    pub target_url: Option<String>,
    pub upstream_url: Option<String>,
    pub api_key: String,
    pub queue_capacity: usize,
    pub batch_size: usize,
    pub flush_interval_ms: u64,
    pub sync_timeout_ms: u64,
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
            },
            auth: AuthConfig {
                jwt_secret: "test-secret-0123456789abcdef".into(),
                access_ttl_secs: 900,
                refresh_ttl_secs: 604_800,
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
                target_url: None,
                upstream_url: None,
                api_key: "test-internal-key".into(),
                queue_capacity: 10_000,
                batch_size: 64,
                flush_interval_ms: 5_000,
                sync_timeout_ms: 5_000,
            },
            retention: RetentionConfig {
                gc_mode: GcMode::Refcount,
                grace_period_secs: 86_400,
                gc_interval_secs: 300,
                orphan_min_age_secs: 3_600,
            },
            limits: LimitsConfig {
                max_file_size: 1024 * 1024 * 1024,
                max_body: 1024 * 1024 * 1024,
            },
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
    pub fn validate(&self) -> Result<(), StorageError> {
        if self.chunking.min == 0
            || self.chunking.avg < self.chunking.min
            || self.chunking.max < self.chunking.avg
        {
            return Err(StorageError::Validation(
                "chunking must satisfy min <= avg <= max and min > 0".into(),
            ));
        }
        if self.chunking.max > 16 * 1024 * 1024 {
            return Err(StorageError::Validation(
                "chunking.max must be <= 16 MiB".into(),
            ));
        }
        if !(1..=22).contains(&self.compression.level) {
            return Err(StorageError::Validation(
                "compression.level must be in 1..=22".into(),
            ));
        }
        if self.auth.jwt_secret.len() < 16 {
            return Err(StorageError::Validation(
                "auth.jwt_secret must be >= 16 chars".into(),
            ));
        }
        if self.auth.users.is_empty() {
            return Err(StorageError::Validation(
                "at least one auth user is required".into(),
            ));
        }
        for u in &self.auth.users {
            if u.password.is_none() && u.password_hash.is_none() {
                return Err(StorageError::Validation(format!(
                    "user {} needs `password` or `password_hash`",
                    u.username
                )));
            }
        }
        if self.replication.enabled {
            if self.replication.target_url.is_none() {
                return Err(StorageError::Validation(
                    "replication.target_url is required when replication is enabled".into(),
                ));
            }
            if self.replication.batch_size == 0 {
                return Err(StorageError::Validation(
                    "replication.batch_size must be > 0".into(),
                ));
            }
        }
        if self.storage.shard_depth > 4 {
            return Err(StorageError::Validation(
                "storage.shard_depth must be <= 4".into(),
            ));
        }
        if self.server.grpc_tls.enabled
            && (self.server.grpc_tls.cert_path.as_os_str().is_empty()
                || self.server.grpc_tls.key_path.as_os_str().is_empty())
        {
            return Err(StorageError::Validation(
                "server.grpc_tls requires cert_path and key_path".into(),
            ));
        }
        Ok(())
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
