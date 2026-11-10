//! gRPC server assembly: services, interceptors, health checking,
//! reflection and optional TLS/mTLS.
//!
//! The server runs as a second listener in the same process and shares the
//! full [`AppState`] with the REST stack. Both listeners shut down
//! gracefully together (see `crate::lib`'s `run`).

use std::time::Duration;

use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::codec::CompressionEncoding;
use tonic::transport::server::{Router, ServerTlsConfig};
use tonic::transport::{Certificate, Identity, Server};
use tower_layer::{Identity as LayerIdentity, Layer as _, Stack};

use crate::config::GrpcTlsConfig;
use crate::error::StorageError;
use crate::grpc::generated::{
    AdminServiceServer, AuthServiceServer, FileServiceServer, ReplicationServiceServer,
    FILE_DESCRIPTOR_SET, SERVICE_NAMES,
};
use crate::grpc::interceptors::auth::AuthInterceptor;
use crate::grpc::interceptors::metrics::GrpcMetricsLayer;
use crate::grpc::interceptors::recovery;
use crate::grpc::interceptors::{self, api_key::ApiKeyInterceptor};
use crate::grpc::services::admin_service::AdminServiceImpl;
use crate::grpc::services::auth_service::AuthServiceImpl;
use crate::grpc::services::file_service::FileServiceImpl;
use crate::grpc::services::health_service;
use crate::grpc::services::replication_service::ReplicationServiceImpl;
use crate::state::AppState;

/// Message-size ceilings matching the REST body limits for large transfers.
pub const MAX_MESSAGE_SIZE: usize = 256 * 1024 * 1024;

/// The panic-recovery layer type (function-pointer flavour).
pub type PanicLayer =
    tower_http::catch_panic::CatchPanicLayer<crate::grpc::interceptors::recovery::PanicHandler>;

/// Full layer stack around the tonic routes: panic recovery outermost,
/// RPC metrics inside.
pub type LayerStack = Stack<PanicLayer, Stack<GrpcMetricsLayer, LayerIdentity>>;

/// The assembled tonic router handed to the serve methods.
pub type AssembledRouter = Router<LayerStack>;

/// gRPC server bound to an [`AppState`].
#[derive(Clone)]
pub struct GrpcServer {
    state: AppState,
    tls: Option<ServerTlsConfig>,
}

/// Installs the process-wide rustls crypto provider exactly once (ring).
/// tonic's `tls` and rcgen's crypto features can both link rustls with
/// different providers, which makes provider auto-selection ambiguous.
static CRYPTO_PROVIDER: std::sync::OnceLock<()> = std::sync::OnceLock::new();

pub fn ensure_crypto_provider() {
    CRYPTO_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        tracing::debug!("rustls ring crypto provider installed for gRPC/TLS");
    });
}

impl GrpcServer {
    pub fn new(state: AppState, tls: Option<ServerTlsConfig>) -> Self {
        Self { state, tls }
    }

    /// Assembles the tonic router with every service registered.
    pub fn assemble(&self) -> Result<AssembledRouter, StorageError> {
        ensure_crypto_provider();
        let (_reporter, health_service) = health_service::build(self.state.clone());

        let reflection = SERVICE_NAMES
            .iter()
            .fold(
                tonic_reflection::server::Builder::configure()
                    .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
                    .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET),
                |b, name| b.with_service_name(*name),
            )
            .with_service_name("grpc.health.v1.Health")
            .build_v1()
            .map_err(|e| StorageError::Internal(format!("reflection service build failed: {e}")))?;

        let server = Server::builder()
            // Outermost: panic recovery, then metrics (tonic nests new
            // layers inward, so metrics ends up wrapping the whole stack).
            .layer(GrpcMetricsLayer)
            .layer(recovery::layer())
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .http2_keepalive_interval(Some(Duration::from_secs(30)))
            .http2_keepalive_timeout(Some(Duration::from_secs(20)))
            .concurrency_limit_per_connection(64)
            .initial_stream_window_size(Some(4 * 1024 * 1024))
            .initial_connection_window_size(Some(8 * 1024 * 1024));

