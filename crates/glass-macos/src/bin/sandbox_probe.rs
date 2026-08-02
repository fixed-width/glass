//! Test-only fixture spawned by `process::tests`' `SandboxLevel::Default` tests (see
//! `process.rs`'s `sandbox_probe_path`) in place of `/bin/sh`: an Apple platform binary like
//! `/bin/sh` ships arm64e-only on Apple Silicon, so the arm64 clip shim those tests exercise
//! can't be `DYLD_INSERT_LIBRARIES`-injected into it (dyld aborts on the arch mismatch before
//! the process runs at all). Any Rust-compiled binary is plain arm64, so this one loads it
//! fine, letting the tests exercise the real injection path instead of dodging it.
//!
//! Two flags, repeatable, processed in argv order; a bad read never aborts the process (matches
//! the `cat ... && echo ... || echo ...` shell one-liner this replaces):
//! - `--print LINE` — write LINE verbatim.
//! - `--read-env VAR OK FAIL` — write OK if the path named by env var VAR can be read, else FAIL.
//!
//! `--read-env` takes an env var NAME, not the path itself: `glass-sandbox-macos::
//! launch_reallows` re-allows any `run[1..]` argv token that is itself an absolute, existing
//! path, on the assumption that a launched script needs to read a path passed as its own
//! argument. One of these tests' paths is the exact file the sandbox must be proving it
//! DENIES — putting it in argv would re-allow the very thing under test. Env vars aren't argv,
//! so `spec.env` can carry the real paths without tripping that logic.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--print" => {
                let line = args
                    .get(i + 1)
                    .unwrap_or_else(|| panic!("sandbox-probe: --print needs a LINE argument"));
                println!("{line}");
                i += 2;
            }
            "--read-env" => {
                let var = args
                    .get(i + 1)
                    .unwrap_or_else(|| panic!("sandbox-probe: --read-env needs a VAR argument"));
                let ok = args
                    .get(i + 2)
                    .unwrap_or_else(|| panic!("sandbox-probe: --read-env needs an OK marker"));
                let fail = args
                    .get(i + 3)
                    .unwrap_or_else(|| panic!("sandbox-probe: --read-env needs a FAIL marker"));
                let path = std::env::var(var)
                    .unwrap_or_else(|_| panic!("sandbox-probe: env var {var} is not set"));
                match std::fs::read(path) {
                    Ok(_) => println!("{ok}"),
                    Err(_) => println!("{fail}"),
                }
                i += 4;
            }
            other => panic!("sandbox-probe: unrecognized argument {other:?}"),
        }
    }
    let _ = std::io::stdout().flush();
}
