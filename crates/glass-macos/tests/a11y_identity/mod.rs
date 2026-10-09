#![cfg(target_os = "macos")]

use std::path::Path;
use std::time::Duration;

use glass_a11y_macos::MacosA11y;
use glass_core::{
    Accessibility, AppSpec, AxContext, AxNode, AxTarget, Deadline, GlassError, Platform,
    PointerHit, SandboxLevel, Stream, WalkLimits,
};
use glass_macos::MacosPlatform;

use crate::common::{try_expect, with_stop_app};

fn target(node: &AxNode, name: &str) -> Result<AxTarget, String> {
    if node.name.as_deref() == Some(name) {
        return Ok(AxTarget {
            id: node.id,
            role: node.role,
            name: node.name.clone(),
            bounds: node.bounds,
            value: node.value.clone(),
        });
    }
    node.children
        .iter()
        .find_map(|child| target(child, name).ok())
        .ok_or_else(|| format!("fixture has no {name:?} target"))
}

pub fn run(platform: &mut MacosPlatform, fixture: &Path) -> Result<(), String> {
    with_stop_app(platform, "identity fixture", |platform| {
        let window = try_expect(
            platform.start_app(&AppSpec {
                build: None,
                run: vec![
                    fixture.to_string_lossy().into_owned(),
                    "--identity-drift".into(),
                ],
                cwd: None,
                env: vec![],
                window_hint: None,
                timeout_ms: 8_000,
                sandbox: SandboxLevel::Off,
                a11y: false,
            }),
            "start identity fixture",
        )?;
        std::thread::sleep(Duration::from_millis(400));
        let ctx = AxContext {
            pids: platform.app_pids(),
            window,
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: Deadline::UNBOUNDED,
        };
        let mut reader = MacosA11y::new();
        let mut before = try_expect(reader.snapshot(&ctx), "initial identity snapshot")?;
        before.assign_ids();
        let button = target(&before.root, "Drift target")?;
        let field = target(&before.root, "Drift field")?;
        let insert = target(&before.root, "Insert sibling")?;
        try_expect(reader.invoke(&ctx, &insert), "insert sibling")?;
        std::thread::sleep(Duration::from_millis(200));
        let mut shifted = try_expect(reader.snapshot(&ctx), "shifted identity snapshot")?;
        shifted.assign_ids();
        if target(&shifted.root, "Drift target")?.id == button.id
            || target(&shifted.root, "Drift field")?.id == field.id
        {
            return Err("fixture insertion did not shift both captured ids".into());
        }

        let mut unknown_bounds = button.clone();
        unknown_bounds.bounds = None;
        match reader.invoke(&ctx, &unknown_bounds) {
            Err(GlassError::AxElementChanged(_)) => {}
            result => {
                return Err(format!(
                    "unknown-bounds relocation did not refuse: {result:?}"
                ));
            }
        }
        let capped = AxContext {
            limits: WalkLimits {
                nodes: 2,
                ..ctx.limits
            },
            ..ctx.clone()
        };
        match reader.invoke(&capped, &button) {
            Err(GlassError::AxElementChanged(_)) => {}
            result => return Err(format!("incomplete relocation did not refuse: {result:?}")),
        }

        try_expect(
            reader.invoke(&ctx, &button),
            "invoke captured target after insertion",
        )?;
        try_expect(
            reader.focus(&ctx, &field),
            "focus captured field after insertion",
        )?;
        let bounds = field.bounds.ok_or("captured field has no bounds")?;
        let hit = try_expect(
            reader.pointer_target_at(
                &ctx,
                &field,
                (
                    bounds.x + bounds.width as i32 / 2,
                    bounds.y + bounds.height as i32 / 2,
                ),
            ),
            "probe captured field after insertion",
        )?;
        if hit != PointerHit::Target {
            return Err(format!("relocated field hit was not the target: {hit:?}"));
        }
        try_expect(
            reader.set_value(&ctx, &field, "relocated"),
            "write captured field",
        )?;
        let mut after = try_expect(reader.snapshot(&ctx), "identity effect snapshot")?;
        after.assign_ids();
        if target(&after.root, "Drift field")?.value.as_deref() != Some("relocated") {
            return Err("relocated write did not reach the field".into());
        }
        std::thread::sleep(Duration::from_millis(200));
        let clicks = platform
            .drain_logs()
            .iter()
            .filter(|(stream, line)| *stream == Stream::Stdout && line == "IDENTITY_TARGET_CLICKED")
            .count();
        if clicks != 1 {
            return Err(format!("expected one target effect, observed {clicks}"));
        }

        let captured = target(&after.root, "Drift target")?;
        let duplicate = target(&after.root, "Duplicate target")?;
        try_expect(reader.invoke(&ctx, &duplicate), "add ambiguous target")?;
        std::thread::sleep(Duration::from_millis(200));
        match reader.invoke(&ctx, &captured) {
            Err(GlassError::AxElementChanged(_)) => {}
            result => return Err(format!("ambiguous relocation did not refuse: {result:?}")),
        }
        std::thread::sleep(Duration::from_millis(200));
        if platform
            .drain_logs()
            .iter()
            .any(|(stream, line)| *stream == Stream::Stdout && line == "IDENTITY_TARGET_CLICKED")
        {
            return Err("ambiguous relocation actuated a target".into());
        }
        println!("A11Y_IDENTITY_DRIFT_PASS");
        Ok(())
    })
}
