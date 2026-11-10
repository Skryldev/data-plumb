//! `file_storage.v1.AdminService` — tenant CRUD, GC trigger, resync and
//! audit query. All RPCs are admin-only (enforced by the interceptor and
//! re-checked in-handler).

use tonic::{Request, Response, Status};

use crate::grpc::convert::{tenant_to_proto, ts_from_ms, uuid_from_str};
use crate::grpc::generated::pb;
use crate::grpc::generated::AdminService;
use crate::grpc::proto_auth::{require_admin_ctx, status_from_error};
use crate::replication::client::ChunkPayload;
use crate::security::audit::{Actor, AuditAction};
use crate::security::auth::AuthContext;
use crate::state::AppState;

pub struct AdminServiceImpl {
    state: AppState,
}

impl AdminServiceImpl {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl AdminService for AdminServiceImpl {
    async fn create_tenant(
        &self,
        request: Request<pb::CreateTenantRequest>,
    ) -> Result<Response<pb::Tenant>, Status> {
        require_admin_ctx(&request)?;
        let ctx = context_from(&request)?;
        let req = request.into_inner();
        let tenant = self
            .state
            .0
            .tenants
            .create(&req.name)
            .await
            .map_err(|e| status_from_error(&e))?;
        self.state
            .0
            .audit
            .record(
                AuditAction::TenantCreate,
                Actor::User(ctx.username),
                Some(tenant.id),
                &tenant.name,
                "",
                "ok",
                "grpc",
            )
            .await;
        Ok(Response::new(tenant_to_proto(&tenant)))
    }

    async fn get_tenant(
        &self,
        request: Request<pb::GetTenantRequest>,
    ) -> Result<Response<pb::Tenant>, Status> {
        require_admin_ctx(&request)?;
        let req = request.into_inner();
        let id = uuid_from_str(&req.id, "tenant id").map_err(|e| status_from_error(&e))?;
        let tenant = self
            .state
            .0
            .tenants
            .get(&id)
            .await
            .map_err(|e| status_from_error(&e))?;
        Ok(Response::new(tenant_to_proto(&tenant)))
    }

    async fn list_tenants(
        &self,
        request: Request<pb::ListTenantsRequest>,
    ) -> Result<Response<pb::ListTenantsResponse>, Status> {
        require_admin_ctx(&request)?;
        let req = request.into_inner();
        let mut tenants = self
            .state
            .0
            .tenants
            .list()
            .await
            .map_err(|e| status_from_error(&e))?;
        let total = tenants.len().min(u32::MAX as usize) as u32;
        if req.limit > 0 {
            tenants.truncate(req.limit as usize);
        }
        Ok(Response::new(pb::ListTenantsResponse {
            tenants: tenants.iter().map(tenant_to_proto).collect(),
            total,
        }))
    }

    async fn trigger_gc(
        &self,
        request: Request<pb::TriggerGcRequest>,
    ) -> Result<Response<pb::GcResult>, Status> {
        require_admin_ctx(&request)?;
        let ctx = context_from(&request)?;
        let req = request.into_inner();

        // dry_run is reported as UNIMPLEMENTED rather than silently running
        // a destructive pass the caller believes was a rehearsal.
        if req.dry_run {
            return Err(Status::unimplemented(
                "gc dry_run is not supported yet; run without dry_run",
            ));
        }
        let mode = match req.mode {
            0 => None, // MODE_UNSPECIFIED → configured mode
            1 => Some(crate::config::GcMode::Refcount),
            2 => Some(crate::config::GcMode::Reachability),
            other => return Err(Status::invalid_argument(format!("unknown gc mode {other}"))),
        };
        let report = if let Some(mode) = mode {
            self.state
                .0
                .gc
                .run_once_with(mode)
                .await
                .map_err(|e| status_from_error(&e))?
        } else {
            self.state
                .0
                .gc
                .run_once()
                .await
                .map_err(|e| status_from_error(&e))?
        };
        self.state
            .0
            .audit
            .record(
                AuditAction::GcRun,
                Actor::User(ctx.username),
                None,
                "",
                "",
                "ok",
                &format!(
                    "grpc mode={:?} purged={} chunks={} orphans={}",
                    req.mode,
                    report.manifests_purged,
                    report.chunks_removed,
                    report.orphans_removed
                ),
            )
            .await;
        Ok(Response::new(pb::GcResult {
            manifests_purged: report.manifests_purged,
            chunks_removed: report.chunks_removed,
            orphans_removed: report.orphans_removed,
            bytes_reclaimed: report.bytes_reclaimed,
        }))
    }

