//! Append-only audit trail persisted in the redb `audit` table.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::StorageError;
use crate::metrics;
use crate::storage::index_db::{now_ms, IndexDb};

/// Who performed the action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    System,
    User(String),
}

impl Actor {
    fn as_str(&self) -> String {
        match self {
            Actor::System => "system".to_string(),
            Actor::User(u) => u.clone(),
        }
    }
}

/// Actions recorded in the audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    Upload,
    Download,
    Delete,
    Purge,
    LoginSuccess,
    LoginFailure,
    /// M-05: a login was refused by the per-username lockout BEFORE the
    /// password verify ran.
    LoginThrottled,
    TenantCreate,
    ReplicationApply,
    GcRun,
    Resync,
}

impl AuditAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuditAction::Upload => "file.upload",
            AuditAction::Download => "file.download",
            AuditAction::Delete => "file.delete",
            AuditAction::Purge => "file.purge",
            AuditAction::LoginSuccess => "auth.login.success",
            AuditAction::LoginFailure => "auth.login.failure",
            AuditAction::TenantCreate => "tenant.create",
            AuditAction::ReplicationApply => "replication.apply",
            AuditAction::GcRun => "storage.gc",
            AuditAction::Resync => "replication.resync",
            AuditAction::LoginThrottled => "auth.login.throttled",
        }
    }
}

/// One audit record. Keys are UUIDv7 bytes, so the table scan order is
/// chronological.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: Uuid,
    pub ts_ms: i64,
    pub actor: String,
    pub tenant_id: Option<Uuid>,
    pub action: String,
    pub object: String,
    pub ip: String,
    pub status: String,
    pub detail: String,
}

/// Thin facade over `IndexDb::append_audit` — never fails the request on
/// audit errors (they are logged and counted instead).
#[derive(Clone)]
pub struct AuditLog {
    index: std::sync::Arc<IndexDb>,
}

impl AuditLog {
    pub fn new(index: std::sync::Arc<IndexDb>) -> Self {
        Self { index }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record(
        &self,
        action: AuditAction,
        actor: Actor,
        tenant_id: Option<Uuid>,
        object: &str,
        ip: &str,
        status: &str,
        detail: &str,
    ) {
        let ev = AuditEvent {
            id: Uuid::now_v7(),
            ts_ms: now_ms(),
            actor: actor.as_str(),
            tenant_id,
            action: action.as_str().to_string(),
            object: object.to_string(),
            ip: ip.to_string(),
            status: status.to_string(),
            detail: detail.to_string(),
        };
        if let Err(e) = self.index.append_audit(&ev).await {
            tracing::error!(error = %e, action = ev.action, "failed to persist audit event");
        }
        metrics::record_audit(action.as_str());
    }

    pub async fn recent(&self, limit: usize) -> Result<Vec<AuditEvent>, StorageError> {
        self.index.list_audit(limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    async fn log() -> (AuditLog, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let idx = Arc::new(IndexDb::open(&dir.path().join("db.redb")).unwrap());
        (AuditLog::new(idx), dir)
    }

    #[tokio::test]
    async fn records_and_reads_back() {
        let (log, _d) = log().await;
        log.record(
            AuditAction::Upload,
            Actor::User("alice".into()),
            Some(Uuid::now_v7()),
            "f1",
            "127.0.0.1",
            "ok",
            "",
        )
        .await;
        log.record(
            AuditAction::Download,
            Actor::System,
            None,
            "f2",
            "10.0.0.1",
            "ok",
            "l1 hit",
        )
        .await;
        let events = log.recent(10).await.unwrap();
        assert_eq!(events.len(), 2);
        let actions: Vec<&str> = events.iter().map(|e| e.action.as_str()).collect();
        assert!(actions.contains(&"file.upload"));
        assert!(actions.contains(&"file.download"));
    }

    #[tokio::test]
    async fn newest_first_ordering() {
        let (log, _d) = log().await;
        for i in 0..5 {
            log.record(
                AuditAction::Upload,
                Actor::System,
                None,
                &format!("f{i}"),
                "",
                "ok",
                "",
            )
            .await;
        }
        let events = log.recent(5).await.unwrap();
        assert_eq!(events[0].object, "f4");
        assert_eq!(events[4].object, "f0");
    }

    #[tokio::test]
    async fn limit_is_respected() {
        let (log, _d) = log().await;
        for i in 0..10 {
            log.record(
                AuditAction::Upload,
                Actor::System,
                None,
                &format!("f{i}"),
                "",
                "ok",
                "",
            )
            .await;
        }
        assert_eq!(log.recent(3).await.unwrap().len(), 3);
    }
}
