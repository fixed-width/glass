//! On-box (LOTUS) deterministic validation of the private-clipboard hook. `#[ignore]`d — it needs
//! Sandboxie running, the built `glass_clip_hook.dll` (path in `GLASS_CLIP_HOOK_DLL`), and the built
//! `clipprobe` example. No GUI / interactive desktop is needed: the probe only calls user32
//! clipboard APIs, so it runs over SSH. Run on the box with:
//! ```text
//!   set GLASS_CLIP_HOOK_DLL=C:\Users\mpd\glass\target\release\glass_clip_hook.dll
//!   cargo test -p glass-windows --release private_clipboard_isolation -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::logbuf::Stream;
use glass_core::{AppSpec, SandboxLevel};

use super::sandboxie::{available, sandboxie_dir, Sandboxie};

/// `<profile>` dir (release/debug) holding the built `glass_clip_hook.dll` and `examples/`.
/// `current_exe` = `<profile>/deps/glass_windows-HASH.exe`.
fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent()
        .and_then(|p| p.parent())
        .expect("profile dir")
        .to_path_buf()
}

/// Poll `sink` until a line contains `needle`, or `timeout` elapses.
fn wait_for_log(
    sink: &Arc<Mutex<Vec<(Stream, String)>>>,
    needle: &str,
    timeout: Duration,
) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(g) = sink.lock() {
            if let Some((_, line)) = g.iter().find(|(_, l)| l.contains(needle)) {
                return Some(line.clone());
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    None
}

#[test]
#[ignore = "on-box: needs Sandboxie + GLASS_CLIP_HOOK_DLL + the clipprobe example"]
fn private_clipboard_isolation() {
    let dir = sandboxie_dir();
    assert!(available(&dir), "Sandboxie not available at {dir}");

    let probe = profile_dir().join("examples").join("clipprobe.exe");
    assert!(
        probe.exists(),
        "build the probe first: cargo build -p glass-clip-hook --release --example clipprobe (looked at {})",
        probe.display()
    );
    let dll = std::env::var("GLASS_CLIP_HOOK_DLL")
        .expect("set GLASS_CLIP_HOOK_DLL to the built glass_clip_hook.dll");
    assert!(
        PathBuf::from(&dll).exists(),
        "GLASS_CLIP_HOOK_DLL points at a missing file: {dll}"
    );

    // A sentinel on the AMBIENT (real) clipboard of this session's window station — the boxed
    // probe must never disturb it.
    let sentinel = format!("SENTINEL-{}", std::process::id());
    crate::clipboard::set(&sentinel).expect("set ambient clipboard");

    let sb = Sandboxie {
        dir: dir.clone(),
        box_name: format!("glass_cliptest_{}", std::process::id()),
    };
    sb.configure(SandboxLevel::Default).expect("configure box");

    let spec = AppSpec {
        build: None,
        run: vec![
            probe.to_string_lossy().into_owned(),
            "roundtrip".into(),
            "FROM-BOX".into(),
        ],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 15000,
        sandbox: SandboxLevel::Default,
    };
    let sink: Arc<Mutex<Vec<(Stream, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let app = sb.launch(&spec, sink.clone()).expect("launch boxed probe");

    // The probe writes then reads CF_UNICODETEXT. The box also carries OpenClipboard=n, so the ONLY
    // way the probe can read back what it wrote is via the injected hook → a correct READBACK proves
    // interception, not the real clipboard.
    let line = wait_for_log(&sink, "READBACK=", Duration::from_secs(12));
    let store = app.private_clipboard();
    let ambient_after = crate::clipboard::get().unwrap_or_default();
    app.kill();

    let line = line.expect("probe produced no READBACK= line (hook not intercepting? check the log sink)");
    assert!(line.contains("READBACK=FROM-BOX"), "boxed read-back mismatch: {line:?}");

    let store = store.expect("Layer 2 inactive — GLASS_CLIP_HOOK_DLL not resolved / pipe server failed");
    assert_eq!(
        store.get().as_deref(),
        Some("FROM-BOX"),
        "host store did not see the boxed write"
    );

    assert_eq!(
        ambient_after, sentinel,
        "AMBIENT CLIPBOARD WAS TOUCHED — isolation breach"
    );
    println!("PASS: boxed clipboard roundtrip served by the private store; ambient clipboard untouched");
}
