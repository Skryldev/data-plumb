//! Authentication: JWT access tokens (HS256), argon2id password hashing and
//! rotating refresh tokens.

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use dashmap::DashMap;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::config::{AuthConfig, Role, UserConfig};
use crate::error::StorageError;

/// JWT claims for an access token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub tenant: Uuid,
    pub role: String,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
}

/// Authenticated request context injected by the auth middleware.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub username: String,
    pub tenant_id: Uuid,
    pub role: Role,
}

impl AuthContext {
    pub fn require_admin(&self) -> Result<(), StorageError> {
        if self.role.is_admin() {
            Ok(())
        } else {
            Err(StorageError::Forbidden("admin role required".into()))
        }
    }

    pub fn require_write(&self) -> Result<(), StorageError> {
        if self.role.can_write() {
            Ok(())
        } else {
            Err(StorageError::Forbidden("write role required".into()))
        }
    }
}

#[derive(Debug, Clone)]
struct RefreshEntry {
    username: String,
    tenant_id: Uuid,
    role: Role,
    expires_at: Instant,
}

/// Token pair returned by login/refresh.
#[derive(Debug, Clone, Serialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
}

pub struct AuthService {
    users: Vec<UserConfig>,
    encoding: EncodingKey,
    decoding: DecodingKey,
    access_ttl: Duration,
    refresh_ttl: Duration,
    refreshes: DashMap<String, RefreshEntry>,
    /// M-05: per-username exponential login lockout (checked before the
    /// argon2 verify; see security::throttle for the fail-open contract).
    throttle: Arc<crate::security::throttle::LoginThrottle>,
    /// Constant-cost dummy hash so unknown users burn the same argon2 time
    /// as wrong passwords (mitigates username enumeration by timing).
    dummy_hash: String,
}

impl AuthService {
    pub fn new(cfg: &AuthConfig) -> Self {
        let dummy = Argon2::default()
            .hash_password(
                b"timing-equalizer-placeholder",
                &SaltString::encode_b64(&{
                    let mut b = [0u8; 16];
                    rand::thread_rng().fill_bytes(&mut b);
                    b
                })
                .expect("salt encode"),
            )
            .expect("dummy hash")
            .to_string();
        Self {
            users: cfg.users.clone(),
            encoding: EncodingKey::from_secret(cfg.jwt_secret.as_bytes()),
            decoding: DecodingKey::from_secret(cfg.jwt_secret.as_bytes()),
            access_ttl: Duration::from_secs(cfg.access_ttl_secs),
            refresh_ttl: Duration::from_secs(cfg.refresh_ttl_secs),
            refreshes: DashMap::new(),
            throttle: crate::security::throttle::LoginThrottle::from_cfg(&cfg.login_throttle),
            dummy_hash: dummy,
        }
    }

    /// M-05: read-only view of the throttle (used by gate tests and the
    /// audit wiring; never bypasses it).
    pub fn login_throttle(&self) -> &crate::security::throttle::LoginThrottle {
        &self.throttle
    }

    fn verify_password(&self, user: &UserConfig, password: &str) -> Result<bool, StorageError> {
        if let Some(phc) = &user.password_hash {
            let parsed = PasswordHash::new(phc)
                .map_err(|e| StorageError::AuthBackend(format!("stored hash invalid: {e}")))?;
            return Ok(Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok());
        }
        if let Some(plain) = &user.password {
            return Ok(constant_time_eq(plain.as_bytes(), password.as_bytes()));
        }
        Ok(false)
    }

