//! Test-only fixture spawned by `process::tests`' `SandboxLevel::Default` tests (see
//! `process.rs`'s `sandbox_probe_path`) in place of `/bin/sh`. `/bin/sh` is Apple platform
//! code, and dyld treats that specially in two ways observed directly on different macOS
//! versions: CI (macos-14) hard-aborts the child on an arm64/arm64e architecture mismatch when
//! it attempts to inject the arm64 clip shim into arm64e `/bin/sh`; a newer host (Darwin 25.5)
//! instead silently drops every `DYLD_*` env var for platform code before even attempting the
//! load, so injection never happens at all. A plain cargo-built binary sidesteps both: it isn't
//! restricted platform code, so `DYLD_INSERT_LIBRARIES` is honored, and it's built for whatever
//! architecture this cargo invocation targets, matching the shim by construction — not just
//! "arm64", so this also works on an Intel Mac.
//!
//! Two flags, repeatable, processed in argv order; a bad read never aborts the process (matches
//! the `cat ... && echo ... || echo ...` shell one-liner this replaces):
//! - `--print LINE` — write LINE verbatim.
//! - `--read-env VAR OK FAIL` — write OK if the path named by env var VAR can be read, else FAIL.
//! - `--protected-reads MARKER_VAR LEASE_VAR` — succeed only when both named files cannot be read.
//!
//! `--read-env` takes an env var NAME, not the path itself: `glass-sandbox-macos::
//! launch_reallows` re-allows any `run[1..]` argv token that is itself an absolute, existing
//! path, on the assumption that a launched script needs to read a path passed as its own
//! argument. One of these tests' paths is the exact file the sandbox must be proving it
//! DENIES — putting it in argv would re-allow the very thing under test. Env vars aren't argv,
//! so `spec.env` can carry the real paths without tripping that logic.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--protected-file" => {
                let path =
                    std::env::var_os(&args[i + 1]).expect("protected file environment variable");
                let path = std::path::Path::new(&path);
                assert!(std::fs::read(path).is_err(), "protected file was readable");
                assert!(
                    std::fs::OpenOptions::new().write(true).open(path).is_err(),
                    "protected file was writable"
                );
                assert!(
                    std::fs::remove_file(path).is_err(),
                    "protected file was removable"
                );
                assert!(
                    std::fs::rename(path, path.with_extension("moved")).is_err(),
                    "protected file was movable"
                );
                i += 2;
            }
            "--print" => {
                let line = args
                    .get(i + 1)
                    .expect("sandbox-probe: --print needs a LINE argument");
                println!("{line}");
                i += 2;
            }
            "--read-env" => {
                let var = args
                    .get(i + 1)
                    .expect("sandbox-probe: --read-env needs a VAR argument");
                let ok = args
                    .get(i + 2)
                    .expect("sandbox-probe: --read-env needs an OK marker");
                let fail = args
                    .get(i + 3)
                    .expect("sandbox-probe: --read-env needs a FAIL marker");
                let path = std::env::var(var)
                    .unwrap_or_else(|_| panic!("sandbox-probe: env var {var} is not set"));
                // `File::open` alone trips the same Seatbelt read check as a full read, without
                // pulling in a multi-megabyte file like `/usr/lib/dyld` just to discard it.
                if std::fs::File::open(path).is_ok() {
                    println!("{ok}");
                } else {
                    println!("{fail}");
                }
                i += 4;
            }
            "--protected-reads" => {
                let marker_var = args
                    .get(i + 1)
                    .expect("sandbox-probe: --protected-reads needs a MARKER_VAR argument");
                let lease_var = args
                    .get(i + 2)
                    .expect("sandbox-probe: --protected-reads needs a LEASE_VAR argument");
                let Some(marker) = std::env::var_os(marker_var) else {
                    eprintln!("sandbox-probe: env var {marker_var} is not set");
                    std::process::exit(2);
                };
                let Some(lease) = std::env::var_os(lease_var) else {
                    eprintln!("sandbox-probe: env var {lease_var} is not set");
                    std::process::exit(2);
                };
                if std::fs::read(marker).is_ok() || std::fs::read(lease).is_ok() {
                    std::process::exit(1);
                }
                i += 3;
            }
            other => panic!("sandbox-probe: unrecognized argument {other:?}"),
        }
    }
}
