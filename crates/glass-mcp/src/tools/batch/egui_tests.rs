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
#[ignore = "needs private Xvfb/AT-SPI and a built glass-fixture-egui"]
fn consecutive_inputs_preserve_clicks_and_final_text_in_batches_and_standalone() {
    let fixture = std::env::var_os("GLASS_EGUI_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../glass-fixture-egui/target/release/glass-fixture-egui")
        });
    assert!(fixture.is_file(), "build the egui fixture: {fixture:?}");
    let dir = tempfile::tempdir().unwrap();
    let xvfb = glass_x11::Xvfb::start("1280x800x24").unwrap();
    let display = xvfb.display.clone();
    let mut glass = Glass::new(
        Box::new(move |_| {
            Ok(Backend {
                platform: Box::new(glass_x11::X11Platform::connect(Some(&display))?),
                accessibility: Some(Box::new(glass_a11y_linux::LinuxA11y::new())),
            })
        }),
        "x11".into(),
        BaselineStore::new(dir.path().join("baselines")),
        1000,
    );
    glass
        .start(&AppSpec {
            build: None,
            run: vec![fixture.to_string_lossy().into_owned()],
            cwd: None,
            env: vec![("LIBGL_ALWAYS_SOFTWARE".into(), "1".into())],
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
                        "batch={batched} repetition={repetition}: missing {expected}; {outcomes:?}"
                    ));
                }
            }
        }
    }
    glass.stop().unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
