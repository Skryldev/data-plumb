//! Thin binary wrapper: all logic lives in the library so integration tests
//! and benchmarks can drive the exact same code paths.

#[tokio::main]
async fn main() {
    if let Err(e) = file_storage::run().await {
        eprintln!("fatal: {e:#}");
        std::process::exit(1);
    }
}
