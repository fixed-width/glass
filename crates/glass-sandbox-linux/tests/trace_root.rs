#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::process::Command;

use glass_core::{ProtectedHostPath, SandboxLevel};
use glass_sandbox_linux::{WrapOpts, wrap_argv};

#[test]
#[ignore = "requires Bubblewrap user and PID namespaces"]
fn protected_trace_root_denies_current_history_and_proc_aliases() {
    let root = tempfile::tempdir().unwrap();
    let traces = root.path().join("traces");
    let current = traces.join("current");
    let history = traces.join("retained");
    std::fs::create_dir_all(&current).unwrap();
    std::fs::create_dir(&history).unwrap();
    let files = [current.join("evidence"), history.join("evidence")];
    for file in &files {
        std::fs::write(file, "host evidence").unwrap();
    }
    let script = r#"
set -eu
for file in "$CURRENT" "$HISTORY"; do
  for alias in "$file" "/proc/self/root$file" "/proc/1/root$file"; do
    if cat "$alias" 2>/dev/null; then exit 1; fi
    if (printf tamper > "$alias") 2>/dev/null; then exit 2; fi
    if rm "$alias" 2>/dev/null; then exit 3; fi
  done
done
"#;
    for level in [SandboxLevel::Default, SandboxLevel::Strict] {
        let options = WrapOpts {
            level,
            home: std::env::var_os("HOME").unwrap(),
            cwd: Some(root.path().to_owned()),
            ro_binds: vec![],
            rw_binds: vec![root.path().to_owned()],
            status_fd: None,
            protected_paths: vec![ProtectedHostPath::directory(&traces)],
        };
        let argv = wrap_argv(
            OsStr::new("/bin/sh"),
            &["-c".into(), script.into()],
            &options,
        )
        .unwrap();
        let output = Command::new(&argv[0])
            .args(&argv[1..])
            .env("CURRENT", &files[0])
            .env("HISTORY", &files[1])
            .output()
            .unwrap();
        assert!(output.status.success(), "{level}: {output:?}");
    }
    for file in files {
        assert_eq!(std::fs::read_to_string(file).unwrap(), "host evidence");
    }
}
