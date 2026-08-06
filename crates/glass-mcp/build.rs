//! Embed a Per-Monitor-V2 (+ long-path-aware) application manifest on the
//! glass-mcp executable. The manifest is a property of the *process* that drives
//! capture/input on Windows (this binary), so it lives here, not in the
//! glass-windows library (a lib can't carry a manifest — embed-manifest emits
//! cargo:rustc-link-arg-bins, which requires a bin target). Gated on the build
//! *target* so a Linux/native build is unaffected and a Windows build embeds it.
//! Mirrors the validated probe at tools/windows-validation/build.rs.
use embed_manifest::manifest::{DpiAwareness, Setting};
use embed_manifest::{embed_manifest, new_manifest};
use std::path::Path;
use std::process::Command;

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
    // with `+crt-static` in .cargo/config.toml). Gated to a Windows host — the msvc
    // toolchain this configures only exists there — so build.rs still compiles elsewhere.
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
    watch_git_head();
}

/// Declare the git files a local build's version is derived from, so cargo re-runs this script when
/// the commit moves.
///
/// Without them the only triggers are `build.rs` itself and the two env vars above, so a rebuild
/// after committing keeps the version string of whatever commit last forced a re-run: a binary was
/// observed reporting `1.1.0-84-g96fac4f` while its own tree was 17 commits further on. A CI tag
/// build takes its version from `GITHUB_REF_NAME` and does not depend on any of this.
///
/// The `-dirty` suffix reflects the working tree, which no declared path can stand in for, so an
/// uncommitted edit still needs this file touched to surface.
fn watch_git_head() {
    // HEAD moves on checkout; the ref it names moves on commit; `packed-refs` is where a gc'd tag
    // or branch ends up, so `describe` can read its answer from there instead.
    watch_git_path("HEAD");
    watch_git_path("packed-refs");
    // A detached HEAD names no ref, and HEAD alone already covers it.
    if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        watch_git_path(&head_ref);
    }
}

/// Declare one path inside the git directory, if it is there.
///
/// Resolved with `--git-path` rather than joined onto `.git/`: in a worktree the refs live in the
/// common directory, not beside that worktree's HEAD. A path that does not exist is skipped
/// deliberately — cargo re-runs a build script unconditionally when a declared path is missing, and
/// a source tarball with no VCS would take that penalty on every build for a version it never reads.
fn watch_git_path(name: &str) {
    let Some(path) = git_output(&["rev-parse", "--git-path", name]) else {
        return;
    };
    if Path::new(&path).exists() {
        println!("cargo:rerun-if-changed={path}");
    }
}

/// Run a git command and return its trimmed stdout, or `None` if git is absent or the command
/// failed. Every caller has a working answer for "this is not a git checkout".
fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve the version string embedded at build time. Strips a leading `v` so `v1.0.1` → `1.0.1`.
fn glass_version() -> String {
    // Release builds are a TAG push in CI, where `GITHUB_REF_TYPE=tag` and `GITHUB_REF_NAME` is the
    // tag (e.g. `v1.0.1`). Gate on the ref TYPE: `GITHUB_REF_NAME` is also set on branch/PR builds
    // (as the branch name), which must NOT become the version.
    if std::env::var("GITHUB_REF_TYPE").as_deref() == Ok("tag")
        && let Ok(tag) = std::env::var("GITHUB_REF_NAME")
    {
        let v = tag.strip_prefix('v').unwrap_or(&tag).trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    // Local builds: derive from the nearest tag (with a `-dirty` / commit suffix when not exactly
    // on a tag), so a dev binary reports an honest version rather than 0.0.0. `watch_git_head`
    // declares the files this reads, so the answer moves with the commit.
    if let Some(d) = git_output(&["describe", "--tags", "--always", "--dirty"])
        && !d.is_empty()
    {
        return d.strip_prefix('v').unwrap_or(&d).to_string();
    }
    // No tag and no git (e.g. a source tarball with no VCS): fall back to the crate version.
    env!("CARGO_PKG_VERSION").to_string()
}
