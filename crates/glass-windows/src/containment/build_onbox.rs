//! On-box (LOTUS) validation that the build step runs UNCONFINED even under Sandboxie
//! containment — only the launched *run* is the security boundary. Completes the Windows
//! follow-on of `2026-06-11-unsandbox-build-design`. `#[ignore]`d: needs Sandboxie. No window is
//! launched, so this runs over SSH (no interactive desktop required).

use glass_core::{AppSpec, SandboxLevel};

use super::imp::Containment;
use super::sandboxie::{available, sandboxie_dir, Sandboxie};

fn spec_with_build(build: String) -> AppSpec {
    AppSpec {
        build: Some(build),
        run: vec!["notepad.exe".into()], // unused by run_build (it never launches)
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 1,
        sandbox: SandboxLevel::Default,
        a11y: false,
    }
}

#[test]
#[ignore = "on-box: needs Sandboxie"]
fn build_runs_on_host_not_in_the_box() {
    let dir = sandboxie_dir();
    assert!(available(&dir), "Sandboxie not available at {dir}");

    // A host path the build writes to. If the build runs CONTAINED, Sandboxie redirects the write
    // into the box's copy-on-write store and this real path stays absent; if it runs UNCONFINED
    // (correct — only the run is contained), the file lands on the real filesystem.
    let marker =
        std::env::temp_dir().join(format!("glass_build_marker_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);

    let sb = Sandboxie::new(
        dir.clone(),
        format!("glass_buildtest_{}", std::process::id()),
    );
    sb.configure(SandboxLevel::Default).expect("configure box");
    let containment = Containment::Sandboxie(sb);

    // No quotes around the path (temp dir is space-free) — quoting it forces Rust's Command to
    // backslash-escape the quotes through `cmd /C`, which mangles the redirect target.
    let spec = spec_with_build(format!("echo built>{}", marker.display()));
    containment.run_build(&spec).expect("run_build");

    let landed = marker.exists();
    let _ = std::fs::remove_file(&marker);
    assert!(
        landed,
        "build must run on the HOST (marker on the real FS at {}), not inside the Sandboxie box",
        marker.display()
    );
}
