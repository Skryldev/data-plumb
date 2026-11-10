//! Token-bucket rate limiting via tower_governor (keyed by peer IP).

use crate::config::RateLimitConfig;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::PeerIpKeyExtractor;
use tower_governor::GovernorLayer;

/// Concrete layer type used in the router (PeerIp + NoOp middleware).
pub type IpGovernorLayer = GovernorLayer<PeerIpKeyExtractor, governor::middleware::NoOpMiddleware>;

pub use governor::middleware::NoOpMiddleware;

/// Builds the governor layer; `None` when rate limiting is disabled.
pub fn build_governor(cfg: &RateLimitConfig) -> Option<IpGovernorLayer> {
    if !cfg.enabled {
        return None;
    }
    let mut builder = GovernorConfigBuilder::default();
    builder.per_second(cfg.per_second as u64);
    builder.burst_size(cfg.burst.max(1));
    let config = builder.finish()?;
    Some(GovernorLayer {
        config: std::sync::Arc::new(config),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_when_enabled() {
        let cfg = RateLimitConfig {
            enabled: true,
            per_second: 10,
            burst: 20,
        };
        assert!(build_governor(&cfg).is_some());
    }

    #[test]
    fn disabled_returns_none() {
        let cfg = RateLimitConfig {
            enabled: false,
            per_second: 10,
            burst: 20,
        };
        assert!(build_governor(&cfg).is_none());
    }

    #[test]
    fn zero_burst_clamped() {
        let cfg = RateLimitConfig {
            enabled: true,
            per_second: 1,
            burst: 0,
        };
        assert!(build_governor(&cfg).is_some());
    }
}
