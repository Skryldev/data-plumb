//! Criterion throughput benchmarks (§9.3/§9.4).
//!
//! Content is documentation-like (compressible) — the spec's acceptance
//! targets assume a documentation workload, not incompressible random data.
//!
//! Run: `cargo bench` (or `cargo bench -- --sample-size 20` for a quick pass)

use criterion::{criterion_group, criterion_main, Criterion};
use file_storage::config::Config;
use file_storage::state::{bootstrap_tenant, AppState};
use file_storage::{build_app, storage::retrieval::RetrievalEngine};
use std::io::Cursor;
use std::sync::Arc;
use uuid::Uuid;

/// Deterministic, compressible, documentation-like content.
fn patterned(len: usize, seed: u64) -> Vec<u8> {
    let block: Vec<u8> = {
        let mut state = seed | 1;
        (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                b"Lorem ipsum dolor sit amet, documentation chunk.\n"[(state % 49) as usize]
            })
            .collect()
    };
    (0..len).map(|i| block[i % block.len()]).collect()
}

fn setup() -> (
    tokio::runtime::Runtime,
    Arc<AppState>,
    uuid::Uuid,
    tempfile::TempDir,
) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("dir");
    let config = Config::test_config(dir.path());
    let (router, state) = build_app(config).expect("app");
    let _ = router; // benches call the pipeline/engine directly (no HTTP)
    let tenant = uuid::Uuid::nil();
    rt.block_on(async {
        bootstrap_tenant(&state).await.expect("bootstrap");
    });
    (rt, Arc::new(state), tenant, dir)
}

fn bench_upload_10mb(c: &mut Criterion) {
    let (rt, state, tenant, _dir) = setup();
    // 20 distinct payloads so every iteration performs real work (no dedup).
    let payloads: Vec<Vec<u8>> = (0..20u64)
        .map(|i| patterned(10 * 1024 * 1024, 1000 + i))
        .collect();
    let mut it = payloads.into_iter().cycle();
    c.bench_function("upload_10mb", |b| {
        b.iter(|| {
            let data = it.next().expect("cycle");
            rt.block_on(async {
                let outcome = state
                    .pipeline
                    .process(
                        Cursor::new(data.clone()),
                        &data[..256.min(data.len())],
                        "application/octet-stream",
                        data.len() as u64,
                        tenant,
                        format!("bench-{uuid}.bin", uuid = Uuid::now_v7()),
                        Uuid::new_v4(),
                        vec![],
                    )
                    .await
                    .expect("upload");
                criterion::black_box(outcome.stored_bytes);
            })
        })
    });
}

fn bench_download_cached(c: &mut Criterion) {
    let (rt, state, tenant, _dir) = setup();
    let data = patterned(10 * 1024 * 1024, 7777);
    let manifest_id = rt.block_on(async {
        let outcome = state
            .pipeline
            .process(
                Cursor::new(data.clone()),
                &data[..256],
                "application/octet-stream",
                data.len() as u64,
                tenant,
                "cached.bin".into(),
                Uuid::new_v4(),
                vec![],
            )
            .await
            .expect("upload");
        // Warm the caches once.
        let mut sink = Vec::new();
        let engine = state.retrieval.clone();
        let manifest = engine
            .lookup_manifest(outcome.manifest.file_id, tenant)
            .expect("lookup");
        engine
            .retrieve_full(&manifest, &mut sink)
            .await
            .expect("warm read");
        outcome.manifest.file_id
    });
    c.bench_function("download_cached_10mb", |b| {
        b.iter(|| {
            rt.block_on(async {
                let engine: Arc<RetrievalEngine> = state.retrieval.clone();
                let manifest = engine.lookup_manifest(manifest_id, tenant).expect("lookup");
                let mut sink = Vec::new();
                engine
                    .retrieve_full(&manifest, &mut sink)
                    .await
                    .expect("read");
                criterion::black_box(sink.len());
            })
        })
    });
}

fn bench_download_cold(c: &mut Criterion) {
    let (rt, state, tenant, _dir) = setup();
    let data = patterned(10 * 1024 * 1024, 8888);
    let manifest_id = rt.block_on(async {
        let outcome = state
            .pipeline
            .process(
                Cursor::new(data.clone()),
                &data[..256],
                "application/octet-stream",
                data.len() as u64,
                tenant,
                "cold.bin".into(),
                Uuid::new_v4(),
                vec![],
            )
            .await
            .expect("upload");
        outcome.manifest.file_id
    });
    c.bench_function("download_cold_10mb", |b| {
        b.iter(|| {
            rt.block_on(async {
                // Invalidate caches to force the cold L1→L2→disk path.
                let manifest = state
                    .retrieval
                    .lookup_manifest(manifest_id, tenant)
                    .expect("lookup");
                let pairs: Vec<([u8; 32], [u8; 32])> = manifest
                    .chunks
                    .iter()
                    .map(|c| {
                        (
                            c.hash,
                            file_storage::storage::manifest::storage_key(
                                &manifest.tenant_id,
                                &c.hash,
                                c.encrypted,
                            ),
                        )
                    })
                    .collect();
                state
                    .cache
                    .invalidate_file_chunks(manifest_id, &pairs)
                    .await;
                let mut sink = Vec::new();
                state
                    .retrieval
                    .retrieve_full(&manifest, &mut sink)
                    .await
                    .expect("read");
                criterion::black_box(sink.len());
            })
        })
    });
}

fn bench_dedup_upload_10mb(c: &mut Criterion) {
    let (rt, state, tenant, _dir) = setup();
    let data = patterned(10 * 1024 * 1024, 9999);
    rt.block_on(async {
        state
            .pipeline
            .process(
                Cursor::new(data.clone()),
                &data[..256],
                "application/octet-stream",
                data.len() as u64,
                tenant,
                "dedup-seed.bin".into(),
                Uuid::new_v4(),
                vec![],
            )
            .await
            .expect("seed upload");
    });
    c.bench_function("dedup_upload_10mb", |b| {
        b.iter(|| {
            rt.block_on(async {
                let outcome = state
                    .pipeline
                    .process(
                        Cursor::new(data.clone()),
                        &data[..256],
                        "application/octet-stream",
                        data.len() as u64,
                        tenant,
                        format!("dedup-{}.bin", Uuid::now_v7()),
                        Uuid::new_v4(),
                        vec![],
                    )
                    .await
                    .expect("dedup upload");
                criterion::black_box(outcome.deduped_bytes);
            })
        })
    });
}

criterion_group!(
    benches,
    bench_upload_10mb,
    bench_download_cached,
    bench_download_cold,
    bench_dedup_upload_10mb
);
criterion_main!(benches);
