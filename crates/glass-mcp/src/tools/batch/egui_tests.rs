use std::path::PathBuf;
use std::time::{Duration, Instant};

use glass_core::{AppSpec, AxNode, AxRole, Backend, BaselineStore, Glass, SandboxLevel};
use serde_json::{Value, json};

use crate::params::Action;
use crate::tools;

fn find(node: &AxNode, name: &str, role: AxRole) -> Option<(i32, i32)> {
    if node.name.as_deref() == Some(name) && node.role == role {
        let bounds = node.bounds?;
        Some((
            bounds.x + i32::try_from(bounds.width / 2).unwrap(),
            bounds.y + i32::try_from(bounds.height / 2).unwrap(),
        ))
    } else {
        node.children
            .iter()
            .find_map(|child| find(child, name, role))
    }
}

fn actions(glass: &mut Glass, input: Vec<Value>, batched: bool) {
    if batched {
        tools::do_actions(
            glass,
            &serde_json::from_value(json!({"actions": input})).unwrap(),
        )
        .unwrap();
    } else {
        for value in input {
            match serde_json::from_value::<Action>(value).unwrap() {
                Action::Click(args) => {
                    tools::click(glass, &args).unwrap();
                }
                Action::Key(args) => {
                    tools::key(glass, &args).unwrap();
                }
                Action::Type(args) => {
                    tools::type_text(glass, &args).unwrap();
                }
                _ => unreachable!(),
            }
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "needs private Xvfb/AT-SPI and a built glass-fixture-egui"]
fn consecutive_inputs_preserve_clicks_and_final_text_in_batches_and_standalone() {
    consecutive_inputs("x11", false);
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "needs headless sway/AT-SPI and a built glass-fixture-egui"]
fn wayland_consecutive_inputs_preserve_clicks_and_final_text() {
    consecutive_inputs("wayland", false);
    consecutive_inputs("wayland", true);
}

#[test]
#[cfg(windows)]
#[ignore = "needs an interactive Windows desktop and a built glass-fixture-egui"]
fn windows_consecutive_inputs_preserve_clicks_and_final_text() {
    // SAFETY: one-time DPI setup for this dedicated on-box test process.
    #[allow(unsafe_code)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    consecutive_inputs("windows", false);
}

#[cfg(target_os = "linux")]
struct PausedApp {
    pid: rustix::process::Pid,
    resume: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl PausedApp {
    fn new(pid_file: &std::path::Path, fixture: &std::path::Path) -> Self {
        use rustix::process::{Pid, Signal, kill_process};
        let raw: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let pid = Pid::from_raw(raw).unwrap();
        assert_eq!(
            std::fs::read_link(format!("/proc/{raw}/exe")).unwrap(),
            fixture.canonicalize().unwrap(),
            "only suspend the fixture launched by this test"
        );
        kill_process(pid, Signal::STOP).unwrap();
        let mut paused = Self { pid, resume: None };
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{raw}/stat")).unwrap();
            if stat.rsplit_once(')').unwrap().1.split_whitespace().next() == Some("T") {
                break;
            }
            assert!(Instant::now() < deadline, "fixture did not stop");
            std::thread::sleep(Duration::from_millis(1));
        }
        paused.resume = Some(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            kill_process(pid, Signal::CONT).unwrap();
        }));
        paused
    }
}

#[cfg(target_os = "linux")]
impl Drop for PausedApp {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process(self.pid, rustix::process::Signal::CONT);
        if let Some(resume) = self.resume.take() {
            let _ = resume.join();
        }
    }
}

