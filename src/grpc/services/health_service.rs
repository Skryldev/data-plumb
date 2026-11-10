//! gRPC health checking (`grpc.health.v1.Health`) backed by real probes:
//! redb readability and data-dir accessibility are checked every 10 s and
//! the per-service statuses flip to NOT_SERVING on failure.

use std::time::Duration;

use tonic_health::pb::health_server::HealthServer;
use tonic_health::server::{health_reporter, HealthReporter};

use crate::error::StorageError;
use crate::state::AppState;

/// Probe interval for the background watcher.
const WATCH_INTERVAL: Duration = Duration::from_secs(10);

/// All names tracked by this health service: the four application services
/// plus the overall server (`""`).
pub fn tracked_services() -> Vec<String> {
    crate::grpc::generated::SERVICE_NAMES
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(String::new()))
        .collect()
}

/// Builds the health service plus its background watcher.
///
/// The watcher probes [`probe`] every [`WATCH_INTERVAL`]; on failure every
/// tracked service flips to NOT_SERVING within one interval (the
/// acceptance criterion allows 15 s).
pub fn build(
    state: AppState,
) -> (
    HealthReporter,
    HealthServer<impl tonic_health::pb::health_server::Health>,
) {
    let (reporter, service) = health_reporter();

    // Initial status: serving — the server only reaches this point when it
    // assembled cleanly.
    for name in tracked_services() {
        let mut reporter = reporter.clone();
        tokio::spawn(async move {
            reporter
                .set_service_status(&name, tonic_health::ServingStatus::Serving)
                .await;
        });
    }

    // Background watcher.
    let watcher_reporter = reporter.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(WATCH_INTERVAL);
        ticker.tick().await; // first tick fires immediately; skip it
        loop {
            ticker.tick().await;
            let status = match probe(&state).await {
                Ok(()) => tonic_health::ServingStatus::Serving,
                Err(e) => {
                    tracing::error!(error = %e, "grpc health probe failed");
                    tonic_health::ServingStatus::NotServing
                }
            };
            for name in tracked_services() {
                let mut watcher_reporter = watcher_reporter.clone();
                watcher_reporter.set_service_status(&name, status).await;
            }
        }
    });

    (reporter, service)
}

/// Real health probe: database readable + storage root accessible.
pub async fn probe(state: &AppState) -> Result<(), StorageError> {
    state.0.index.ping().await?;
    let root = &state.0.cfg.storage.root;
    match tokio::fs::metadata(root).await {
        Ok(_) => Ok(()),
        Err(e) => Err(StorageError::Unavailable(format!(
            "storage root {root:?} is not accessible: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracked_names_include_overall() {
        let names = tracked_services();
        assert_eq!(names.len(), 5);
        assert!(names.iter().any(|n| n.is_empty()), "overall service name");
        assert!(names.iter().any(|n| n.ends_with("FileService")));
        assert!(names.iter().any(|n| n.ends_with("AuthService")));
        assert!(names.iter().any(|n| n.ends_with("AdminService")));
        assert!(names.iter().any(|n| n.ends_with("ReplicationService")));
    }
}
