//! Tenant management: registration, quotas and access checks.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{not_found, StorageError};
use crate::storage::index_db::{now_ms, IndexDb};

/// A tenant record, stored as JSON in the redb `tenants` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    pub id: Uuid,
    pub name: String,
    pub created_at_ms: i64,
    pub file_count: u64,
    pub bytes_used: u64,
}

/// Seed entry from configuration.
#[derive(Debug, Clone)]
pub struct TenantSeed {
    pub id: Uuid,
    pub name: String,
}

/// Tenant store facade over the index.
#[derive(Clone)]
pub struct TenantStore {
    index: std::sync::Arc<IndexDb>,
}

impl TenantStore {
    pub fn new(index: std::sync::Arc<IndexDb>) -> Self {
        Self { index }
    }

    /// Registers configured tenants (idempotent upsert at boot).
    pub async fn seed(&self, seeds: &[TenantSeed]) -> Result<(), StorageError> {
        for s in seeds {
            if self.index.get_tenant(&s.id).await?.is_some() {
                continue;
            }
            self.index
                .upsert_tenant(&Tenant {
                    id: s.id,
                    name: s.name.clone(),
                    created_at_ms: now_ms(),
                    file_count: 0,
                    bytes_used: 0,
                })
                .await?;
            tracing::info!(tenant = %s.id, name = %s.name, "tenant seeded");
        }
        Ok(())
    }

    pub async fn create(&self, name: &str) -> Result<Tenant, StorageError> {
        let name = name.trim();
        if name.is_empty() || name.len() > 128 {
            return Err(StorageError::Validation(
                "tenant name must be 1..=128 characters".into(),
            ));
        }
        let t = Tenant {
            id: Uuid::now_v7(),
            name: name.to_string(),
            created_at_ms: now_ms(),
            file_count: 0,
            bytes_used: 0,
        };
        self.index.upsert_tenant(&t).await?;
        Ok(t)
    }

    pub async fn get(&self, id: &Uuid) -> Result<Tenant, StorageError> {
        self.index
            .get_tenant(id)
            .await?
            .ok_or_else(|| not_found(format!("tenant {id}")))
    }

    pub async fn list(&self) -> Result<Vec<Tenant>, StorageError> {
        self.index.list_tenants().await
    }

    /// Ensures a tenant referenced by replication/auth exists (creates a
    /// placeholder when it does not).
    pub async fn ensure(&self, id: &Uuid, fallback_name: &str) -> Result<Tenant, StorageError> {
        if let Some(t) = self.index.get_tenant(id).await? {
            return Ok(t);
        }
        let t = Tenant {
            id: *id,
            name: fallback_name.to_string(),
            created_at_ms: now_ms(),
            file_count: 0,
            bytes_used: 0,
        };
        self.index.upsert_tenant(&t).await?;
        Ok(t)
    }

    pub async fn exists(&self, id: &Uuid) -> Result<bool, StorageError> {
        Ok(self.index.get_tenant(id).await?.is_some())
    }
}

/// Enforces that `ctx_tenant` may access a file owned by `file_tenant`.
pub fn check_access(
    ctx_tenant: &Uuid,
    file_tenant: &Uuid,
    is_admin: bool,
) -> Result<(), StorageError> {
    if is_admin || ctx_tenant == file_tenant {
        Ok(())
    } else {
        Err(StorageError::Forbidden(
            "file belongs to a different tenant".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    async fn store() -> (TenantStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let idx = Arc::new(IndexDb::open(&dir.path().join("db.redb")).unwrap());
        (TenantStore::new(idx), dir)
    }

    #[tokio::test]
    async fn create_get_list() {
        let (ts, _d) = store().await;
        let t = ts.create("acme").await.unwrap();
        assert_eq!(t.name, "acme");
        assert_eq!(ts.get(&t.id).await.unwrap().id, t.id);
        assert!(ts.exists(&t.id).await.unwrap());
        assert_eq!(ts.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_bad_names() {
        let (ts, _d) = store().await;
        assert!(matches!(
            ts.create("   ").await,
            Err(StorageError::Validation(_))
        ));
        let long = "x".repeat(129);
        assert!(ts.create(&long).await.is_err());
    }

    #[tokio::test]
    async fn seed_is_idempotent() {
        let (ts, _d) = store().await;
        let id = Uuid::now_v7();
        let seeds = vec![TenantSeed {
            id,
            name: "boot".into(),
        }];
        ts.seed(&seeds).await.unwrap();
        ts.seed(&seeds).await.unwrap();
        assert_eq!(ts.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ensure_creates_placeholder() {
        let (ts, _d) = store().await;
        let id = Uuid::now_v7();
        let t = ts.ensure(&id, "replicated-tenant").await.unwrap();
        assert_eq!(t.name, "replicated-tenant");
        let again = ts.ensure(&id, "other").await.unwrap();
        assert_eq!(again.name, "replicated-tenant");
    }

    #[test]
    fn access_rules() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert!(check_access(&a, &a, false).is_ok());
        assert!(check_access(&a, &b, true).is_ok());
        assert!(matches!(
            check_access(&a, &b, false),
            Err(StorageError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn missing_tenant_is_not_found() {
        let (ts, _d) = store().await;
        let id = Uuid::now_v7();
        assert!(matches!(ts.get(&id).await, Err(StorageError::NotFound(_))));
    }
}
