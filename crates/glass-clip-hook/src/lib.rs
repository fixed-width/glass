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