fn consecutive_inputs(backend: &'static str, stall: bool) {
    let fixture = std::env::var_os("GLASS_EGUI_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../glass-fixture-egui/target/release")
                .join(format!(
                    "glass-fixture-egui{}",
                    std::env::consts::EXE_SUFFIX
                ))
        });
    assert!(fixture.is_file(), "build the egui fixture: {fixture:?}");
    let dir = tempfile::tempdir().unwrap();
    #[cfg(target_os = "linux")]
    let xvfb = (backend == "x11").then(|| glass_x11::Xvfb::start("1280x800x24").unwrap());
    #[cfg(target_os = "linux")]
    let display = xvfb.as_ref().map(|xvfb| xvfb.display.clone());
    let mut glass = Glass::new(
        Box::new(move |_| {
            #[cfg(target_os = "linux")]
            let backend = Backend {
                platform: match backend {
                    "x11" => Box::new(glass_x11::X11Platform::connect(display.as_deref())?),
                    "wayland" => Box::new(glass_wayland::WaylandPlatform::new()?),
                    _ => unreachable!(),
                },
                accessibility: Some(Box::new(glass_a11y_linux::LinuxA11y::new())),
            };
            #[cfg(windows)]
            let backend = Backend {
                platform: Box::new(glass_windows::WindowsPlatform::new()?),
                accessibility: Some(Box::new(glass_a11y_windows::WindowsA11y::new())),
            };
            Ok(backend)
        }),
        backend.into(),
        BaselineStore::new(dir.path().join("baselines")),
        1000,
    );
    let fixture_run = vec![fixture.to_string_lossy().into_owned()];
    #[cfg(target_os = "linux")]
    let pid_file = dir.path().join("fixture.pid");
    #[cfg(target_os = "linux")]
    let fixture_run = if stall {
        let launcher = dir.path().join("launch.sh");
        std::fs::write(&launcher, "echo $$ > \"$1\"\nexec \"$2\"\n").unwrap();
        vec![
            "sh".into(),
            launcher.to_string_lossy().into_owned(),
            pid_file.to_string_lossy().into_owned(),
            fixture.to_string_lossy().into_owned(),
        ]
    } else {
        fixture_run
    };
    glass
        .start(&AppSpec {
            build: None,
            run: fixture_run,
            cwd: None,
            env: if backend == "x11" {
                vec![("LIBGL_ALWAYS_SOFTWARE".into(), "1".into())]
            } else {
                vec![]
            },
            window_hint: None,
            timeout_ms: 10_000,
            sandbox: SandboxLevel::Off,
            a11y: true,
        })
        .unwrap();

    let expected = [
        ("Semantic Save", AxRole::Button),
        ("Restart movement", AxRole::Button),
        ("Text:", AxRole::TextField),
        ("Apply", AxRole::Button),
    ];
    let deadline = Instant::now() + Duration::from_secs(5);
    let centers = loop {
        if let Ok(tree) = glass.a11y_snapshot(None) {
            let centers: Option<Vec<_>> = expected
                .iter()
                .map(|(name, role)| find(&tree.root, name, *role))
                .collect();
            if let Some(centers) = centers {
                break centers;
            }
        }
        assert!(Instant::now() < deadline, "fixture controls unavailable");
        std::thread::sleep(Duration::from_millis(20));
    };
    let click =
        |index: usize| json!({"action": "click", "x": centers[index].0, "y": centers[index].1});
    let mut failures = Vec::new();
    for repetition in 0..4 {
        for batched in [false, true] {
            let (_, cursor) = glass.logs(0, 1000, None, None).unwrap();
            let text = format!("sequence-{repetition}-{batched}.json");
            #[cfg(target_os = "linux")]
            let paused = stall.then(|| PausedApp::new(&pid_file, &fixture));
            actions(
                &mut glass,
                vec![
                    click(0),
                    click(1),
                    click(2),
                    json!({"action": "key", "chord": "ctrl+a"}),
                    json!({"action": "type", "text": text}),
                    click(3),
                ],
                batched,
            );
            #[cfg(target_os = "linux")]
            drop(paused);
            let deadline = Instant::now() + Duration::from_secs(2);
            let applied = format!("[fixture] applied_text={text}");
            let lines = loop {
                let (lines, _) = glass.logs(cursor, 1000, None, None).unwrap();
                if lines
                    .iter()
                    .any(|line| line.text.starts_with("[fixture] applied_text="))
                    || Instant::now() >= deadline
                {
                    break lines.into_iter().map(|line| line.text).collect::<Vec<_>>();
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            for expected in [
                "[fixture] semantic_save",
                "[fixture] movement_restart",
                &applied,
            ] {
                if !lines.iter().any(|line| line == expected) {
                    let outcomes: Vec<_> = lines
                        .iter()
                        .filter(|line| !line.starts_with("[fixture] key "))
                        .collect();
                    failures.push(format!(
                        "backend={backend} stall={stall} batch={batched} repetition={repetition}: missing {expected}; {outcomes:?}"
                    ));
                }
            }
        }
    }
    glass.stop().unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
