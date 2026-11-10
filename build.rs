//! Compiles the `file_storage.v1` protobuf definitions with tonic-build.
//!
//! protoc is supplied by `protoc-bin-vendored`, so the build is hermetic:
//! no system protobuf installation is required. The generated descriptor set
//! is exported for the gRPC reflection service (`src/grpc/server.rs`).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let protoc_include = protoc_bin_vendored::include_path()?;
    // build scripts are single-threaded before compilation starts.
    std::env::set_var("PROTOC", protoc);
    std::env::set_var("PROTOC_INCLUDE", protoc_include);

    let descriptor =
        std::path::Path::new(&std::env::var("OUT_DIR")?).join("file_storage_v1_descriptor.bin");

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .file_descriptor_set_path(&descriptor)
        .compile_protos(
            &[
                "proto/file_storage/v1/common.proto",
                "proto/file_storage/v1/file.proto",
                "proto/file_storage/v1/auth.proto",
                "proto/file_storage/v1/admin.proto",
                "proto/file_storage/v1/replication.proto",
            ],
            &["proto"],
        )?;

    println!("cargo:rerun-if-changed=proto/");
    Ok(())
}