    async fn resync(
        &self,
        request: Request<pb::ResyncRequest>,
    ) -> Result<Response<pb::ResyncResult>, Status> {
        require_admin_ctx(&request)?;
        let ctx = context_from(&request)?;
        let req = request.into_inner();
        let file_id = uuid_from_str(&req.file_id, "file_id").map_err(|e| status_from_error(&e))?;

        let manifest = self
            .state
            .0
            .retrieval
            .manifest(&file_id)
            .await
            .map_err(|e| status_from_error(&e))?;
        crate::security::tenant::check_access(&ctx.tenant_id, &manifest.tenant_id, true)
            .map_err(|e| status_from_error(&e))?;

        let mut payloads: Vec<ChunkPayload> = Vec::with_capacity(manifest.chunks.len());
        for cr in &manifest.chunks {
            let blob = self
                .state
                .0
                .blocks
                .read(&cr.hash)
                .await
                .map_err(|e| status_from_error(&e))?;
            payloads.push(ChunkPayload {
                hash: cr.hash,
                plain_hash: cr.plain_hash,
                size_original: cr.size_original,
                size_stored: cr.size_stored,
                compressed: cr.compressed,
                encrypted: cr.encrypted,
                blob,
            });
        }
        let bundle = crate::replication::client::encode_file_bundle(&manifest, &payloads)
            .map_err(|e| status_from_error(&e))?;
        self.state
            .0
            .replicator
            .push_bundle(&bundle, &file_id)
            .await
            .map_err(|e| status_from_error(&e))?;
        self.state
            .0
            .audit
            .record(
                AuditAction::Resync,
                Actor::User(ctx.username),
                Some(manifest.tenant_id),
                &manifest.name,
                "",
                "ok",
                &format!("file_id={file_id} grpc chunks={}", payloads.len()),
            )
            .await;
        Ok(Response::new(pb::ResyncResult {
            file_id: file_id.to_string(),
            replicated: true,
        }))
    }

    async fn query_audit(
        &self,
        request: Request<pb::QueryAuditRequest>,
    ) -> Result<Response<pb::QueryAuditResponse>, Status> {
        require_admin_ctx(&request)?;
        let req = request.into_inner();
        let limit = if req.limit == 0 {
            100
        } else {
            (req.limit as usize).clamp(1, 1000)
        };
        let mut events = self
            .state
            .0
            .audit
            .recent(limit * 4)
            .await
            .map_err(|e| status_from_error(&e))?;
        if let Some(tenant) = req.tenant_id.trim().non_empty() {
            let id = uuid_from_str(tenant, "tenant_id").map_err(|e| status_from_error(&e))?;
            events.retain(|e| e.tenant_id == Some(id));
        }
        events.truncate(limit);
        let entries: Vec<pb::AuditEntry> = events
            .iter()
            .map(|e| pb::AuditEntry {
                id: e.id.to_string(),
                at: Some(ts_from_ms(e.ts_ms)),
                actor: e.actor.clone(),
                tenant_id: e.tenant_id.map(|t| t.to_string()).unwrap_or_default(),
                action: e.action.clone(),
                object: e.object.clone(),
                ip: e.ip.clone(),
                status: e.status.clone(),
                detail: e.detail.clone(),
            })
            .collect();
        let total = entries.len().min(u32::MAX as usize) as u32;
        Ok(Response::new(pb::QueryAuditResponse { entries, total }))
    }
}

/// Reads the admin context injected by the interceptor (falling back to
/// metadata verification for safety).
fn context_from<T>(request: &Request<T>) -> Result<AuthContext, Status> {
    request
        .extensions()
        .get::<AuthContext>()
        .cloned()
        .ok_or_else(|| Status::unauthenticated("missing auth context"))
}

trait NonEmptyStr {
    fn non_empty(&self) -> Option<&str>;
}

impl NonEmptyStr for str {
    fn non_empty(&self) -> Option<&str> {
        let t = self.trim();
        (!t.is_empty()).then_some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_empty_trims() {
        assert_eq!("  x  ".non_empty(), Some("x"));
        assert_eq!("   ".non_empty(), None);
        assert_eq!("".non_empty(), None);
    }
}
