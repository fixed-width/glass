//! Compile the vendored `proto/idb.proto` into a tonic gRPC client at build time
//! using protox (a pure-Rust protobuf compiler) — no `protoc` binary is required,
//! so the Linux CI box compiles this crate with no extra system tooling.
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/idb.proto";
    println!("cargo:rerun-if-changed={proto}");
    // protox parses the proto and returns a prost `FileDescriptorSet`.
    let fds = protox::compile([proto], ["proto"])?;
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    // tonic-build 0.12: `compile_fds` consumes a prost FileDescriptorSet and writes
    // the generated `<package>.rs` into `out_dir`. Client only (no server stubs).
    tonic_build::configure()
        .build_server(false)
        .out_dir(&out)
        .compile_fds(fds)?;
    Ok(())
}
