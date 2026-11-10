//! gRPC transport: generated protobuf types, server, interceptors, services
//! and the Rust client SDK.
//!
//! The gRPC server runs as a second listener in the same process and shares
//! the whole [`crate::state::AppState`] stack (index, block store, caches,
//! pipeline, retrieval, auth) with the REST API.

// tonic::Status is a large enum by design; wrapping every client/server
// signature in Box would fight the ecosystem.
#![allow(clippy::result_large_err)]

pub mod client;
pub mod convert;
pub mod generated;
pub mod interceptors;
pub mod proto_auth;
pub mod server;
pub mod services;

pub use client::FileStorageClient;
pub use server::GrpcServer;
