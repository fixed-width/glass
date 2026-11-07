#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::process::Command;

use glass_core::{ProtectedHostPath, SandboxLevel};
use glass_sandbox_linux::{WrapOpts, wrap_argv};

#[test]
#[ignore = "requires Bubblewrap user namespaces"]
fn explicit_input_is_read_only_without_exposing_sibling_files() {
    let root = tempfile::tempdir().unwrap();
    let inputs = root.path().join("inputs");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&inputs).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let allowed = inputs.join("allowed.wav");
    let unrelated = inputs.join("unrelated.wav");
    std::fs::write(&allowed, "sample").unwrap();
    std::fs::write(&unrelated, "private").unwrap();
    let options = WrapOpts {
        level: SandboxLevel::Default,
        home: std::env::var_os("HOME").unwrap(),
        cwd: Some(workspace),
        read_only_inputs: vec![allowed.clone()],
        ro_binds: vec![],
        rw_binds: vec![],
        status_fd: None,
        protected_paths: vec![],
    };
    let script = r#"set -eu
test "$(cat "$1")" = sample
test ! -e "$2"
if (printf changed > "$1") 2>/dev/null; then exit 1; fi
"#;
    let argv = wrap_argv(
        OsStr::new("/bin/sh"),
        &[
            "-c".into(),
            script.into(),
            "sh".into(),
            allowed.clone().into_os_string(),
            unrelated.into_os_string(),
        ],
        &options,
    )
    .unwrap();
    let output = Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read_to_string(allowed).unwrap(), "sample");
}

#[test]
fn explicit_input_refuses_protected_paths_and_missing_files() {
    let root = tempfile::tempdir().unwrap();
    let protected = root.path().join("protected");
    std::fs::create_dir(&protected).unwrap();
    let forbidden = protected.join("evidence");
    std::fs::write(&forbidden, "private").unwrap();
    let masks = [ProtectedHostPath::directory(&protected)];
    for input in [
        root.path().to_path_buf(),
        forbidden,
        root.path().join("missing"),
    ] {
        assert!(glass_sandbox_linux::validate_read_only_inputs(&[input], &masks).is_err());
    }
}

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
            read_only_inputs: Vec::new(),
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
