//! `file_storage.v1.AuthService` — login / refresh / logout, mirroring the
//! REST auth endpoints with the same audit trail.

use std::time::SystemTime;
use tonic::{Request, Response, Status};

use crate::grpc::convert::ts_from_ms;
use crate::grpc::generated::pb;
use crate::grpc::generated::AuthService;
use crate::grpc::proto_auth::status_from_error;
use crate::security::audit::{Actor, AuditAction};
use crate::state::AppState;

pub struct AuthServiceImpl {
    state: AppState,
}

impl AuthServiceImpl {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

/// Epoch ms for the access-token absolute expiry.
fn expires_at_ms(expires_in: u64) -> i64 {
    (SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + expires_in) as i64
        * 1000
}

fn to_response(pair: crate::security::auth::TokenPair) -> pb::LoginResponse {
    pb::LoginResponse {
        access_token: pair.access_token,
        refresh_token: pair.refresh_token,
        token_type: "Bearer".to_string(),
        expires_in: pair.expires_in,
        expires_at: Some(ts_from_ms(expires_at_ms(pair.expires_in))),
    }
}

#[tonic::async_trait]
impl AuthService for AuthServiceImpl {
    async fn login(
        &self,
        request: Request<pb::LoginRequest>,
    ) -> Result<Response<pb::LoginResponse>, Status> {
        let req = request.into_inner();
        let username = req.username.trim().to_string();
        if username.is_empty() || req.password.is_empty() {
            return Err(Status::invalid_argument(
                "username and password are required",
            ));
        }
        match self.state.0.auth.login(&username, &req.password) {
            Ok(pair) => {
                self.state
                    .0
                    .audit
                    .record(
                        AuditAction::LoginSuccess,
                        Actor::User(username.clone()),
                        None,
                        &username,
                        "",
                        "ok",
                        "grpc",
                    )
                    .await;
                Ok(Response::new(to_response(pair)))
            }
            Err(e) => {
                self.state
                    .0
                    .audit
                    .record(
                        AuditAction::LoginFailure,
                        Actor::User(username.clone()),
                        None,
                        &username,
                        "",
                        "rejected",
                        "grpc",
                    )
                    .await;
                Err(status_from_error(&e))
            }
        }
    }

    async fn refresh(
        &self,
        request: Request<pb::RefreshRequest>,
    ) -> Result<Response<pb::LoginResponse>, Status> {
        let req = request.into_inner();
        if req.refresh_token.trim().is_empty() {
            return Err(Status::invalid_argument("refresh_token is required"));
        }
        let pair = self
            .state
            .0
            .auth
            .refresh(&req.refresh_token)
            .map_err(|e| status_from_error(&e))?;
        Ok(Response::new(to_response(pair)))
    }

    async fn logout(
        &self,
        request: Request<pb::LogoutRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let req = request.into_inner();
        if req.token.trim().is_empty() {
            return Err(Status::invalid_argument(
                "token (the refresh token) is required",
            ));
        }
        // Idempotent, like the REST endpoint.
        let _ = self.state.0.auth.logout(&req.token);
        Ok(Response::new(pb::Empty {}))
    }
}
