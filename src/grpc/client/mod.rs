//! Public Rust client SDK for the `file_storage.v1` gRPC API.
//!
//! [`FileStorageClient`] wraps the generated tonic client with
//! convenience helpers (streaming upload from any `AsyncRead`, streaming
//! download into any `AsyncWrite`), metadata handling and an opt-in
//! retry policy for unary calls.

pub mod retry;

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio_stream::wrappers::ReceiverStream;
use tonic::codec::CompressionEncoding;
use tonic::metadata::AsciiMetadataValue;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};
use tonic::{Request, Status};

use crate::grpc::generated::pb;
use crate::grpc::generated::{
    AdminServiceClient, AuthServiceClient, FileServiceClient, ReplicationServiceClient,
};
use retry::RetryPolicy;

/// Message ceiling mirrored from the server configuration.
const MAX_MESSAGE_SIZE: usize = 256 * 1024 * 1024;

/// Bytes per client-streaming upload message.
const UPLOAD_PIECE_SIZE: usize = 1024 * 1024;

/// High-level client for the file + auth + admin APIs.
#[derive(Clone)]
pub struct FileStorageClient {
    files: FileServiceClient<Channel>,
    auth: AuthServiceClient<Channel>,
    admin: AdminServiceClient<Channel>,
    token: Option<String>,
    retry: RetryPolicy,
}

/// Client for the internal replication protocol (x-api-key auth).
#[derive(Clone)]
pub struct ReplicationClient {
    inner: ReplicationServiceClient<Channel>,
    api_key: String,
}

impl FileStorageClient {
    /// Connects over plaintext (`endpoint` like `http://127.0.0.1:9090`).
    pub async fn connect(endpoint: &str) -> Result<Self, Status> {
        crate::grpc::server::ensure_crypto_provider();
        let channel = Channel::from_shared(endpoint.to_string())
            .map_err(|e| Status::invalid_argument(format!("bad endpoint: {e}")))?
            .connect()
            .await
            .map_err(|e| Status::unavailable(format!("connect failed: {e}")))?;
        Ok(Self::from_channel(channel))
    }

    /// Connects over TLS, optionally pinning a custom CA PEM.
    pub async fn connect_tls(endpoint: &str, ca_pem: Option<&[u8]>) -> Result<Self, Status> {
        crate::grpc::server::ensure_crypto_provider();
        let mut tls = ClientTlsConfig::new().with_native_roots();
        if let Some(pem) = ca_pem {
            tls = tls.ca_certificate(Certificate::from_pem(pem));
        }
        let channel = Channel::from_shared(endpoint.to_string())
            .map_err(|e| Status::invalid_argument(format!("bad endpoint: {e}")))?
            .tls_config(tls)
            .map_err(|e| Status::invalid_argument(format!("tls config: {e}")))?
            .connect()
            .await
            .map_err(|e| Status::unavailable(format!("connect failed: {e}")))?;
        Ok(Self::from_channel(channel))
    }

