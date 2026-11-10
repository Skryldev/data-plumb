//! Prometheus metrics. A single global recorder is installed once per process
//! (multi-instance test processes share it; labels keep series distinguishable).

use metrics_exporter_prometheus::PrometheusHandle;
use std::sync::OnceLock;

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Installs the global Prometheus recorder exactly once.
pub fn init_recorder() {
    let build = || {
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .install_recorder()
            .expect("failed to install prometheus recorder")
    };
    let _ = HANDLE.get_or_init(build);
}

/// Renders the Prometheus exposition text for `GET /metrics`.
pub fn render() -> String {
    match HANDLE.get() {
        Some(h) => h.render(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Metric name constants (single source of truth).
// ---------------------------------------------------------------------------

pub const M_UPLOADS: &str = "file_storage_uploads_total";
pub const M_DOWNLOADS: &str = "file_storage_downloads_total";
pub const M_UPLOAD_DURATION: &str = "file_storage_upload_duration_seconds";
pub const M_DOWNLOAD_DURATION: &str = "file_storage_download_duration_seconds";
pub const M_BYTES_WRITTEN: &str = "file_storage_bytes_written_total";
pub const M_BYTES_READ: &str = "file_storage_bytes_read_total";
pub const M_DEDUP_HITS: &str = "file_storage_dedup_hits_total";
pub const M_DEDUP_BYTES: &str = "file_storage_dedup_bytes_saved_total";
pub const M_CACHE_HITS: &str = "file_storage_cache_hits_total";
pub const M_CHUNKS_STORED: &str = "file_storage_chunks_stored_total";
pub const M_REPL_QUEUE: &str = "file_storage_replication_queue_depth";
pub const M_REPL_FAILURES: &str = "file_storage_replication_failures_total";
pub const M_REPL_APPLIED: &str = "file_storage_replication_ops_applied_total";
pub const M_REPL_LATENCY: &str = "file_storage_replication_flush_seconds";
pub const M_HASH_MISMATCH: &str = "file_storage_hash_mismatch_total";
pub const M_GC_FILES: &str = "file_storage_gc_files_purged_total";
pub const M_GC_BYTES: &str = "file_storage_gc_bytes_reclaimed_total";
pub const M_GC_ORPHANS: &str = "file_storage_gc_orphans_removed_total";
pub const M_REQUEST_DURATION: &str = "file_storage_http_request_duration_seconds";
pub const M_REQUESTS: &str = "file_storage_http_requests_total";
pub const M_RPC_DURATION: &str = "file_storage_grpc_request_duration_seconds";
pub const M_RPC_REQUESTS: &str = "file_storage_grpc_requests_total";
pub const M_AUDIT_EVENTS: &str = "file_storage_audit_events_total";
pub const M_UPTIME: &str = "file_storage_uptime_seconds";

/// Records an upload completion.
pub fn record_upload(dur_secs: f64, status: &'static str, bytes: u64, dedup_bytes: u64) {
    metrics::counter!(M_UPLOADS, "status" => status).increment(1);
    metrics::histogram!(M_UPLOAD_DURATION, "status" => status).record(dur_secs);
    metrics::counter!(M_BYTES_WRITTEN).increment(bytes);
    if dedup_bytes > 0 {
        metrics::counter!(M_DEDUP_BYTES).increment(dedup_bytes);
    }
}

/// Records a download completion. `source` ∈ {l1, l2, disk, mixed}.
pub fn record_download(dur_secs: f64, status: &'static str, bytes: u64, source: &'static str) {
    metrics::counter!(M_DOWNLOADS, "status" => status, "source" => source).increment(1);
    metrics::histogram!(M_DOWNLOAD_DURATION, "status" => status).record(dur_secs);
    metrics::counter!(M_BYTES_READ).increment(bytes);
}

pub fn record_cache_hit(level: &'static str) {
    metrics::counter!(M_CACHE_HITS, "level" => level).increment(1);
}

pub fn record_chunks_stored(n: u64, deduped: u64) {
    metrics::counter!(M_CHUNKS_STORED).increment(n);
    if deduped > 0 {
        metrics::counter!(M_DEDUP_HITS).increment(deduped);
    }
}

pub fn record_hash_mismatch() {
    metrics::counter!(M_HASH_MISMATCH).increment(1);
}

pub fn record_replication_flush(dur_secs: f64, ok: bool) {
    metrics::histogram!(M_REPL_LATENCY, "ok" => if ok { "true" } else { "false" }).record(dur_secs);
    if !ok {
        metrics::counter!(M_REPL_FAILURES).increment(1);
    }
}

pub fn record_replication_applied(n: u64) {
    metrics::counter!(M_REPL_APPLIED).increment(n);
}

pub fn set_replication_queue_depth(d: f64) {
    metrics::gauge!(M_REPL_QUEUE).set(d);
}

pub fn record_gc(files: u64, bytes: u64, orphans: u64) {
    if files > 0 {
        metrics::counter!(M_GC_FILES).increment(files);
    }
    if bytes > 0 {
        metrics::counter!(M_GC_BYTES).increment(bytes);
    }
    if orphans > 0 {
        metrics::counter!(M_GC_ORPHANS).increment(orphans);
    }
}

pub fn record_http_request(method: &str, route: &str, status: u16, dur_secs: f64) {
    metrics::counter!(M_REQUESTS, "method" => method.to_owned(), "route" => route.to_owned(), "status" => status.to_string()).increment(1);
    metrics::histogram!(M_REQUEST_DURATION, "method" => method.to_owned(), "route" => route.to_owned())
        .record(dur_secs);
}

/// Records one gRPC call (unary and streaming). `rpc` is the full method
/// path (`/file_storage.v1.FileService/Upload`), `code` the gRPC status
/// name (`ok` on success) — dashboards group by transport via the metric
/// family itself.
pub fn record_grpc_request(rpc: &str, code: &str, dur_secs: f64) {
    metrics::counter!(M_RPC_REQUESTS, "rpc" => rpc.to_owned(), "code" => code.to_owned())
        .increment(1);
    metrics::histogram!(M_RPC_DURATION, "rpc" => rpc.to_owned(), "code" => code.to_owned())
        .record(dur_secs);
}

pub fn record_audit(action: &str) {
    metrics::counter!(M_AUDIT_EVENTS, "action" => action.to_owned()).increment(1);
}

pub fn set_uptime(secs: f64) {
    metrics::gauge!(M_UPTIME).set(secs);
}

/// Normalises a request path so per-route metric cardinality stays bounded
/// (`/files/0193…-…` -> `/files/:id`).
pub fn normalize_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if seg.len() == 36 && seg.chars().filter(|c| *c == '-').count() == 4 {
                ":id"
            } else if seg.len() == 64 && seg.chars().all(|c| c.is_ascii_hexdigit()) {
                ":hash"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}
