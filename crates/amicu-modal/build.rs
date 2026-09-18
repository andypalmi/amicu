//! Generates the tonic/prost client for Modal's control-plane RPC service
//! from the vendored `proto/modal_proto/api.proto` (see `proto/VENDORED.md`
//! for the upstream pin).
//!
//! `protoc` is not assumed to be installed — CI runners and the Docker
//! `cargo-chef` build stage have neither, so this points `prost-build` at
//! the `protoc` binary and well-known-types include directory bundled by
//! `protoc-bin-vendored` instead of relying on a `PROTOC` environment
//! variable or a system install.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_dir = PathBuf::from("proto");
    let proto_file = proto_dir.join("modal_proto/api.proto");
    println!("cargo:rerun-if-changed={}", proto_file.display());

    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let protoc_include = protoc_bin_vendored::include_path()?;

    let mut prost_config = tonic_prost_build::Config::new();
    prost_config.protoc_executable(protoc);

    tonic_prost_build::configure()
        // Client-only crate: no generated server trait/module.
        .build_server(false)
        .compile_with_config(prost_config, &[proto_file], &[proto_dir, protoc_include])?;

    Ok(())
}