    fn from_channel(channel: Channel) -> Self {
        let files = FileServiceClient::new(channel.clone())
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE);
        let auth = AuthServiceClient::new(channel.clone())
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip);
        let admin = AdminServiceClient::new(channel)
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE);
        Self {
            files,
            auth,
            admin,
            token: None,
            retry: RetryPolicy::default(),
        }
    }

    /// Sets the bearer token used on every subsequent call.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// The current bearer token (tests / diagnostics).
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Admin: query the audit trail.
    pub async fn query_audit(
        &self,
        limit: u32,
        tenant_id: &str,
    ) -> Result<pb::QueryAuditResponse, Status> {
        let op = || async {
            self.admin
                .clone()
                .query_audit(self.authed(pb::QueryAuditRequest {
                    limit,
                    tenant_id: tenant_id.to_string(),
                }))
                .await
                .map(|r| r.into_inner())
        };
        self.retry.run(op).await
    }

    /// Replaces the retry policy for unary calls.
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    fn authed<T>(&self, msg: T) -> Request<T> {
        let mut req = Request::new(msg);
        self.attach_bearer(req.metadata_mut());
        req
    }

    /// Best-effort bearer metadata insertion (tokens are ASCII JWTs, so
    /// this practically never fails).
    fn attach_bearer(&self, metadata: &mut tonic::metadata::MetadataMap) {
        if let Some(token) = &self.token {
            if let Ok(mut v) = AsciiMetadataValue::try_from(format!("Bearer {token}").as_str()) {
                v.set_sensitive(true);
                metadata.insert("authorization", v);
            }
        }
    }

    /// Exchanges credentials for a token pair and stores the access token.
    pub async fn login(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<pb::LoginResponse, Status> {
        let pair = self
            .auth
            .login(pb::LoginRequest {
                username: username.to_string(),
                password: password.to_string(),
            })
            .await?
            .into_inner();
        self.token = Some(pair.access_token.clone());
        Ok(pair)
    }

    /// Rotates the refresh token for a fresh pair.
    pub async fn refresh(&mut self, refresh_token: &str) -> Result<pb::LoginResponse, Status> {
        let pair = self
            .auth
            .refresh(pb::RefreshRequest {
                refresh_token: refresh_token.to_string(),
            })
            .await?
            .into_inner();
        self.token = Some(pair.access_token.clone());
        Ok(pair)
    }

    /// Revokes a refresh token.
    pub async fn logout(&mut self, refresh_token: &str) -> Result<(), Status> {
        self.auth
            .logout(pb::LogoutRequest {
                token: refresh_token.to_string(),
            })
            .await?
            .into_inner();
        Ok(())
    }

    /// Uploads a file from any `AsyncRead` as 1 MiB client-streaming
    /// pieces; the server performs content-defined chunking.
    pub async fn upload<R>(
        &self,
        header: pb::UploadHeader,
        reader: R,
    ) -> Result<pb::UploadResponse, Status>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let (tx, rx) = tokio::sync::mpsc::channel::<pb::UploadChunk>(32);
        let head = pb::UploadChunk {
            payload: Some(pb::upload_chunk::Payload::Header(header)),
        };
        tx.send(head)
            .await
            .map_err(|_| Status::internal("upload channel closed"))?;

        // Move the sender into the reader task: when the task ends the
        // sender drops, closing the stream so the server observes EOF.
        tokio::spawn(async move {
            let mut reader = reader;
            let mut buf = vec![0u8; UPLOAD_PIECE_SIZE];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let frame = pb::UploadChunk {
                            payload: Some(pb::upload_chunk::Payload::Data(buf[..n].to_vec())),
                        };
                        if tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        let response = self
            .files
            .clone()
            .upload(self.authed_stream(stream))
            .await?
            .into_inner();
        Ok(response)
    }

    fn authed_stream<S>(&self, stream: S) -> Request<S> {
        let mut req = Request::new(stream);
        self.attach_bearer(req.metadata_mut());
        req
    }

    /// Downloads a whole file into `writer`; returns the header (with the
    /// original BLAKE3 for verification).
    pub async fn download<W>(
        &self,
        file_id: &str,
        writer: &mut W,
    ) -> Result<pb::DownloadHeader, Status>
    where
        W: AsyncWrite + Unpin + Send,
    {
        self.download_range(file_id, None, None, writer).await
    }

    /// Downloads the half-open byte range `[start, end)` (either may be
    /// `None`) into `writer`.
    pub async fn download_range<W>(
        &self,
        file_id: &str,
        start: Option<u64>,
        end: Option<u64>,
        writer: &mut W,
    ) -> Result<pb::DownloadHeader, Status>
    where
        W: AsyncWrite + Unpin + Send,
    {
        let mut stream = self
            .files
            .clone()
            .download(self.authed(pb::DownloadRequest {
                file_id: file_id.to_string(),
                start,
                end,
            }))
            .await?
            .into_inner();

        let mut header: Option<pb::DownloadHeader> = None;
        while let Some(chunk) = stream.message().await? {
            match chunk.payload {
                Some(pb::download_chunk::Payload::Header(h)) => header = Some(h),
                Some(pb::download_chunk::Payload::Data(data)) => {
                    writer
                        .write_all(&data)
                        .await
                        .map_err(|e| Status::internal(format!("write failed: {e}")))?;
                }
                None => {}
            }
        }
        header.ok_or_else(|| Status::data_loss("download stream ended without a header"))
    }

    /// Manifest lookup without content.
    pub async fn get_manifest(
        &self,
        file_id: &str,
        include_chunks: bool,
    ) -> Result<pb::Manifest, Status> {
        let op = || async {
            self.files
                .clone()
                .get_manifest(self.authed(pb::GetManifestRequest {
                    file_id: file_id.to_string(),
                    include_chunks,
                }))
                .await
                .map(|r| r.into_inner())
        };
        self.retry.run(op).await
    }

    /// Tenant file listing.
    pub async fn list_files(
        &self,
        limit: u32,
        offset: u32,
        tag: &str,
        name: &str,
        include_deleted: bool,
    ) -> Result<pb::ListFilesResponse, Status> {
        let op = || async {
            self.files
                .clone()
                .list_files(self.authed(pb::ListFilesRequest {
                    limit,
                    offset,
                    tag: tag.to_string(),
                    name: name.to_string(),
                    include_deleted,
                }))
                .await
                .map(|r| r.into_inner())
        };
        self.retry.run(op).await
    }

    /// Soft delete (`purge=false`) or hard purge (`purge=true`, admin).
    pub async fn delete_file(
        &self,
        file_id: &str,
        purge: bool,
    ) -> Result<pb::DeleteFileResponse, Status> {
        let op = || async {
            self.files
                .clone()
                .delete_file(self.authed(pb::DeleteFileRequest {
                    file_id: file_id.to_string(),
                    purge,
                }))
                .await
                .map(|r| r.into_inner())
        };
        self.retry.run(op).await
    }

    /// Admin: trigger one GC pass.
    pub async fn trigger_gc(&self, mode: i32, dry_run: bool) -> Result<pb::GcResult, Status> {
        let op = || async {
            self.admin
                .clone()
                .trigger_gc(self.authed(pb::TriggerGcRequest { mode, dry_run }))
                .await
                .map(|r| r.into_inner())
        };
        self.retry.run(op).await
    }

    /// Admin: list tenants.
    pub async fn list_tenants(&self, limit: u32) -> Result<pb::ListTenantsResponse, Status> {
        let op = || async {
            self.admin
                .clone()
                .list_tenants(self.authed(pb::ListTenantsRequest { limit }))
                .await
                .map(|r| r.into_inner())
        };
        self.retry.run(op).await
    }
}

impl ReplicationClient {
    /// Connects and stores the cluster API key for x-api-key metadata.
    pub async fn connect(endpoint: &str, api_key: &str) -> Result<Self, Status> {
        let channel = Channel::from_shared(endpoint.to_string())
            .map_err(|e| Status::invalid_argument(format!("bad endpoint: {e}")))?
            .connect()
            .await
            .map_err(|e| Status::unavailable(format!("connect failed: {e}")))?;
        Ok(Self {
            inner: ReplicationServiceClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_SIZE)
                .max_encoding_message_size(MAX_MESSAGE_SIZE),
            api_key: api_key.to_string(),
        })
    }

    fn keyed<T>(&self, msg: T) -> Request<T> {
        let mut req = Request::new(msg);
        if let Ok(mut v) = AsciiMetadataValue::try_from(self.api_key.as_str()) {
            v.set_sensitive(true);
            req.metadata_mut().insert("x-api-key", v);
        }
        req
    }

    /// Pulls one raw chunk blob (repair path).
    pub async fn pull_chunk(&self, hash: &[u8; 32]) -> Result<Vec<u8>, Status> {
        let mut stream = self
            .inner
            .clone()
            .pull_chunk(self.keyed(pb::PullChunkRequest {
                hash: hash.to_vec(),
            }))
            .await?
            .into_inner();
        let mut out = Vec::new();
        while let Some(frame) = stream.message().await? {
            match frame.payload {
                Some(pb::pull_chunk_response::Payload::Data(data)) => out.extend_from_slice(&data),
                Some(pb::pull_chunk_response::Payload::Header(_)) => {}
                None => {}
            }
        }
        Ok(out)
    }

    /// Pulls a manifest bundle (metadata only).
    pub async fn pull_manifest(&self, file_id: &str) -> Result<pb::ManifestBundle, Status> {
        Ok(self
            .inner
            .clone()
            .pull_manifest(self.keyed(pb::PullManifestRequest {
                file_id: file_id.to_string(),
            }))
            .await?
            .into_inner())
    }

    /// Access to the raw generated client for advanced streaming use.
    pub fn raw(&self) -> &ReplicationServiceClient<Channel> {
        &self.inner
    }

    /// The configured connect timeout helper for health polling.
    pub fn probe_interval() -> Duration {
        Duration::from_secs(2)
    }
}
