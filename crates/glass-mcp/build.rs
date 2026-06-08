//! Embed a Per-Monitor-V2 (+ long-path-aware) application manifest on the
//! glass-mcp executable. The manifest is a property of the *process* that drives
//! capture/input on Windows (this binary), so it lives here, not in the
//! glass-windows library (a lib can't carry a manifest — embed-manifest emits
//! cargo:rustc-link-arg-bins, which requires a bin target). Gated on the build
//! *target* so a Linux/native build is unaffected and a Windows build embeds it.
//! Mirrors the validated probe at tools/windows-validation/build.rs.
use embed_manifest::manifest::{DpiAwareness, Setting};
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_manifest(
            new_manifest("Glass.Mcp")
                .dpi_awareness(DpiAwareness::PerMonitorV2)
                .long_path_aware(Setting::Enabled),
        )
        .expect("failed to embed application manifest");
    }
    // Statically link only the VCRuntime, leaving the OS-provided UCRT dynamic (paired
    // with `+crt-static` in .cargo/config.toml). Gated to a Windows host: that's where
    // the only commercial msvc builds happen (CI + the dev box), and it keeps the crate
    // off non-Windows builds entirely so build.rs always compiles there.
    #[cfg(windows)]
    static_vcruntime::metabuild();
    println!("cargo:rerun-if-changed=build.rs");
}
