//! On-box validation for artifact paths protected by Sandboxie.

use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::logbuf::Stream;
use glass_core::platform::ProtectedHostPath;
use glass_core::{AppSpec, SandboxLevel};

use super::sandboxie::{Sandboxie, available, sandboxie_dir};

#[derive(Debug, PartialEq, Eq)]
enum AccessResult {
    Denied,
    UnexpectedSuccess,
    UnexpectedError(i32),
}

#[derive(Debug, PartialEq, Eq)]
struct ProbeResult {
    marker_read: AccessResult,
    marker_write: AccessResult,
    lease_write: AccessResult,
}

#[derive(Debug, PartialEq, Eq)]
enum ProbeParseError {
    Incomplete(String),
}

const ACCESS_DENIED_HRESULT: i32 = -2_147_024_891;
const SHARING_VIOLATION: i32 = 32;

fn parse_access(text: &str, label: &str) -> AccessResult {
    if text.lines().any(|line| line == format!("{label}_SUCCESS")) {
        return AccessResult::UnexpectedSuccess;
    }
    let prefix = format!("{label}_ERROR:");
    let code = text
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|value| value.parse::<i32>().ok());
    match code {
        Some(ACCESS_DENIED_HRESULT) => AccessResult::Denied,
        Some(code) => AccessResult::UnexpectedError(code),
        None => AccessResult::UnexpectedSuccess,
    }
}

fn parse_probe_logs(text: &str) -> Result<ProbeResult, ProbeParseError> {
    if !text.lines().any(|line| line == "PROBE_COMPLETE") {
        return Err(ProbeParseError::Incomplete(
            text.chars().take(1024).collect(),
        ));
    }
    Ok(ProbeResult {
        marker_read: parse_access(text, "MARKER"),
        marker_write: parse_access(text, "MARKER_WRITE"),
        lease_write: parse_access(text, "LEASE"),
    })
}

struct ArtifactOnboxFixture {
    root: PathBuf,
    process_dir: PathBuf,
    lease_path: PathBuf,
    marker: PathBuf,
    marker_text: String,
    lease: Option<File>,
}

impl ArtifactOnboxFixture {
    fn new() -> Self {
        let nonce = format!("{}_{}", std::process::id(), monotonic_nonce());
        let root = std::env::temp_dir().join(format!("glass_artifact_onbox_{nonce}"));
        let process_dir = root.join("server-owned");
        let lease_path = root.join("server-owned.lease");
        let marker = process_dir.join("marker.txt");
        std::fs::create_dir(&root).expect("create owned root");
        std::fs::create_dir(&process_dir).expect("create process directory");
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(&lease_path)
            .expect("create exclusive lease");
        let marker_text = format!("glass-artifact-marker-{nonce}");
        std::fs::write(&marker, &marker_text).expect("write marker");
        for path in [&process_dir, &lease_path, &marker] {
            crate::restrict_path_to_current_user(path).expect("apply private DACL");
        }
        Self {
            root,
            process_dir,
            lease_path,
            marker,
            marker_text,
            lease: Some(lease),
        }
    }

    fn protected_paths(&self) -> Vec<ProtectedHostPath> {
        vec![
            ProtectedHostPath::directory(self.process_dir.clone()),
            ProtectedHostPath::file(self.lease_path.clone()),
        ]
    }

    fn run_probe_in_sandboxie(&self, level: SandboxLevel) -> Result<ProbeResult, ProbeParseError> {
        let dir = sandboxie_dir();
        assert!(available(&dir), "Sandboxie not available at {dir}");
        let box_name = super::sandboxie::unique_box_name();
        let sandboxie = Sandboxie::new(dir, box_name);
        sandboxie
            .configure_with_paths(level, &self.protected_paths())
            .expect("configure artifact closed paths");

        let marker = path_string(&self.marker);
        let lease = path_string(&self.lease_path);
        let probe = "$ErrorActionPreference='Stop'; try { [void][IO.File]::ReadAllText($env:ARTIFACT_MARKER); Write-Output 'MARKER_SUCCESS' } catch { Write-Output ('MARKER_ERROR:'+ $_.Exception.GetBaseException().HResult) }; try { [IO.File]::AppendAllText($env:ARTIFACT_MARKER,'tamper'); Write-Output 'MARKER_WRITE_SUCCESS' } catch { Write-Output ('MARKER_WRITE_ERROR:'+ $_.Exception.GetBaseException().HResult) }; try { [IO.File]::AppendAllText($env:ARTIFACT_LEASE,'tamper'); Write-Output 'LEASE_SUCCESS' } catch { Write-Output ('LEASE_ERROR:'+ $_.Exception.GetBaseException().HResult) }; Write-Output 'PROBE_COMPLETE'";
        let script = format!(
            "$env:ARTIFACT_MARKER='{}'; $env:ARTIFACT_LEASE='{}'; {probe}",
            marker.replace('\'', "''"),
            lease.replace('\'', "''")
        );
        let spec = AppSpec {
            build: None,
            run: vec![
                "powershell.exe".into(),
                "-NoProfile".into(),
                "-Command".into(),
                script,
            ],
            cwd: None,
            env: vec![
                ("ARTIFACT_MARKER".into(), marker),
                ("ARTIFACT_LEASE".into(), lease),
            ],
            window_hint: None,
            timeout_ms: 15_000,
            sandbox: level,
            a11y: false,
        };
        let logs: Arc<Mutex<Vec<(Stream, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let app = sandboxie.launch(&spec, logs.clone()).expect("launch probe");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let complete = logs
                .lock()
                .is_ok_and(|lines| lines.iter().any(|(_, line)| line == "PROBE_COMPLETE"));
            if complete {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        app.kill();
        let text = logs
            .lock()
            .expect("probe log buffer")
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        parse_probe_logs(&text)
    }

    fn conflicting_lease_open_error(&self) -> i32 {
        let _keep_alive = self.lease.as_ref().expect("lease handle retained");
        OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&self.lease_path)
            .expect_err("exclusive lease must reject a conflicting host open")
            .raw_os_error()
            .expect("Windows sharing failure has an OS error")
    }
}

impl Drop for ArtifactOnboxFixture {
    fn drop(&mut self) {
        drop(self.lease.take());
        let _ = std::fs::remove_file(&self.marker);
        let _ = std::fs::remove_file(&self.lease_path);
        let _ = std::fs::remove_dir(&self.process_dir);
        let _ = std::fs::remove_dir(&self.root);
    }
}

fn path_string(path: &Path) -> String {
    path.as_os_str()
        .to_owned()
        .into_string()
        .expect("on-box artifact test path must be Unicode")
}

fn monotonic_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_nanos()
}

