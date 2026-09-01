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
}

#[derive(Debug, PartialEq, Eq)]
struct ProbeResult {
    marker_read: AccessResult,
    lease_write: AccessResult,
}

struct ArtifactOnboxFixture {
    root: PathBuf,
    process_dir: PathBuf,
    lease_path: PathBuf,
    marker: PathBuf,
    marker_text: String,
    lease: Option<File>,
    box_prefix: String,
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
            box_prefix: format!("glass_artifact_{nonce}"),
        }
    }

    fn protected_paths(&self) -> Vec<ProtectedHostPath> {
        vec![
            ProtectedHostPath::directory(self.process_dir.clone()),
            ProtectedHostPath::file(self.lease_path.clone()),
        ]
    }

    fn run_probe_in_sandboxie(&self, level: SandboxLevel) -> ProbeResult {
        let dir = sandboxie_dir();
        assert!(available(&dir), "Sandboxie not available at {dir}");
        let box_name = format!("{}_{level}", self.box_prefix);
        let sandboxie = Sandboxie::new(dir, box_name);
        sandboxie
            .configure_with_paths(level, &self.protected_paths())
            .expect("configure artifact closed paths");

        let marker = path_string(&self.marker);
        let lease = path_string(&self.lease_path);
        let script = "$ErrorActionPreference='Stop'; try { $v=[IO.File]::ReadAllText($env:ARTIFACT_MARKER); Write-Output ('MARKER_SUCCESS:'+ $v) } catch { Write-Output 'MARKER_DENIED' }; try { [IO.File]::AppendAllText($env:ARTIFACT_LEASE,'tamper'); Write-Output 'LEASE_SUCCESS' } catch { Write-Output 'LEASE_DENIED' }".to_string();
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
        let mut app = sandboxie.launch(&spec, logs.clone()).expect("launch probe");
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if app.try_wait().expect("query probe").is_some() {
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
        ProbeResult {
            marker_read: if text.contains("MARKER_DENIED") && !text.contains("MARKER_SUCCESS") {
                AccessResult::Denied
            } else {
                AccessResult::UnexpectedSuccess
            },
            lease_write: if text.contains("LEASE_DENIED") && !text.contains("LEASE_SUCCESS") {
                AccessResult::Denied
            } else {
                AccessResult::UnexpectedSuccess
            },
        }
    }

    fn lease_is_still_exclusively_locked(&self) -> bool {
        let _keep_alive = self.lease.as_ref().expect("lease handle retained");
        OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&self.lease_path)
            .is_err()
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
        let result = fixture.run_probe_in_sandboxie(level);
        assert_eq!(result.marker_read, AccessResult::Denied);
        assert_eq!(result.lease_write, AccessResult::Denied);
    }
    assert_eq!(
        std::fs::read_to_string(&fixture.marker).expect("host reads marker"),
        fixture.marker_text
    );
    assert!(fixture.lease_is_still_exclusively_locked());
}
