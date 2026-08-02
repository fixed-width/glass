//! Compile the vendored `proto/idb.proto` into a tonic gRPC client at build time
//! using protox (a pure-Rust protobuf compiler) — no `protoc` binary is required,
//! so the Linux CI box compiles this crate with no extra system tooling.
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The generated client's only consumer, the `idb` module, is `#![cfg(unix)]`-gated
    // in lib.rs; skip codegen when it won't be compiled in. `CARGO_CFG_UNIX` reflects the
    // compile target, not the host, so this holds under cross-compilation too.
    if std::env::var_os("CARGO_CFG_UNIX").is_none() {
        return Ok(());
    }
    let proto = "proto/idb.proto";
    println!("cargo:rerun-if-changed={proto}");
    // protox parses the proto and returns a prost `FileDescriptorSet`.
    let fds = protox::compile([proto], ["proto"])?;
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    // `compile_fds` consumes a prost FileDescriptorSet and writes the generated
    // `<package>.rs` into `out_dir`. Client only (no server stubs). tonic 0.14 moved
    // prost codegen out of `tonic-build` into `tonic-prost-build`, whose output leans on
    // the `tonic-prost` runtime crate for the codec.
    // `build_transport(false)`: the generated client would otherwise carry an inherent
    // `connect(dst)` constructor for `Channel`, which collides with idb's own `connect`
    // RPC of the same name. glass dials the UDS itself and hands the ready `Channel` to
    // `CompanionServiceClient::new`, so the constructor is dead weight either way.
    tonic_prost_build::configure()
        .build_server(false)
        .build_transport(false)
        .out_dir(&out)
        .compile_fds(fds)?;
    Ok(())
}
