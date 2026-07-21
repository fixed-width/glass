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

    // The user-facing version. The crate version is pinned at 0.0.0 (releases are tag-driven,
    // not version-bumped), so derive the real version at build time: the release tag in CI, else
    // the nearest git tag for a local build, else the crate version as a last resort. Consumed via
    // `env!("GLASS_VERSION")` by `--version`, `doctor`, and the MCP handshake so none report 0.0.0.
    println!("cargo:rustc-env=GLASS_VERSION={}", glass_version());
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_TYPE");
    println!("cargo:rerun-if-changed=build.rs");
}

/// Resolve the version string embedded at build time. Strips a leading `v` so `v1.0.1` → `1.0.1`.
fn glass_version() -> String {
    // Release builds are a TAG push in CI, where `GITHUB_REF_TYPE=tag` and `GITHUB_REF_NAME` is the
    // tag (e.g. `v1.0.1`). Gate on the ref TYPE: `GITHUB_REF_NAME` is also set on branch/PR builds
    // (as the branch name), which must NOT become the version.
    if std::env::var("GITHUB_REF_TYPE").as_deref() == Ok("tag") {
        if let Ok(tag) = std::env::var("GITHUB_REF_NAME") {
            let v = tag.strip_prefix('v').unwrap_or(&tag).trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    // Local builds: derive from the nearest tag (with a `-dirty` / commit suffix when not exactly
    // on a tag), so a dev binary reports an honest version rather than 0.0.0.
    if let Ok(out) = std::process::Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
    {
        if out.status.success() {
            let d = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !d.is_empty() {
                return d.strip_prefix('v').unwrap_or(&d).to_string();
            }
        }
    }
    // No tag and no git (e.g. a source tarball with no VCS): fall back to the crate version.
    env!("CARGO_PKG_VERSION").to_string()
}