    /// Validates credentials and returns a fresh token pair.
    ///
    /// M-05 order of operations (rule 17):
    /// 1. throttle check BEFORE argon2 (cheap-fail protects the 64 MiB
    ///    verify budget);
    /// 2. verify — fail-CLOSED: a verifier/backend error propagates and is
    ///    NOT recorded as a throttle failure (a flaky password store must
    ///    not lock out its user);
    /// 3. `InvalidCredentials` records a throttle failure;
    /// 4. success resets the backoff.
    pub fn login(&self, username: &str, password: &str) -> Result<TokenPair, StorageError> {
        self.throttle.check(username)?;
        let user = self.users.iter().find(|u| u.username == username);
        let verify = match user {
            Some(u) => self.verify_password(u, password),
            None => {
                // burn comparable time for unknown users
                let burned = PasswordHash::new(&self.dummy_hash).ok().and_then(|h| {
                    Argon2::default()
                        .verify_password(password.as_bytes(), &h)
                        .ok()
                });
                let _ = burned;
                Ok(false)
            }
        };
        // Verifier errors (corrupted/invalid stored hash) are fail-closed:
        // they NEVER authenticate and NEVER count toward the lockout.
        let ok = verify?;
        if !ok {
            self.throttle.record_failure(username);
            return Err(StorageError::Unauthorized("invalid credentials".into()));
        }
        let user = user.expect("checked above");
        self.throttle.record_success(username);
        Ok(self.issue_pair(&user.username, user.tenant_id, user.role))
    }

    fn issue_pair(&self, username: &str, tenant_id: Uuid, role: Role) -> TokenPair {
        let now = now_secs();
        let claims = Claims {
            sub: username.to_string(),
            tenant: tenant_id,
            role: role.as_str().to_string(),
            iat: now,
            exp: now + self.access_ttl.as_secs() as i64,
            jti: Uuid::now_v7().to_string(),
        };
        let access = encode(
            &Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &self.encoding,
        )
        .expect("HS256 encode cannot fail with valid key");
        let refresh = Uuid::now_v7().to_string();
        self.refreshes.insert(
            refresh.clone(),
            RefreshEntry {
                username: username.to_string(),
                tenant_id,
                role,
                expires_at: Instant::now() + self.refresh_ttl,
            },
        );
        TokenPair {
            access_token: access,
            refresh_token: refresh,
            token_type: "Bearer",
            expires_in: self.access_ttl.as_secs(),
        }
    }

    /// Exchanges a valid, unexpired refresh token for a new pair (rotation).
    pub fn refresh(&self, refresh_token: &str) -> Result<TokenPair, StorageError> {
        // drop expired entries lazily
        self.refreshes.retain(|_, e| e.expires_at > Instant::now());
        let entry = self
            .refreshes
            .get(refresh_token)
            .map(|e| e.clone())
            .ok_or_else(|| StorageError::Unauthorized("invalid refresh token".into()))?;
        self.refreshes.remove(refresh_token);
        Ok(self.issue_pair(&entry.username, entry.tenant_id, entry.role))
    }

    /// Invalidates a refresh token (logout). Returns true when it existed.
    pub fn logout(&self, refresh_token: &str) -> bool {
        self.refreshes.remove(refresh_token).is_some()
    }

    /// Verifies an access token and rebuilds the auth context.
    pub fn verify_access(&self, token: &str) -> Result<AuthContext, StorageError> {
        let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.validate_exp = true;
        validation.leeway = 5;
        let data = decode::<Claims>(token, &self.decoding, &validation)
            .map_err(|e| StorageError::Unauthorized(format!("invalid access token: {e}")))?;
        let role = data
            .claims
            .role
            .parse::<Role>()
            .map_err(|_| StorageError::Unauthorized("unknown role in token".into()))?;
        Ok(AuthContext {
            username: data.claims.sub,
            tenant_id: data.claims.tenant,
            role,
        })
    }

    /// Active (unexpired) refresh sessions — diagnostics.
    pub fn active_sessions(&self) -> usize {
        self.refreshes.retain(|_, e| e.expires_at > Instant::now());
        self.refreshes.len()
    }

