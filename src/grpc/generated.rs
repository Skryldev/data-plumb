//! Generated protobuf/tonic types for `file_storage.v1` (single prost
//! package module, produced by `build.rs`).

/// All messages and service modules of `package file_storage.v1`.
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/file_storage.v1.rs"));
}

pub use pb::admin_service_client::AdminServiceClient;
pub use pb::admin_service_server::{AdminService, AdminServiceServer};
pub use pb::auth_service_client::AuthServiceClient;
pub use pb::auth_service_server::{AuthService, AuthServiceServer};
pub use pb::file_service_client::FileServiceClient;
pub use pb::file_service_server::{FileService, FileServiceServer};
pub use pb::replication_service_client::ReplicationServiceClient;
pub use pb::replication_service_server::{ReplicationService, ReplicationServiceServer};
pub use pb::{
    AuditEntry, ChunkMeta, ChunkRef, CreateTenantRequest, DeleteFile, DeleteFileRequest,
    DeleteFileResponse, DownloadChunk, DownloadHeader, DownloadRequest, Empty, Flush, GcResult,
    GetManifestRequest, GetTenantRequest, ListFilesRequest, ListFilesResponse, ListTenantsRequest,
    ListTenantsResponse, LoginRequest, LoginResponse, LogoutRequest, Manifest, ManifestBundle,
    PageRequest, PullChunkRequest, PullChunkResponse, PullManifestRequest, PutChunk, PutManifest,
    QueryAuditRequest, QueryAuditResponse, RefreshRequest, ReplicationAck, ReplicationOp,
    ResyncRequest, ResyncResult, Tenant, TriggerGcRequest, UploadChunk, UploadHeader,
    UploadResponse,
};

/// Encoded `FileDescriptorProto`s for every compiled proto — fed to the
/// gRPC reflection service so `grpcurl` can discover RPCs without files.
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/file_storage_v1_descriptor.bin"));

/// Service names advertised over reflection and health checking.
pub const SERVICE_NAMES: [&str; 4] = [
    "file_storage.v1.FileService",
    "file_storage.v1.AuthService",
    "file_storage.v1.AdminService",
    "file_storage.v1.ReplicationService",
];