#[test]
#[ignore = "requires Windows Sandboxie Plus"]
fn sandboxie_denies_artifact_read_and_lease_tampering() {
    let fixture = ArtifactOnboxFixture::new();
    for path in [&fixture.process_dir, &fixture.lease_path, &fixture.marker] {
        assert!(
            crate::path_has_private_dacl(path).expect("inspect private DACL"),
            "owned artifact path lacks its private DACL"
        );
    }

    for level in [SandboxLevel::Default, SandboxLevel::Strict] {
        let result = fixture
            .run_probe_in_sandboxie(level)
            .expect("probe reached completion sentinel");
        assert_eq!(result.marker_read, AccessResult::Denied);
        assert_eq!(result.marker_write, AccessResult::Denied);
        assert_eq!(result.lease_write, AccessResult::Denied);
    }
    assert_eq!(
        std::fs::read_to_string(&fixture.marker).expect("host reads marker"),
        fixture.marker_text
    );
    assert_eq!(fixture.conflicting_lease_open_error(), SHARING_VIOLATION);
}

#[test]
#[ignore = "requires Windows Sandboxie Plus"]
fn protected_trace_root_denies_current_and_retained_history() {
    let mut fixture = ArtifactOnboxFixture::new();
    let current = fixture.marker.clone();
    let history = fixture.process_dir.join("retained");
    std::fs::create_dir(&history).unwrap();
    let old = history.join("old-evidence");
    std::fs::write(&old, "retained evidence").unwrap();
    for marker in [&current, &old] {
        fixture.marker = marker.clone();
        for level in [SandboxLevel::Default, SandboxLevel::Strict] {
            let result = fixture
                .run_probe_in_sandboxie(level)
                .expect("probe completed");
            assert_eq!(
                result.marker_read,
                AccessResult::Denied,
                "{level} {marker:?}"
            );
            assert_eq!(
                result.marker_write,
                AccessResult::Denied,
                "{level} {marker:?}"
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(&current).unwrap(),
        fixture.marker_text
    );
    assert_eq!(std::fs::read_to_string(&old).unwrap(), "retained evidence");
    std::fs::remove_file(old).unwrap();
    std::fs::remove_dir(history).unwrap();
    fixture.marker = current;
}

#[cfg(test)]
mod parser_tests {
    use super::{AccessResult, ProbeParseError, parse_probe_logs};

    #[test]
    fn access_denied_hresult_and_completion_are_required() {
        let result =
            parse_probe_logs("MARKER_ERROR:-2147024891\nLEASE_ERROR:-2147024891\nPROBE_COMPLETE")
                .unwrap();

        assert_eq!(result.marker_read, AccessResult::Denied);
        assert_eq!(result.lease_write, AccessResult::Denied);
    }

    #[test]
    fn sharing_violation_does_not_prove_sandboxie_lease_denial() {
        let result =
            parse_probe_logs("MARKER_ERROR:-2147024891\nLEASE_ERROR:-2147024864\nPROBE_COMPLETE")
                .unwrap();

        assert_eq!(
            result.lease_write,
            AccessResult::UnexpectedError(-2147024864)
        );
    }

    #[test]
    fn missing_completion_is_a_distinct_timeout() {
        assert_eq!(
            parse_probe_logs("MARKER_ERROR:-2147024891\nLEASE_ERROR:-2147024891").unwrap_err(),
            ProbeParseError::Incomplete("MARKER_ERROR:-2147024891\nLEASE_ERROR:-2147024891".into())
        );
    }
}