    pub fn user_count(&self) -> usize {
        self.users.len()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl std::str::FromStr for Role {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "admin" => Ok(Role::Admin),
            "user" => Ok(Role::User),
            "viewer" => Ok(Role::Viewer),
            other => Err(StorageError::Unauthorized(format!(
                "unknown role '{other}'"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuthConfig;
    use std::sync::Arc;
    use uuid::Uuid;

    fn service() -> (Arc<AuthService>, Uuid) {
        let tenant: Uuid = "01930000-0000-7000-8000-000000000001".parse().unwrap();
        let cfg = AuthConfig {
            jwt_secret: "test-secret-0123456789abcdef".into(),
            access_ttl_secs: 60,
            refresh_ttl_secs: 600,
            login_throttle: crate::config::ThrottleConfig::default(),
            users: vec![
                UserConfig {
                    username: "alice".into(),
                    password: Some("wonderland".into()),
                    password_hash: None,
                    tenant_id: tenant,
                    role: Role::User,
                },
                UserConfig {
                    username: "root".into(),
                    password: Some("rootpw".into()),
                    password_hash: None,
                    tenant_id: tenant,
                    role: Role::Admin,
                },
                UserConfig {
                    username: "hashed".into(),
                    password: None,
                    password_hash: Some(
                        Argon2::default()
                            .hash_password(b"argonpw", &SaltString::encode_b64(&[7u8; 16]).unwrap())
                            .unwrap()
                            .to_string(),
                    ),
                    tenant_id: tenant,
                    role: Role::Viewer,
                },
            ],
        };
        (Arc::new(AuthService::new(&cfg)), tenant)
    }

    #[test]
    fn login_success_and_claims() {
        let (svc, tenant) = service();
        let pair = svc.login("alice", "wonderland").unwrap();
        let ctx = svc.verify_access(&pair.access_token).unwrap();
        assert_eq!(ctx.username, "alice");
        assert_eq!(ctx.tenant_id, tenant);
        assert_eq!(ctx.role, Role::User);
    }

    #[test]
    fn argon2_hashed_user_can_login() {
        let (svc, _) = service();
        let pair = svc.login("hashed", "argonpw").unwrap();
        let ctx = svc.verify_access(&pair.access_token).unwrap();
        assert_eq!(ctx.role, Role::Viewer);
        assert!(svc.login("hashed", "wrong").is_err());
    }

    #[test]
    fn wrong_credentials_rejected() {
        let (svc, _) = service();
        assert!(svc.login("alice", "nope").is_err());
        assert!(svc.login("ghost", "nope").is_err());
        assert!(svc.login("alice", "").is_err());
    }

    #[test]
    fn refresh_rotation() {
        let (svc, _) = service();
        let p1 = svc.login("alice", "wonderland").unwrap();
        assert_eq!(svc.active_sessions(), 1);
        let p2 = svc.refresh(&p1.refresh_token).unwrap();
        assert_eq!(svc.active_sessions(), 1, "old token rotated away");
        assert!(svc.refresh(&p1.refresh_token).is_err(), "reuse must fail");
        assert!(svc.verify_access(&p2.access_token).is_ok());
    }

    #[test]
    fn logout_invalidates() {
        let (svc, _) = service();
        let p = svc.login("alice", "wonderland").unwrap();
        assert!(svc.logout(&p.refresh_token));
        assert!(!svc.logout(&p.refresh_token));
        assert!(svc.refresh(&p.refresh_token).is_err());
    }

    #[test]
    fn tampered_token_rejected() {
        let (svc, _) = service();
        let p = svc.login("alice", "wonderland").unwrap();
        let mut parts: Vec<String> = p.access_token.split('.').map(String::from).collect();
        parts[2].push('x');
        assert!(svc.verify_access(&parts.join(".")).is_err());
        assert!(svc.verify_access("garbage").is_err());
        assert!(svc.verify_access("").is_err());
    }

    #[test]
    fn role_gate_helpers() {
        let (svc, _) = service();
        let admin = svc
            .verify_access(&svc.login("root", "rootpw").unwrap().access_token)
            .unwrap();
        let viewer = svc
            .verify_access(&svc.login("hashed", "argonpw").unwrap().access_token)
            .unwrap();
        assert!(admin.require_admin().is_ok());
        assert!(admin.require_write().is_ok());
        assert!(viewer.require_admin().is_err());
        assert!(viewer.require_write().is_err());
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn role_parse() {
        assert_eq!("admin".parse::<Role>().unwrap(), Role::Admin);
        assert_eq!("user".parse::<Role>().unwrap(), Role::User);
        assert_eq!("viewer".parse::<Role>().unwrap(), Role::Viewer);
        assert!("root".parse::<Role>().is_err());
    }
}
