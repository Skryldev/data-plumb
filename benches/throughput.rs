//! Criterion benchmarks for the ingest pipeline and read path.
//!
//! Run with: `cargo bench`

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use file_storage::config::AppConfig;
use file_storage::core::chunker;
use file_storage::core::compressor::Compressor;

fn bench_input(len: usize) -> Vec<u8> {
    let mut state = 0x9E3779B97F4A7C15u64;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

fn bench_chunker(c: &mut Criterion) {
    let cfg = file_storage::config::ChunkingConfig {
        min: 8192,
        avg: 32768,
        max: 131072,
    };
    let data = bench_input(4 * 1024 * 1024);
    c.bench_function("cdc_chunk_4mb", |b| {
        b.iter(|| {
            let chunks = chunker::chunk_slice(black_box(&data), &cfg);
            black_box(chunks.len())
        })
    });
}

fn bench_compress(c: &mut Criterion) {
    let comp = Compressor::new(3, 1024);
    let data = bench_input(128 * 1024);
    c.bench_function("zstd_compress_128kb", |b| {
        b.iter(|| {
            let out = comp.compress(black_box(&data));
            black_box(out.size_stored)
        })
    });
}

fn bench_pipeline_upload(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = AppConfig::for_tests().with_data_root(dir.path());
    let state = rt
        .block_on(file_storage::state::build_state(std::sync::Arc::new(cfg)))
        .unwrap();
    let data = std::sync::Arc::new(bench_input(2 * 1024 * 1024));

    let mut group = c.benchmark_group("upload_pipeline");
    group.sample_size(10);
    group.bench_function("upload_2mb", |b| {
        b.iter(|| {
            let data = data.clone();
            let stream = futures::stream::iter(
                data.chunks(64 * 1024)
                    .map(|c| {
                        Ok::<bytes::Bytes, file_storage::error::StorageError>(
                            bytes::Bytes::copy_from_slice(c),
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            let outcome = rt
                .block_on(state.0.pipeline.process(
                    uuid::Uuid::now_v7(),
                    "bench.bin",
                    "application/octet-stream",
                    vec![],
                    stream,
                ))
                .unwrap();
            black_box(outcome.manifest.size_original)
        })
    });
    group.finish();
}

fn bench_retrieval(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = AppConfig::for_tests().with_data_root(dir.path());
    let state = rt
        .block_on(file_storage::state::build_state(std::sync::Arc::new(cfg)))
        .unwrap();
    let data = bench_input(2 * 1024 * 1024);

    // Seed one file.
    let file_id = {
        let stream = futures::stream::iter(
            data.chunks(64 * 1024)
                .map(|c| {
                    Ok::<bytes::Bytes, file_storage::error::StorageError>(
                        bytes::Bytes::copy_from_slice(c),
                    )
                })
                .collect::<Vec<_>>(),
        );
        let outcome = rt
            .block_on(state.0.pipeline.process(
                state.0.cfg.tenants[0].id,
                "bench-read.bin",
                "application/octet-stream",
                vec![],
                stream,
            ))
            .unwrap();
        outcome.manifest.file_id
    };

    let mut group = c.benchmark_group("retrieval");
    group.sample_size(10);
    group.bench_function("download_2mb_cold", |b| {
        b.iter(|| {
            let m = rt.block_on(state.0.retrieval.manifest(&file_id)).unwrap();
            let stream = state.0.retrieval.stream_full(m);
            let total: usize = rt.block_on(async {
                use futures::StreamExt as _;
                stream.map(|r| r.unwrap()).count().await
            });
            black_box(total)
        })
    });
    group.bench_function("download_2mb_warm", |b| {
        b.iter(|| {
            let m = rt.block_on(state.0.retrieval.manifest(&file_id)).unwrap();
            let stream = state.0.retrieval.stream_full(m);
            let total: usize = rt.block_on(async {
                use futures::StreamExt as _;
                stream.map(|r| r.unwrap()).count().await
            });
            black_box(total)
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_chunker,
    bench_compress,
    bench_pipeline_upload,
    bench_retrieval
);
criterion_main!(benches);
