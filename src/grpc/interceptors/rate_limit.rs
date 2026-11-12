//! Per-tenant token-bucket rate limiting for unary AND streaming RPCs.
//!
//! M-05: ONE governor instance lives in `AppState` (`state.0.rate_limiter`);
//! the unary interceptor and the streaming handlers (Upload/Download) all
//! take tokens from that same instance — the streaming bypass is closed.
//! The bucket map uses the shared bounded-map helper: entries evict
//! individually (oldest first), never wholesale (the pre-M-05 `clear()`
//! was the I-06 bug pattern).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use uuid::Uuid;

use crate::core::bounded_map::BoundedMap;

/// Buckets are evicted individually (oldest first) when the map grows
/// beyond this many tenants — never a wholesale clear.
const MAX_TRACKED_TENANTS: usize = 10_000;

#[derive(Debug, Clone)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Rate limiter shared by the unary interceptor and streaming handlers.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Inner>,
}

struct Inner {
    enabled: bool,
    per_second: f64,
    burst: f64,
    buckets: Mutex<BoundedMap<Uuid, Bucket>>,
}

impl RateLimiter {
    pub fn from_cfg(enabled: bool, per_second: u32, burst: u32) -> Self {
        Self {
            inner: Arc::new(Inner {
                enabled,
                per_second: per_second.max(1) as f64,
                burst: burst.max(1) as f64,
                buckets: Mutex::new(BoundedMap::new(MAX_TRACKED_TENANTS)),
            }),
        }
    }

    /// Takes one token for `tenant`; `false` means the caller is over rate.
    pub fn allow(&self, tenant: Uuid) -> bool {
        if !self.inner.enabled {
            return true;
        }
        let now = Instant::now();
        let mut map = self
            .inner
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let allowed;
        if let Some(b) = map.get_mut(&tenant) {
            let elapsed = now.duration_since(b.last_refill).as_secs_f64();
            b.tokens = (b.tokens + elapsed * self.inner.per_second).min(self.inner.burst);
            b.last_refill = now;
            allowed = b.tokens >= 1.0;
            if allowed {
                b.tokens -= 1.0;
            }
        } else {
            // Fresh bucket starts at `burst` (>= 1 by from_cfg) and THIS
            // request consumes its first token. get_or_insert performs the
            // oldest-first eviction beyond the shared helper's cap (I-06
            // fix: never a wholesale clear).
            let initial = self.inner.burst - 1.0;
            map.get_or_insert(tenant, || Bucket {
                tokens: initial,
                last_refill: now,
            });
            allowed = true; // burst >= 1 ⇒ at least this token existed
        }
        allowed
    }

    /// Stable identity of the shared governor instance — gate tests assert
    /// the interceptor and streaming handlers resolve to the SAME instance.
    pub fn instance_ptr(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(per_second: u32, burst: u32, enabled: bool) -> RateLimiter {
        RateLimiter::from_cfg(enabled, per_second, burst)
    }

    #[test]
    fn burst_allows_then_exhausts() {
        let l = limiter(1, 3, true);
        let t = Uuid::now_v7();
        assert!(l.allow(t) && l.allow(t) && l.allow(t));
        assert!(!l.allow(t), "bucket must be exhausted");
    }

    #[test]
    fn disabled_always_allows() {
        let l = limiter(1, 1, false);
        let t = Uuid::now_v7();
        for _ in 0..100 {
            assert!(l.allow(t));
        }
    }

    #[test]
    fn tenants_are_independent() {
        let l = limiter(1, 1, true);
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert!(l.allow(a));
        assert!(!l.allow(a));
        assert!(l.allow(b));
    }
}
