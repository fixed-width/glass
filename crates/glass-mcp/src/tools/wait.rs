//! Wait-for-condition tools: block until a precise predicate holds, return
//! text-only JSON (region can opt into an image). Mirrors the capture-tool style.

use glass_core::{
    frame_to_webp, AxRole, ElementCondition, Glass, RegionUntil, Stream, WaitElementParams,
    WaitLogParams, WaitRegionParams,
};
use serde_json::json;

use crate::params::*;
use crate::tools::{OutContent, ToolOutput, ToolResult};

pub fn wait_for_element(glass: &mut Glass, a: &WaitForElementArgs) -> ToolResult {
    if a.name.is_none() && a.role.is_none() {
        return Err("specify `name` and/or `role` to select an element".into());
    }
    let role = match a.role.as_deref() {
        Some(r) => Some(AxRole::from_name(r).ok_or_else(|| format!("unknown role '{r}'"))?),
        None => None,
    };
    let condition = match a.condition.as_deref() {
        None => ElementCondition::Appears,
        Some(c) => ElementCondition::from_name(c)
            .ok_or_else(|| format!("unknown condition '{c}' (appears/disappears/enabled/disabled/checked/unchecked/selected/unselected/expanded/collapsed/focused/visible/hidden)"))?,
    };
    let params = WaitElementParams {
        name: a.name.clone(),
        role,
        value_contains: a.value_contains.clone(),
        condition,
        interval_ms: a.interval_ms.unwrap_or(200),
        timeout_ms: a.timeout_ms.unwrap_or(10_000),
    };
    let o = glass.wait_for_element(&params).map_err(|e| e.to_string())?;
    let element = o.element.map(|e| {
        json!({
            "id": e.id.0,
            "role": format!("{:?}", e.role),
            "name": e.name,
            "value": e.value,
            "bounds": e.bounds.map(|b| json!({ "x": b.x, "y": b.y, "width": b.width, "height": b.height })),
            "states": e.states.active(),
        })
    });
    let body = json!({ "matched": o.matched, "elapsed_ms": o.elapsed_ms, "element": element }).to_string();
    Ok(ToolOutput::text(crate::untrusted::wrap_untrusted(&body)))
}

pub fn wait_for_region(glass: &mut Glass, a: &WaitForRegionArgs) -> ToolResult {
    let until = match a.until.as_deref() {
        None | Some("changes") => RegionUntil::Changes,
        Some("matches") => RegionUntil::Matches,
        Some(o) => return Err(format!("unknown until '{o}' (use changes/matches)")),
    };
    let perceptual = match a.mode.as_deref() {
        None | Some("perceptual") => true,
        Some("exact") => false,
        Some(o) => return Err(format!("unknown mode '{o}' (use perceptual/exact)")),
    };
    if until == RegionUntil::Matches && a.baseline.is_none() {
        return Err("until=\"matches\" requires a `baseline` to converge to".into());
    }
    let params = WaitRegionParams {
        baseline: a.baseline.clone(),
        region: a.region.as_ref().map(|r| r.into()),
        until,
        perceptual,
        threshold: a.threshold.unwrap_or(0.1),
        tolerance: a.tolerance.unwrap_or(0),
        interval_ms: a.interval_ms.unwrap_or(100),
        timeout_ms: a.timeout_ms.unwrap_or(10_000),
    };
    let o = glass.wait_for_region(&params).map_err(|e| e.to_string())?;
    let bbox = o
        .bbox
        .map(|b| json!({ "x": b.x, "y": b.y, "width": b.width, "height": b.height }));
    let meta = json!({
        "matched": o.matched,
        "changed_pct": o.changed_pct,
        "bbox": bbox,
        "elapsed_ms": o.elapsed_ms,
    });
    let mut out = Vec::new();
    let mut image_produced = false;
    if o.matched && a.include_image.unwrap_or(false) {
        out.push(OutContent::Image(frame_to_webp(&o.frame).map_err(|e| e.to_string())?));
        image_produced = true;
    }
    out.push(OutContent::Text(meta.to_string()));
    if image_produced {
        out.push(OutContent::Text(crate::untrusted::IMAGE_NOTE.to_string()));
    }
    Ok(ToolOutput(out))
}

