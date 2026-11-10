//! Per-tenant token-bucket rate limiting for unary RPCs.
//!
//! Interceptors cannot be async, so the limiter is a plain `Mutex` map of
//! token buckets keyed by tenant id. The bucket refills linearly
//! (`per_second` tokens/s) up to `burst`. Disabled configs pass through.
//! Streaming RPCs do not pass through interceptors and are therefore not
//! rate-limited here (documented; the REST path keeps its governor).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use uuid::Uuid;

/// Buckets are pruned when the map grows beyond this many tenants.
const MAX_TRACKED_TENANTS: usize = 10_000;

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Rate limiter shared by the unary auth interceptor.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Inner>,
}

struct Inner {
    enabled: bool,
    per_second: f64,
    burst: f64,
    buckets: Mutex<HashMap<Uuid, Bucket>>,
}

impl RateLimiter {
    pub fn from_cfg(enabled: bool, per_second: u32, burst: u32) -> Self {
        Self {
            inner: Arc::new(Inner {
                enabled,
                per_second: per_second.max(1) as f64,
                burst: burst.max(1) as f64,
                buckets: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Takes one token for `tenant`; `false` means the caller is over rate.
    pub fn allow(&self, tenant: Uuid) -> bool {
        if !self.inner.enabled {
            return true;
        }
        let now = Instant::now();
        let mut buckets = self
            .inner
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if buckets.len() > MAX_TRACKED_TENANTS {
            buckets.clear();
        }
        let bucket = buckets
            .entry(tenant)
            .and_modify(|b| {
                let elapsed = now.duration_since(b.last_refill).as_secs_f64();
                b.tokens = (b.tokens + elapsed * self.inner.per_second).min(self.inner.burst);
                b.last_refill = now;
            })
            .or_insert_with(|| Bucket {
                tokens: self.inner.burst,
                last_refill: now,
            });
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
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
