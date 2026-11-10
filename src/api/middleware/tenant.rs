//! Tenant context helpers used by handlers (access checks and usage
//! accounting live in `security::tenant`; this module wires them to the HTTP
//! layer and provides the per-request guard).

use uuid::Uuid;

use crate::error::StorageError;
use crate::security::auth::AuthContext;
use crate::security::tenant;
use crate::state::AppState;

/// Ensures the authenticated tenant exists (auto-provisions a placeholder for
/// replicated tenants) and returns it.
pub async fn resolve_tenant(
    state: &AppState,
    ctx: &AuthContext,
) -> Result<crate::security::tenant::Tenant, StorageError> {
    state.0.tenants.ensure(&ctx.tenant_id, &ctx.username).await
}

/// Access guard for a file owned by `file_tenant`.
pub fn check_file_access(ctx: &AuthContext, file_tenant: &Uuid) -> Result<(), StorageError> {
    tenant::check_access(&ctx.tenant_id, file_tenant, ctx.role.is_admin())
}

/// Write-permission guard (upload/delete need user or admin).
pub fn check_write(ctx: &AuthContext) -> Result<(), StorageError> {
    ctx.require_write()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Role;
    use uuid::Uuid;

    fn ctx(tenant: Uuid, role: Role) -> AuthContext {
        AuthContext {
            username: "u".into(),
            tenant_id: tenant,
            role,
        }
    }

    #[test]
    fn same_tenant_ok() {
        let t = Uuid::now_v7();
        assert!(check_file_access(&ctx(t, Role::User), &t).is_ok());
    }

    #[test]
    fn cross_tenant_requires_admin() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert!(check_file_access(&ctx(a, Role::User), &b).is_err());
        assert!(check_file_access(&ctx(a, Role::Admin), &b).is_ok());
    }

    #[test]
    fn viewer_cannot_write() {
        assert!(check_write(&ctx(Uuid::now_v7(), Role::Viewer)).is_err());
        assert!(check_write(&ctx(Uuid::now_v7(), Role::User)).is_ok());
    }
}
