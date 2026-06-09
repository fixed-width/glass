//! glass private-clipboard shim.
//!
//! Two faces: (1) an injected **DLL** (`cdylib`) loaded into the Sandboxie-boxed app via
//! `InjectDll64=`, which detours the user32 clipboard APIs and proxies them to a host store
//! over a named pipe; (2) a pure **library** (`rlib`) exposing the wire [`proto`]col and the
//! host-side [`store`], reused by `glass-windows`. Only [`hook`] is Win32 — the rest is pure
//! and unit-tested on the Linux dev box.

pub mod proto;
pub mod store;

#[cfg(windows)]
mod hook;

/// Sandboxie InjectDll entry point (called after SbieDll, before the app's entry). Inert unless
/// `GLASS_CLIP_PIPE` is set (only the target app's process tree carries it). See [`hook`].
#[cfg(windows)]
#[no_mangle]
pub extern "system" fn InjectDllMain(_h_sbie_dll: isize, _unused: usize) {
    // A panic unwinding across this `extern "system"` frame into the host app would be UB.
    // `catch_unwind` contains it (member-crate `panic = "abort"` is ignored by Cargo — only the
    // workspace-root `[profile]` is honored — so we cannot rely on it).
    let _ = std::panic::catch_unwind(hook::init);
}