pub fn wait_for_log(glass: &mut Glass, a: &WaitForLogArgs) -> ToolResult {
    if a.contains.trim().is_empty() {
        return Err("`contains` must be a non-empty substring".into());
    }
    let stream = match a.stream.as_deref() {
        None | Some("both") => None,
        Some("stdout") => Some(Stream::Stdout),
        Some("stderr") => Some(Stream::Stderr),
        Some(o) => return Err(format!("unknown stream '{o}' (use stdout/stderr/both)")),
    };
    let params = WaitLogParams {
        contains: a.contains.clone(),
        stream,
        cursor: a.cursor,
        interval_ms: a.interval_ms.unwrap_or(100),
        timeout_ms: a.timeout_ms.unwrap_or(10_000),
    };
    let o = glass.wait_for_log(&params).map_err(|e| e.to_string())?;
    let line = o.line.map(|l| {
        json!({
            "seq": l.seq,
            "stream": match l.stream { Stream::Stdout => "stdout", Stream::Stderr => "stderr" },
            "text": l.text,
        })
    });
    let body = json!({ "matched": o.matched, "line": line, "cursor": o.cursor, "elapsed_ms": o.elapsed_ms })
        .to_string();
    Ok(ToolOutput::text(crate::untrusted::wrap_untrusted(&body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::*;
    use glass_core::AppSpec;

    fn started_a11y() -> Glass {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: glass_core::SandboxLevel::Off,
        })
        .unwrap();
        g
    }

    fn elem_args() -> WaitForElementArgs {
        WaitForElementArgs {
            name: None,
            role: None,
            condition: None,
            value_contains: None,
            interval_ms: Some(0),
            timeout_ms: Some(1000),
        }
    }

    #[test]
    fn element_requires_a_selector() {
        let mut g = started_a11y();
        let err = wait_for_element(&mut g, &elem_args()).unwrap_err();
        assert!(err.contains("name") && err.contains("role"), "got: {err}");
    }

    #[test]
    fn element_rejects_unknown_role_and_condition() {
        let mut g = started_a11y();
        let mut a = elem_args();
        a.role = Some("notarole".into());
        assert!(wait_for_element(&mut g, &a).unwrap_err().contains("unknown role"));

        let mut b = elem_args();
        b.name = Some("Save".into());
        b.condition = Some("nope".into());
        assert!(wait_for_element(&mut g, &b).unwrap_err().contains("unknown condition"));
    }

    #[test]
    fn element_match_returns_json_with_id() {
        // testutil fake_tree's Button "Save" is enabled+focusable.
        let mut g = started_a11y();
        let mut a = elem_args();
        a.role = Some("Button".into());
        a.condition = Some("enabled".into());
        let out = wait_for_element(&mut g, &a).unwrap();
        match &out.0[0] {
            OutContent::Text(t) => {
                assert!(t.starts_with(crate::untrusted::NOTE), "must be marked untrusted: {t}");
                assert!(t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"), "enveloped: {t}");
                assert!(t.contains("\"matched\":true"), "got: {t}");
                assert!(t.contains("\"id\":1"), "got: {t}");
                assert!(t.contains("\"name\":\"Save\""), "got: {t}");
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn element_timeout_is_soft_text() {
        let mut g = started_a11y();
        let mut a = elem_args();
        a.name = Some("Save".into());
        a.condition = Some("checked".into()); // never true
        a.timeout_ms = Some(0);
        let out = wait_for_element(&mut g, &a).unwrap();
        match &out.0[0] {
            OutContent::Text(t) => {
                assert!(t.starts_with(crate::untrusted::NOTE), "must be marked untrusted: {t}");
                assert!(t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"), "enveloped: {t}");
                assert!(t.contains("\"matched\":false"), "got: {t}");
                assert!(t.contains("\"element\":null"), "got: {t}");
            }
            _ => panic!("expected text"),
        }
    }

    use glass_core::Frame;

    fn started_frames(frames: Vec<Frame>) -> Glass {
        let mut g = glass_with(FakePlatform::new(2, 2).with_frames(frames));
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: glass_core::SandboxLevel::Off,
        })
        .unwrap();
        g
    }

    fn region_args() -> WaitForRegionArgs {
        WaitForRegionArgs {
            baseline: None,
            region: None,
            until: None,
            mode: Some("exact".into()),
            threshold: None,
            tolerance: Some(0),
            interval_ms: Some(0),
            timeout_ms: Some(1000),
            include_image: None,
        }
    }

    #[test]
    fn region_changes_matches_and_reports_pct() {
        let black = Frame::solid(2, 2, [0, 0, 0, 255]);
        let white = Frame::solid(2, 2, [255, 255, 255, 255]);
        let mut g = started_frames(vec![black, white]);
        let out = wait_for_region(&mut g, &region_args()).unwrap();
        assert_eq!(out.0.len(), 1, "no include_image -> text only");
        match out.0.last().unwrap() {
            OutContent::Text(t) => assert!(t.contains("\"matched\":true"), "got: {t}"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn region_matches_requires_baseline() {
        let mut g = started_frames(vec![Frame::solid(2, 2, [0, 0, 0, 255])]);
        let mut a = region_args();
        a.until = Some("matches".into());
        a.baseline = None;
        assert!(wait_for_region(&mut g, &a).unwrap_err().contains("baseline"));
    }

    #[test]
    fn region_rejects_unknown_until_and_mode() {
        let mut g = started_frames(vec![Frame::solid(2, 2, [0, 0, 0, 255])]);
        let mut a = region_args();
        a.until = Some("sideways".into());
        assert!(wait_for_region(&mut g, &a).unwrap_err().contains("unknown until"));
        let mut b = region_args();
        b.mode = Some("fuzzy".into());
        assert!(wait_for_region(&mut g, &b).unwrap_err().contains("unknown mode"));
    }

    fn started_logs(logs: Vec<(glass_core::Stream, &str)>) -> Glass {
        let mut g = glass_with(FakePlatform::new(10, 10).with_logs(logs));
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: glass_core::SandboxLevel::Off,
        })
        .unwrap();
        g
    }

    #[test]
    fn log_matches_from_cursor_zero() {
        use glass_core::Stream;
        let mut g = started_logs(vec![(Stream::Stdout, "build done")]);
        let a = WaitForLogArgs {
            contains: "done".into(),
            stream: None,
            cursor: Some(0),
            interval_ms: Some(0),
            timeout_ms: Some(1000),
        };
        let out = wait_for_log(&mut g, &a).unwrap();
        match &out.0[0] {
            OutContent::Text(t) => {
                assert!(t.starts_with(crate::untrusted::NOTE), "must be marked untrusted: {t}");
                assert!(t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"), "enveloped: {t}");
                assert!(t.contains("\"matched\":true"), "got: {t}");
                assert!(t.contains("\"text\":\"build done\""), "got: {t}");
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn log_rejects_empty_contains_and_bad_stream() {
        use glass_core::Stream;
        let mut g = started_logs(vec![(Stream::Stdout, "x")]);
        let empty = WaitForLogArgs {
            contains: "  ".into(),
            stream: None,
            cursor: None,
            interval_ms: Some(0),
            timeout_ms: Some(0),
        };
        assert!(wait_for_log(&mut g, &empty).unwrap_err().contains("non-empty"));
        let bad = WaitForLogArgs {
            contains: "x".into(),
            stream: Some("weird".into()),
            cursor: Some(0),
            interval_ms: Some(0),
            timeout_ms: Some(0),
        };
        assert!(wait_for_log(&mut g, &bad).unwrap_err().contains("unknown stream"));
    }

    #[test]
    fn log_timeout_is_soft_text() {
        use glass_core::Stream;
        let mut g = started_logs(vec![(Stream::Stdout, "old")]);
        let a = WaitForLogArgs {
            contains: "never".into(),
            stream: None,
            cursor: Some(0),
            interval_ms: Some(0),
            timeout_ms: Some(0),
        };
        let out = wait_for_log(&mut g, &a).unwrap();
        match &out.0[0] {
            OutContent::Text(t) => {
                assert!(t.starts_with(crate::untrusted::NOTE), "must be marked untrusted: {t}");
                assert!(t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"), "enveloped: {t}");
                assert!(t.contains("\"matched\":false"), "got: {t}");
                assert!(t.contains("\"line\":null"), "got: {t}");
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn region_include_image_returns_image_then_text_on_match() {
        let black = Frame::solid(2, 2, [0, 0, 0, 255]);
        let white = Frame::solid(2, 2, [255, 255, 255, 255]);
        let mut g = started_frames(vec![black, white]);
        let mut a = region_args();
        a.include_image = Some(true);
        let out = wait_for_region(&mut g, &a).unwrap();
        assert_eq!(out.0.len(), 3, "matched + include_image -> [Image, Text, IMAGE_NOTE]");
        assert!(matches!(out.0[0], OutContent::Image(_)), "image first");
        match &out.0[1] {
            OutContent::Text(t) => assert!(t.contains("\"matched\":true"), "got: {t}"),
            _ => panic!("expected text second"),
        }
    }

    // ── untrusted-marking tests ────────────────────────────────────────────

    #[test]
    fn region_include_image_has_image_note() {
        let black = Frame::solid(2, 2, [0, 0, 0, 255]);
        let white = Frame::solid(2, 2, [255, 255, 255, 255]);
        let mut g = started_frames(vec![black, white]);
        let mut a = region_args();
        a.include_image = Some(true);
        let out = wait_for_region(&mut g, &a).unwrap();
        // must have [Image, meta, IMAGE_NOTE]
        assert!(out.0.len() >= 3, "expected [Image, meta, IMAGE_NOTE], got {} items", out.0.len());
        let has_note = out.0.iter().any(|c| matches!(c, OutContent::Text(t) if t == crate::untrusted::IMAGE_NOTE));
        assert!(has_note, "IMAGE_NOTE must be present when image is returned");
        // scalar meta is NOT enveloped
        let meta_enveloped = out.0.iter().any(|c| matches!(c, OutContent::Text(t) if t.contains("matched") && t.contains("⟦untrusted:")));
        assert!(!meta_enveloped, "scalar meta must NOT be enveloped");
    }

    #[test]
    fn region_no_image_no_note() {
        let black = Frame::solid(2, 2, [0, 0, 0, 255]);
        let white = Frame::solid(2, 2, [255, 255, 255, 255]);
        let mut g = started_frames(vec![black, white]);
        // include_image defaults to None (false) -> no image -> no note
        let out = wait_for_region(&mut g, &region_args()).unwrap();
        let has_note = out.0.iter().any(|c| matches!(c, OutContent::Text(t) if t == crate::untrusted::IMAGE_NOTE));
        assert!(!has_note, "no IMAGE_NOTE when no image is produced");
        let has_envelope = out.0.iter().any(|c| matches!(c, OutContent::Text(t) if t.contains("⟦untrusted:")));
        assert!(!has_envelope, "no envelope on scalar-only result");
    }
}
