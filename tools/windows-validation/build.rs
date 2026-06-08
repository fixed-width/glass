//! Embed a Per-Monitor-V2 (+ long-path-aware) application manifest so the probe
//! process is DPI-aware before any startup code runs. The runtime
//! SetProcessDpiAwarenessContext call in main() loses to whatever sets awareness
//! first; a manifest is authoritative. This mirrors what the real glass-windows
//! backend will ship.
use embed_manifest::manifest::{DpiAwareness, Setting};
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_manifest(
            new_manifest("Glass.WindowsValidation")
                .dpi_awareness(DpiAwareness::PerMonitorV2)
                .long_path_aware(Setting::Enabled),
        )
        .expect("failed to embed application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
