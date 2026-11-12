//! File storage microservice — hybrid chunked content-addressed storage.
//!
//! Pipeline: declared-MIME validation against magic bytes (H-03,
//! `validation.mode: enforce|off`) -> FastCDC chunking -> BLAKE3 hashing ->
//! zstd compression -> AES-256-GCM (optional, per-tenant) ->
//! content-addressed block store, with redb as the ACID metadata index and
//! moka as the three-tier cache.

pub mod api;
pub mod cache;
pub mod config;
pub mod core;
pub mod error;
pub mod gc;
pub mod grpc;
pub mod metrics;
pub mod replication;
pub mod security;
pub mod state;
pub mod storage;

use anyhow::Context as _;
use argon2::PasswordHasher as _;
use clap::{Parser, Subcommand};
use std::future::IntoFuture as _;

/// Command line interface of the `file-storage` binary.
#[derive(Debug, Parser)]
#[command(
    name = "file-storage",
    version,
    about = "Hybrid chunked content-addressed file storage service"
)]
pub struct Cli {
    /// Path to the YAML configuration file. In containers the path MUST be
    /// an operator-mounted secret (C-01); `FS_CONFIG` overrides.
    #[arg(long, env = "FS_CONFIG", default_value = "config/default.yaml")]
    pub config: String,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Generate an argon2id PHC string for `auth.users[].password_hash`.
    HashPassword {
        /// Plaintext password to hash. When omitted, read from stdin
        /// (no echo) so the secret never lands in shell history.
        #[arg(long, env = "FS_HASH_PASSWORD")]
        password: Option<String>,
    },
}

/// Boots the dual-stack service: REST and gRPC listeners in one process,
/// sharing the same `AppState`, with a joint graceful shutdown. Returns
/// after shutdown completes.
pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(Command::HashPassword { password }) = cli.command {
        let password = match password {
            Some(p) => p,
            None => {
                // C-01: stdin path keeps the secret out of argv/history.
                eprintln!("enter password to hash:");
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                    .map_err(|e| anyhow::anyhow!("stdin read failed: {e}"))?;
                buf.trim_end_matches(['\r', '\n']).to_string()
            }
        };
        let salt = argon2::password_hash::SaltString::generate(
            &mut argon2::password_hash::rand_core::OsRng,
        );
        let phc = argon2::Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("argon2 hashing failed: {e}"))?
            .to_string();
        println!("{phc}");
        return Ok(());
    }

    let cfg = std::sync::Arc::new(
        config::AppConfig::load(&cli.config)
            .context(format!("failed to load configuration from {}", cli.config))?,
    );
    cfg.validate().context("invalid configuration")?;

    metrics::init_recorder();

    let state = state::build_state(cfg.clone())
        .await
        .context("failed to initialise service state")?;
    let app = api::router::build_router(state.clone());

    let rest_addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let rest_listener = tokio::net::TcpListener::bind(&rest_addr)
        .await
        .with_context(|| format!("failed to bind {rest_addr}"))?;
    tracing::info!(
        addr = %rest_addr,
        version = env!("CARGO_PKG_VERSION"),
        "file-storage REST listening"
    );

    let grpc_addr: std::net::SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.grpc_port)
        .parse()
        .context("invalid grpc bind address")?;
    let grpc_tls = grpc::server::build_tls_config(&cfg.server.grpc_tls)
        .await
        .context("failed to load gRPC TLS material")?;
    if grpc_tls.is_some() {
        tracing::info!(addr = %grpc_addr, "file-storage gRPC listening (TLS)"
        );
    } else {
        tracing::info!(addr = %grpc_addr, "file-storage gRPC listening");
    }

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    // A watch channel fans the shutdown signal out to both transports so
    // each stops accepting and drains its in-flight requests/streams.
    let (shutdown_tx, rest_rx) = tokio::sync::watch::channel(false);
    let mut grpc_rx = shutdown_tx.subscribe();

    let rest = axum::serve(
        rest_listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let mut rest_rx = rest_rx;
        let _ = rest_rx.changed().await;
    })
    .into_future();
    tokio::pin!(rest);

    let grpc_server = grpc::server::GrpcServer::new(state.clone(), grpc_tls);
    let grpc = grpc_server.serve(grpc_addr, async move {
        let _ = grpc_rx.changed().await;
    });
    tokio::pin!(grpc);

    // Any listener error, or the shutdown signal, ends the process; every
    // path below drains the other transport first.
    let outcome: anyhow::Result<()> = tokio::select! {
        r = &mut rest => {
            let _ = shutdown_tx.send(true);
            match r {
                Ok(()) => Ok(()),
                Err(e) => Err(anyhow::Error::new(e).context("REST server error")),
            }
        }
        g = &mut grpc => {
            let _ = shutdown_tx.send(true);
            g.map_err(anyhow::Error::new)
        }
        _ = &mut shutdown => {
            tracing::info!("shutdown signal received: draining listeners");
            let _ = shutdown_tx.send(true);
            let (r, g) = tokio::join!(&mut rest, &mut grpc);
            match (r, g) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(e), _) => Err(anyhow::Error::new(e).context("REST server error")),
                (_, Err(e)) => Err(anyhow::Error::new(e).context("gRPC server error")),
            }
        }
    };

    tracing::info!("shutting down: flushing replication queue");
    state.0.replicator.shutdown().await;
    tracing::info!("bye");
    outcome
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::warn!(error = %e, "cannot install SIGTERM handler"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
