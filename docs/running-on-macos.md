Running glass on a macOS host — *planned*.

← [Back to README](../README.md)

## Current status

glass-mcp does **not** build on macOS yet. `crates/glass-mcp/src/lib.rs` contains a
`#[cfg(not(any(target_os = "linux", windows)))] compile_error!(...)`, so `cargo build`
fails on macOS today.

macOS support — a native macOS backend plus a macOS build — is **planned** (arriving
when hardware is available for development and testing).

## Android from macOS

Once glass-mcp builds on macOS, the **Android** backend will work from a Mac too — it
is host-OS-agnostic and simply shells out to `adb`. No macOS-native display backend is
required for Android.

Check back here for setup instructions once the macOS build ships.
