//! On-box smoke for the Windows clipboard: set a sentinel via `set_clipboard`, read it back via
//! `get_clipboard`, and assert equality — a trivial global-storage round-trip on the real desktop.
//! The Win32 clipboard is global (no window/session needed), so this calls the Platform methods
//! directly. Windows-only; a no-op elsewhere so the Linux dev box stays green.
//!   cargo run -p glass-windows --example onbox_clipboard

fn main() {
    #[cfg(windows)]
    imp::run();
    #[cfg(not(windows))]
    eprintln!("glass-windows `onbox_clipboard` example is Windows-only; no-op on this host.");
}

#[cfg(windows)]
mod imp {
    use glass_core::Platform;
    use glass_windows::WindowsPlatform;

    pub fn run() {
        println!("== glass-windows on-box clipboard smoke ==");
        let mut p = match WindowsPlatform::new() {
            Ok(p) => p,
            Err(e) => {
                println!("FATAL WindowsPlatform::new: {e}");
                return;
            }
        };

        // Includes non-ASCII to exercise the UTF-16 round-trip.
        const SENTINEL: &str = "glass-clip-✓-é-世界";

        match p.set_clipboard(SENTINEL) {
            Ok(()) => println!("  set PASS"),
            Err(e) => {
                println!("  set FAIL {e}");
                return;
            }
        }
        match p.get_clipboard() {
            Ok(got) if got == SENTINEL => println!("  get PASS (round-trip exact: {got:?})"),
            Ok(got) => println!("  get FAIL (mismatch: got {got:?}, want {SENTINEL:?})"),
            Err(e) => println!("  get FAIL {e}"),
        }

        // Empty/overwrite sanity: set "" then get "".
        match p.set_clipboard("").and_then(|()| p.get_clipboard()) {
            Ok(got) if got.is_empty() => println!("  empty round-trip PASS"),
            Ok(got) => println!("  empty round-trip FAIL (got {got:?})"),
            Err(e) => println!("  empty round-trip FAIL {e}"),
        }
        println!("== done ==");
    }
}
