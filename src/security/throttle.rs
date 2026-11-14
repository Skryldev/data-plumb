//! M-05: per-username exponential login lockout.
//!
//! Rule 17 (V5): the throttle is fail-OPEN, the auth verifier is
//! fail-CLOSED.
//!
//! * `check` runs BEFORE the argon2 verify — a locked-out username is
//!   refused without burning the 64 MiB verify budget (cheap-fail).
//! * A cache miss NEVER blocks a login (fail-open).
//! * `record_failure` is called ONLY for `InvalidCredentials` — a
//!   `VerifierError`/backend error must not lock out a user whose
//!   password store is flaky.
//! * `record_success` resets the backoff.
//! * The map is bounded at `map_capacity` entries with per-entry TTL —
//!   moka evicts individually (oldest/least-recently-used), it never
//!   wholesale-clears (the I-06 bug pattern is structurally impossible
//!   here).
//!
//! Audit events are emitted by the CALLER (the login handlers), keeping
//! a single audit point per surface.

use std::sync::{Arc, Mutex};

use crate::config::ThrottleConfig;
use crate::core::bounded_map::BoundedMap;
use crate::error::StorageError;

#[derive(Debug, Clone)]
struct BackoffState {
    consecutive_failures: u32,
    locked_until_ms: u64,
}

/// Clonable, shareable login throttle (Arc inside).
#[derive(Clone)]
pub struct LoginThrottle {
    cfg: ThrottleConfig,
    /// Bounded insertion-order map (shared oldest-eviction helper —
    /// never wholesale-cleared, I-06 fix).
    state: Arc<Mutex<BoundedMap<String, BackoffState>>>,
    /// V6-15: per-IP ladder with its OWN map + LONGER cap — a username-map
    /// eviction cannot reset the IP-level budget, and an IP window outlasts
    /// the username window (attacker cannot wait out the IP while the
    /// victim's username entry TTLs).
    ip_state: Arc<Mutex<BoundedMap<std::net::IpAddr, BackoffState>>>,
}