        let mut server = match &self.tls {
            Some(tls) => server
                .tls_config(tls.clone())
                .map_err(|e| StorageError::Internal(format!("grpc tls config: {e}")))?,
            None => server,
        };

        Ok(server
            .add_service(
                interceptors::InterceptorLayer::new(AuthInterceptor::file_service(
                    self.state.clone(),
                ))
                .layer(
                    FileServiceServer::new(FileServiceImpl::new(self.state.clone()))
                        .accept_compressed(CompressionEncoding::Gzip)
                        .send_compressed(CompressionEncoding::Gzip)
                        .max_decoding_message_size(MAX_MESSAGE_SIZE)
                        .max_encoding_message_size(MAX_MESSAGE_SIZE),
                ),
            )
            .add_service(
                AuthServiceServer::new(AuthServiceImpl::new(self.state.clone()))
                    .accept_compressed(CompressionEncoding::Gzip)
                    .send_compressed(CompressionEncoding::Gzip)
                    .max_encoding_message_size(MAX_MESSAGE_SIZE),
            )
            .add_service(
                interceptors::InterceptorLayer::new(AuthInterceptor::admin_service(
                    self.state.clone(),
                ))
                .layer(
                    AdminServiceServer::new(AdminServiceImpl::new(self.state.clone()))
                        .accept_compressed(CompressionEncoding::Gzip)
                        .send_compressed(CompressionEncoding::Gzip)
                        .max_decoding_message_size(MAX_MESSAGE_SIZE)
                        .max_encoding_message_size(MAX_MESSAGE_SIZE),
                ),
            )
            .add_service(
                interceptors::InterceptorLayer::new(ApiKeyInterceptor::new(self.state.clone()))
                    .layer(
                        ReplicationServiceServer::new(ReplicationServiceImpl::new(
                            self.state.clone(),
                        ))
                        .accept_compressed(CompressionEncoding::Gzip)
                        .send_compressed(CompressionEncoding::Gzip)
                        .max_decoding_message_size(MAX_MESSAGE_SIZE)
                        .max_encoding_message_size(MAX_MESSAGE_SIZE),
                    ),
            )
            .add_service(health_service)
            .add_service(reflection))
    }

    /// Serves on a pre-bound listener (tests / custom acceptors). Returns
    /// only when the listener stream ends.
    pub async fn serve_on(&self, listener: TcpListener) -> Result<(), StorageError> {
        let router = self.assemble()?;
        router
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .map_err(|e| StorageError::Internal(format!("grpc server error: {e}")))
    }

    /// Serves on `addr` until `shutdown` resolves (production path).
    pub async fn serve(
        &self,
        addr: std::net::SocketAddr,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), StorageError> {
        let router = self.assemble()?;
        router
            .serve_with_shutdown(addr, shutdown)
            .await
            .map_err(|e| StorageError::Internal(format!("grpc server error: {e}")))
    }
}

/// Builds the optional server TLS configuration from config: server
/// identity always, plus an optional client CA root for mTLS.
pub async fn build_tls_config(
    cfg: &GrpcTlsConfig,
) -> Result<Option<ServerTlsConfig>, StorageError> {
    if !cfg.enabled {
        return Ok(None);
    }
    let cert = tokio::fs::read(&cfg.cert_path).await?;
    let key = tokio::fs::read(&cfg.key_path).await?;
    let identity = Identity::from_pem(cert, key);
    let tls = ServerTlsConfig::new().identity(identity);
    let tls = match cfg
        .client_ca_path
        .as_deref()
        .filter(|p| !p.as_os_str().is_empty())
    {
        Some(ca_path) => {
            let ca = tokio::fs::read(ca_path).await?;
            tls.client_ca_root(Certificate::from_pem(ca))
        }
        None => tls,
    };
    Ok(Some(tls))
}