impl LoginThrottle {
    pub fn from_cfg(cfg: &ThrottleConfig) -> Arc<Self> {
        Arc::new(Self {
            cfg: cfg.clone(),
            state: Arc::new(Mutex::new(
                BoundedMap::new(cfg.map_capacity.max(1) as usize),
            )),
            ip_state: Arc::new(Mutex::new(BoundedMap::new(
                cfg.ip_map_capacity.max(1) as usize
            ))),
        })
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Called BEFORE argon2. `Err(Throttled)` carries the remaining
    /// lockout in the message (`retry_after=<ms>ms`). Fail-open: any
    /// internal surprise degrades to "allow".
    pub fn check(&self, username: &str) -> Result<(), StorageError> {
        if !self.cfg.enabled {
            return Ok(());
        }
        // Fail-open by construction: the lock cannot fail usably (poison
        // is recovered); a missing entry means "no backoff state".
        let map = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(st) = map.get(&username.to_string()) {
            let now = now_ms();
            if now < st.locked_until_ms {
                let retry_after = st.locked_until_ms - now;
                return Err(StorageError::Throttled(format!(
                    "login locked for user {username}: retry_after={retry_after}ms"
                )));
            }
        }
        Ok(())
    }

    /// Called AFTER a `InvalidCredentials` result. Doubles the backoff up
    /// to the cap. Returns the applied lockout (ms) for diagnostics.
    pub fn record_failure(&self, username: &str) -> u64 {
        if !self.cfg.enabled {
            return 0;
        }
        let now = now_ms();
        let mut map = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let applied = match map.get_mut(&username.to_string()) {
            Some(st) => {
                st.consecutive_failures = st.consecutive_failures.saturating_add(1);
                let exp = st.consecutive_failures.min(32) as f64;
                let raw = self.cfg.base_ms as f64 * self.cfg.multiplier.powf(exp - 1.0);
                let lockout = (raw as u64).min(self.cfg.lockout_cap_ms);
                st.locked_until_ms = now.saturating_add(lockout);
                lockout
            }
            None => {
                map.get_or_insert(username.to_string(), || BackoffState {
                    consecutive_failures: 1,
                    locked_until_ms: now.saturating_add(self.cfg.base_ms),
                });
                self.cfg.base_ms
            }
        };
        applied
    }

    // -- V6-15: per-IP layer ------------------------------------------------

    /// Fail-open check of the per-IP ladder (call BEFORE argon2, together
    /// with [`check`](Self::check)). `Err(Throttled)` names the dimension.
    pub fn check_ip(&self, ip: std::net::IpAddr) -> Result<(), StorageError> {
        if !self.cfg.enabled {
            return Ok(());
        }
        let map = self
            .ip_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(st) = map.get(&ip) {
            let now = now_ms();
            if now < st.locked_until_ms {
                let retry_after = st.locked_until_ms - now;
                return Err(StorageError::Throttled(format!(
                    "login locked for peer {ip}: retry_after={retry_after}ms"
                )));
            }
        }
        Ok(())
    }

    /// Records a credential failure AGAINST THE PEER IP (called on
    /// InvalidCredentials, together with [`record_failure`](Self::record_failure)).
    pub fn record_failure_ip(&self, ip: std::net::IpAddr) -> u64 {
        if !self.cfg.enabled {
            return 0;
        }
        let now = now_ms();
        let mut map = self
            .ip_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match map.get_mut(&ip) {
            Some(st) => {
                st.consecutive_failures = st.consecutive_failures.saturating_add(1);
                let exp = st.consecutive_failures.min(32) as f64;
                let raw = self.cfg.base_ms as f64 * self.cfg.multiplier.powf(exp - 1.0);
                let lockout = (raw as u64).min(self.cfg.ip_lockout_cap_ms);
                st.locked_until_ms = now.saturating_add(lockout);
                lockout
            }
            None => {
                map.get_or_insert(ip, || BackoffState {
                    consecutive_failures: 1,
                    locked_until_ms: now.saturating_add(self.cfg.base_ms),
                });
                self.cfg.base_ms
            }
        }
    }

    /// Called AFTER a successful verify. Resets the backoff.
    pub fn record_success(&self, username: &str) {
        if !self.cfg.enabled {
            return;
        }
        let mut map = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.remove(&username.to_string());
        // V6-15: a verified-successful login also clears the peer's IP
        // ladder (shared-IP households must not inherit stale failures).
        // The ip itself is unknown here; the caller clears it via
        // record_success_ip when it has it.
    }

    /// V6-15: clears the per-IP ladder on a successful verify (companion of
    /// [`record_success`](Self::record_success); the caller has the peer).
    pub fn record_success_ip(&self, ip: std::net::IpAddr) {
        if !self.cfg.enabled {
            return;
        }
        let mut map = self
            .ip_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.remove(&ip);
    }

    /// Failure counter for a username (0 when none) — test/gate helper.
    pub fn failures(&self, username: &str) -> u32 {
        let map = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.get(&username.to_string())
            .map(|s| s.consecutive_failures)
            .unwrap_or(0)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(base_ms: u64, cap_ms: u64) -> ThrottleConfig {
        ThrottleConfig {
            enabled: true,
            base_ms,
            multiplier: 2.0,
            lockout_cap_ms: cap_ms,
            map_capacity: 10_000,
            ip_lockout_cap_ms: 60_000,
            ip_map_capacity: 100,
        }
    }

    #[test]
    fn lockout_doubles_and_caps() {
        let t = LoginThrottle::from_cfg(&cfg(100, 1_000));
        // Failures 1..=5 → applied lockouts 100,200,400,800,1600→capped 1000.
        assert_eq!(t.record_failure("u"), 100);
        assert_eq!(t.record_failure("u"), 200);
        assert_eq!(t.record_failure("u"), 400);
        assert_eq!(t.record_failure("u"), 800);
        assert_eq!(t.record_failure("u"), 1_000, "capped at lockout_cap");
        assert!(t.check("u").is_err());
        t.record_success("u");
        assert!(t.check("u").is_ok(), "success resets the backoff");
        assert_eq!(t.failures("u"), 0);
    }

    #[test]
    fn users_are_independent() {
        let t = LoginThrottle::from_cfg(&cfg(1_000, 60_000));
        t.record_failure("alice");
        assert!(t.check("alice").is_err());
        assert!(t.check("bob").is_ok(), "other users unaffected");
    }

    #[test]
    fn disabled_passes_through() {
        let mut c = cfg(1_000, 60_000);
        c.enabled = false;
        let t = LoginThrottle::from_cfg(&c);
        t.record_failure("u");
        assert!(t.check("u").is_ok());
    }
}
