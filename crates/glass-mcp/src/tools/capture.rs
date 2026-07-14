//! Capture, visual-diff, and log tools.

use glass_core::{frame_to_webp, Frame, Glass, Region, Stream};
use serde_json::json;

use crate::params::*;
use crate::tools::{OutContent, ToolOutput, ToolResult};

/// Crop the captured frame to the requested region, or return it whole.
fn crop_frame(frame: Frame, region: Option<&RegionArgs>) -> Result<Frame, String> {
    match region {
        Some(r) => frame.crop(&r.into()).map_err(|e| e.to_string()),
        None => Ok(frame),
    }
}

pub fn screenshot(glass: &mut Glass, a: &ScreenshotArgs) -> ToolResult {
    let frame = glass
        .screenshot(
            a.region.as_ref().map(|r| r.into()),
            a.window_id.map(glass_core::WindowId),
        )
        .map_err(|e| e.to_string())?;
    let img = frame_to_webp(&frame).map_err(|e| e.to_string())?;
    let mut meta = json!({ "width": frame.width, "height": frame.height });
    if let Some(r) = a.region.as_ref() {
        meta["x"] = json!(r.x);
        meta["y"] = json!(r.y);
    }
    Ok(ToolOutput::image_result(
        "glass_screenshot",
        Some(img),
        meta,
        vec![],
    ))
}

pub fn wait_stable(glass: &mut Glass, a: &WaitStableArgs) -> ToolResult {
    let params = glass_core::WaitStableParams {
        interval_ms: a.interval_ms.unwrap_or(100),
        settle_frames: a.settle_frames.unwrap_or(3),
        tolerance: a.tolerance.unwrap_or(0),
        timeout_ms: a.timeout_ms.unwrap_or(5000),
        stability_region: a.stability_region.as_ref().map(|r| r.into()),
        window: a.window_id.map(glass_core::WindowId),
    };
    let outcome = glass.wait_stable(&params).map_err(|e| e.to_string())?;
    let settled = outcome.settled;
    // `saw_motion`/`observed_ms` make `settled` non-opaque: settled with saw_motion:false
    // over a short observed_ms is only a brief quiet window (a slow animation can hide).
    let saw_motion = outcome.saw_motion;
    let observed_ms = outcome.observed_ms;

    // Text-only: report the settle status + full-frame dims, no WebP. `region`
    // (which only crops the returned image) is intentionally ignored here.
    if !a.include_image.unwrap_or(true) {
        let meta = json!({
            "settled": settled,
            "saw_motion": saw_motion,
            "observed_ms": observed_ms,
            "width": outcome.frame.width,
            "height": outcome.frame.height,
        });
        return Ok(ToolOutput::result("glass_wait_stable", meta));
    }

    let frame = crop_frame(outcome.frame, a.region.as_ref())?;
    let img = frame_to_webp(&frame).map_err(|e| e.to_string())?;
    let mut meta = json!({ "settled": settled, "saw_motion": saw_motion, "observed_ms": observed_ms, "width": frame.width, "height": frame.height });
    if let Some(r) = a.region.as_ref() {
        meta["x"] = json!(r.x);
        meta["y"] = json!(r.y);
    }
    Ok(ToolOutput::image_result(
        "glass_wait_stable",
        Some(img),
        meta,
        vec![],
    ))
}

pub fn baseline_save(glass: &mut Glass, a: &BaselineSaveArgs) -> ToolResult {
    glass.save_baseline(&a.name).map_err(|e| e.to_string())?;
    Ok(ToolOutput::result(
        "glass_baseline_save",
        json!({ "name": a.name }),
    ))
}

pub fn diff(glass: &mut Glass, a: &DiffArgs) -> ToolResult {
    let region = a.region.as_ref().map(Region::from);
    let (r, current) = match a.mode.as_deref().unwrap_or("perceptual") {
        "perceptual" => glass.diff_baseline_perceptual_with_frame(
            &a.name,
            region.as_ref(),
            a.threshold.unwrap_or(0.1),
        ),
        "exact" => {
            glass.diff_baseline_with_frame(&a.name, region.as_ref(), a.tolerance.unwrap_or(0))
        }
        other => {
            return Err(format!(
                "unknown diff mode '{other}' (use perceptual/exact)"
            ))
        }
    }
    .map_err(|e| e.to_string())?;

    let bbox = r
        .bbox
        .map(|b| json!({ "x": b.x, "y": b.y, "width": b.width, "height": b.height }));
    let mut body = json!({
        "changed_pixels": r.changed_pixels,
        "total_pixels": r.total_pixels,
        "changed_pct": r.changed_pct,
        "aa_ignored": r.aa_ignored,
        "bbox": bbox,
    });
    // Echo the region so the caller can map the region-relative bbox back to
    // window coordinates.
    if let Some(rr) = a.region.as_ref() {
        body["region"] = json!({ "x": rr.x, "y": rr.y, "width": rr.width, "height": rr.height });
    }

    // Opt-in: when something changed, attach the current frame cropped to the
    // changed region (token-minimal, exactly what differs). Nothing changed ->
    // no image.
    let mut image = None;
    if a.include_image.unwrap_or(false) {
        if let Some(b) = r.bbox {
            let region = Region {
                x: b.x,
                y: b.y,
                width: b.width,
                height: b.height,
            };
            let cropped = current.crop(&region).map_err(|e| e.to_string())?;
            image = Some(frame_to_webp(&cropped).map_err(|e| e.to_string())?);
        }
    }
    Ok(ToolOutput::image_result("glass_diff", image, body, vec![]))
}

pub fn logs(glass: &mut Glass, a: &LogsArgs) -> ToolResult {
    let stream = match a.stream.as_deref() {
        None | Some("both") => None,
        Some("stdout") => Some(Stream::Stdout),
        Some("stderr") => Some(Stream::Stderr),
        Some(other) => return Err(format!("unknown stream '{other}' (use stdout/stderr/both)")),
    };
    let (lines, cursor) = glass
        .logs(
            a.cursor.unwrap_or(0),
            a.max_lines.unwrap_or(200) as usize,
            stream,
            a.contains.as_deref(),
        )
        .map_err(|e| e.to_string())?;
    let json_lines: Vec<_> = lines
        .iter()
        .map(|l| {
            json!({
                "seq": l.seq,
                "stream": match l.stream { Stream::Stdout => "stdout", Stream::Stderr => "stderr" },
                "text": l.text,
            })
        })
        .collect();
    let body = json!({ "lines": json_lines }).to_string();
    Ok(ToolOutput::result_with(
        "glass_logs",
        json!({ "cursor": cursor }),
        vec![OutContent::Text(crate::untrusted::wrap_untrusted(&body))],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::start as start_tool;
    use crate::tools::testutil::*;
    use glass_core::Frame;

    fn started_with(platform: FakePlatform) -> Glass {
        let mut g = glass_with(platform);
        let a = StartArgs {
            build: None,
            run: vec!["app".into()],
            backend: None,
            sandbox: None,
            cwd: None,
            env: std::collections::BTreeMap::new(),
            window_hint: None,
            timeout_ms: None,
            a11y: None,
        };
        start_tool(&mut g, &a).unwrap();
        g
    }

    #[test]
    fn screenshot_returns_image_then_meta() {
        let frame = Frame::solid(4, 4, [1, 2, 3, 255]);
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![frame]));
        let out = screenshot(
            &mut g,
            &ScreenshotArgs {
                region: None,
                window_id: None,
            },
        )
        .unwrap();
        assert!(matches!(out.0[0], OutContent::Image(_)));
        match &out.0[1] {
            OutContent::Text(t) => assert!(t.contains("\"width\":4")),
            _ => panic!("expected meta text"),
        }
        if let OutContent::Image(bytes) = &out.0[0] {
            let decoded = glass_core::frame_from_webp(bytes).unwrap();
            assert_eq!((decoded.width, decoded.height), (4, 4));
        }
    }

    #[test]
    fn screenshot_with_region_returns_cropped_dims() {
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![Frame::solid(
            4,
            4,
            [5, 6, 7, 255],
        )]));
        let a = ScreenshotArgs {
            region: Some(RegionArgs {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            }),
            window_id: None,
        };
        let out = screenshot(&mut g, &a).unwrap();
        match &out.0[1] {
            OutContent::Text(t) => {
                assert!(t.contains("\"width\":2"));
                assert!(t.contains("\"height\":2"));
                assert!(t.contains("\"x\":1"));
            }
            _ => panic!("expected meta text"),
        }
        if let OutContent::Image(bytes) = &out.0[0] {
            let decoded = glass_core::frame_from_webp(bytes).unwrap();
            assert_eq!((decoded.width, decoded.height), (2, 2));
        } else {
            panic!("expected image content");
        }
    }

    #[test]
    fn screenshot_region_out_of_bounds_errors() {
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![Frame::solid(
            4,
            4,
            [0, 0, 0, 255],
        )]));
        let a = ScreenshotArgs {
            region: Some(RegionArgs {
                x: 0,
                y: 0,
                width: 99,
                height: 99,
            }),
            window_id: None,
        };
        assert!(screenshot(&mut g, &a).unwrap_err().contains("region"));
    }

    #[test]
    fn screenshot_with_window_id_routes_through_capture_window() {
        // testutil's FakePlatform doesn't override capture_window, so a window_id
        // must reach the backend's (Unsupported) capture_window rather than
        // silently falling back to the scripted capture_frame frame.
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![Frame::solid(
            4,
            4,
            [0, 0, 0, 255],
        )]));
        let a = ScreenshotArgs {
            region: None,
            window_id: Some(7),
        };
        let err = screenshot(&mut g, &a).unwrap_err();
        assert!(err.contains("not supported"), "got: {err}");
    }

    #[test]
    fn wait_stable_with_region_crops_returned_frame() {
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![Frame::solid(
            4,
            4,
            [0, 0, 0, 255],
        )]));
        let a = WaitStableArgs {
            interval_ms: Some(1),
            settle_frames: Some(1),
            tolerance: None,
            timeout_ms: Some(200),
            region: Some(RegionArgs {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            }),
            stability_region: None,
            include_image: None,
            window_id: None,
        };
        let out = wait_stable(&mut g, &a).unwrap();
        if let OutContent::Image(bytes) = &out.0[0] {
            let decoded = glass_core::frame_from_webp(bytes).unwrap();
            assert_eq!((decoded.width, decoded.height), (2, 2));
        } else {
            panic!("expected image content");
        }
    }

    #[test]
    fn wait_stable_out_of_bounds_stability_region_errors() {
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![Frame::solid(
            4,
            4,
            [0, 0, 0, 255],
        )]));
        let a = WaitStableArgs {
            interval_ms: Some(1),
            settle_frames: Some(1),
            tolerance: None,
            timeout_ms: Some(200),
            region: None,
            stability_region: Some(RegionArgs {
                x: 0,
                y: 0,
                width: 99,
                height: 1,
            }),
            include_image: None,
            window_id: None,
        };
        assert!(wait_stable(&mut g, &a).unwrap_err().contains("region"));
    }

    #[test]
    fn wait_stable_with_window_id_routes_through_capture_window() {
        // As above: testutil's FakePlatform has no capture_window override, so a
        // window_id must reach it (Unsupported) rather than silently polling the
        // active window's scripted capture_frame frames.
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![Frame::solid(
            4,
            4,
            [0, 0, 0, 255],
        )]));
        let a = WaitStableArgs {
            interval_ms: Some(1),
            settle_frames: Some(1),
            tolerance: None,
            timeout_ms: Some(200),
            region: None,
            stability_region: None,
            include_image: None,
            window_id: Some(3),
        };
        let err = wait_stable(&mut g, &a).unwrap_err();
        assert!(err.contains("not supported"), "got: {err}");
    }

    #[test]
    fn baseline_save_then_diff_reports_change() {
        let base = Frame::solid(2, 2, [0, 0, 0, 255]);
        let mut changed = base.clone();
        changed.pixels[0] = 255;
        let mut g = started_with(FakePlatform::new(2, 2).with_frames(vec![base, changed]));
        baseline_save(
            &mut g,
            &BaselineSaveArgs {
                name: "main".into(),
            },
        )
        .unwrap();
        let out = diff(
            &mut g,
            &DiffArgs {
                region: None,
                name: "main".into(),
                mode: None,
                threshold: None,
                tolerance: None,
                include_image: None,
            },
        )
        .unwrap();
        match &out.0[0] {
            OutContent::Text(t) => assert!(t.contains("\"changed_pixels\":1")),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn diff_with_region_scopes_and_echoes_region() {
        // Whole baseline; current differs only outside the region.
        let base = Frame::solid(4, 4, [0, 0, 0, 255]);
        let mut changed = base.clone();
        changed.pixels[(3 * 4 + 3) * 4] = 255; // pixel (3,3), outside (0,0,2,2)
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![base, changed]));
        baseline_save(&mut g, &BaselineSaveArgs { name: "m".into() }).unwrap();
        let out = diff(
            &mut g,
            &DiffArgs {
                region: Some(RegionArgs {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                }),
                name: "m".into(),
                mode: None,
                threshold: None,
                tolerance: None,
                include_image: None,
            },
        )
        .unwrap();
        match &out.0[0] {
            OutContent::Text(t) => {
                assert!(
                    t.contains("\"changed_pixels\":0"),
                    "region excludes change: {t}"
                );
                assert!(t.contains("\"region\":{"), "region echoed: {t}");
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn diff_exact_mode_and_unknown_mode() {
        let base = Frame::solid(2, 2, [0, 0, 0, 255]);
        let mut changed = base.clone();
        changed.pixels[0] = 255;
        let mut g = started_with(FakePlatform::new(2, 2).with_frames(vec![base, changed]));
        baseline_save(&mut g, &BaselineSaveArgs { name: "m".into() }).unwrap();
        // explicit exact mode still works
        let out = diff(
            &mut g,
            &DiffArgs {
                region: None,
                name: "m".into(),
                mode: Some("exact".into()),
                threshold: None,
                tolerance: Some(0),
                include_image: None,
            },
        )
        .unwrap();
        match &out.0[0] {
            OutContent::Text(t) => assert!(t.contains("\"changed_pixels\":1")),
            _ => panic!("expected text"),
        }
        // unknown mode is rejected (no silent fallback)
        let err = diff(
            &mut g,
            &DiffArgs {
                region: None,
                name: "m".into(),
                mode: Some("fuzzy".into()),
                threshold: None,
                tolerance: None,
                include_image: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("unknown diff mode"), "got: {err}");
    }

    #[test]
    fn diff_missing_baseline_errors() {
        let mut g = started_with(FakePlatform::new(2, 2).with_frames(vec![Frame::solid(
            2,
            2,
            [0, 0, 0, 255],
        )]));
        let err = diff(
            &mut g,
            &DiffArgs {
                region: None,
                name: "absent".into(),
                mode: None,
                threshold: None,
                tolerance: None,
                include_image: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("baseline not found"));
    }

    #[test]
    fn logs_returns_json_lines() {
        let platform = FakePlatform::new(10, 10).with_logs(vec![(Stream::Stdout, "ready")]);
        let mut g = started_with(platform);
        let out = logs(
            &mut g,
            &LogsArgs {
                cursor: None,
                max_lines: None,
                stream: None,
                contains: None,
            },
        )
        .unwrap();
        // envelope leads, carrying the cursor
        match &out.0[0] {
            OutContent::Text(t) => {
                let v: serde_json::Value =
                    serde_json::from_str(t).expect("envelope must be valid JSON");
                assert_eq!(v["ok"], json!(true), "envelope: {v}");
                assert_eq!(v["tool"], json!("glass_logs"), "envelope: {v}");
                assert_eq!(v["result"]["cursor"], json!(1), "envelope: {v}");
            }
            _ => panic!("expected envelope text as first item"),
        }
        // untrusted lines sibling follows, carrying the app-controlled lines
        match &out.0[1] {
            OutContent::Text(t) => {
                assert!(
                    t.starts_with(crate::untrusted::NOTE),
                    "must be marked untrusted: {t}"
                );
                assert!(
                    t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"),
                    "enveloped: {t}"
                );
                assert!(t.contains("\"text\":\"ready\""));
            }
            _ => panic!("expected untrusted lines text as second item"),
        }
    }

    #[test]
    fn logs_rejects_bad_stream() {
        let mut g = started_with(FakePlatform::new(10, 10));
        let a = LogsArgs {
            cursor: None,
            max_lines: None,
            stream: Some("weird".into()),
            contains: None,
        };
        assert!(logs(&mut g, &a).unwrap_err().contains("unknown stream"));
    }

    #[test]
    fn wait_stable_text_only_omits_image() {
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![Frame::solid(
            4,
            4,
            [0, 0, 0, 255],
        )]));
        let a = WaitStableArgs {
            interval_ms: Some(1),
            settle_frames: Some(1),
            tolerance: None,
            timeout_ms: Some(200),
            region: None,
            stability_region: None,
            include_image: Some(false),
            window_id: None,
        };
        let out = wait_stable(&mut g, &a).unwrap();
        assert_eq!(
            out.0.len(),
            1,
            "text-only should emit a single content item"
        );
        match &out.0[0] {
            OutContent::Text(t) => {
                assert!(t.contains("\"settled\":true"), "got: {t}");
                assert!(
                    t.contains("\"width\":4") && t.contains("\"height\":4"),
                    "got: {t}"
                );
            }
            _ => panic!("expected text-only, got an image"),
        }
    }

    #[test]
    fn diff_with_image_returns_bbox_crop() {
        let base = Frame::solid(2, 2, [0, 0, 0, 255]);
        let mut changed = base.clone();
        changed.pixels[0] = 255; // change pixel (0,0) -> bbox is 1x1 at (0,0)
        let mut g = started_with(FakePlatform::new(2, 2).with_frames(vec![base, changed]));
        baseline_save(&mut g, &BaselineSaveArgs { name: "m".into() }).unwrap();
        let out = diff(
            &mut g,
            &DiffArgs {
                region: None,
                name: "m".into(),
                mode: None,
                threshold: None,
                tolerance: None,
                include_image: Some(true),
            },
        )
        .unwrap();
        assert_eq!(out.0.len(), 3, "expected [Image, Text, IMAGE_NOTE]");
        match &out.0[0] {
            OutContent::Image(bytes) => {
                let decoded = glass_core::frame_from_webp(bytes).unwrap();
                assert_eq!(
                    (decoded.width, decoded.height),
                    (1, 1),
                    "image is the bbox crop"
                );
            }
            _ => panic!("expected image first"),
        }
        match &out.0[1] {
            OutContent::Text(t) => assert!(t.contains("\"changed_pixels\":1"), "got: {t}"),
            _ => panic!("expected metrics text"),
        }
    }

    #[test]
    fn diff_with_image_unchanged_returns_text_only() {
        let base = Frame::solid(2, 2, [9, 9, 9, 255]);
        let mut g = started_with(FakePlatform::new(2, 2).with_frames(vec![base.clone(), base]));
        baseline_save(&mut g, &BaselineSaveArgs { name: "m".into() }).unwrap();
        let out = diff(
            &mut g,
            &DiffArgs {
                region: None,
                name: "m".into(),
                mode: None,
                threshold: None,
                tolerance: None,
                include_image: Some(true),
            },
        )
        .unwrap();
        assert_eq!(out.0.len(), 1, "no change -> no image");
        assert!(matches!(out.0[0], OutContent::Text(_)));
    }

    #[test]
    fn wait_stable_image_has_note_and_meta_unmarked() {
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![Frame::solid(
            4,
            4,
            [0, 0, 0, 255],
        )]));
        let a = WaitStableArgs {
            interval_ms: Some(1),
            settle_frames: Some(1),
            tolerance: None,
            timeout_ms: Some(200),
            region: None,
            stability_region: None,
            include_image: Some(true),
            window_id: None,
        };
        let out = wait_stable(&mut g, &a).unwrap();
        // must have [Image, meta-Text, IMAGE_NOTE-Text]
        assert!(
            out.0.len() >= 3,
            "expected [Image, meta, IMAGE_NOTE], got {} items",
            out.0.len()
        );
        assert!(
            matches!(out.0[0], OutContent::Image(_)),
            "first item must be Image"
        );
        // IMAGE_NOTE is present
        let has_note = out
            .0
            .iter()
            .any(|c| matches!(c, OutContent::Text(t) if t == crate::untrusted::IMAGE_NOTE));
        assert!(
            has_note,
            "IMAGE_NOTE must be present when include_image=true"
        );
        // meta text contains settled/width/height and is NOT enveloped
        let meta_enveloped = out.0.iter().any(|c| {
            matches!(c, OutContent::Text(t) if t.contains("\"settled\"") && t.contains("⟦untrusted:"))
        });
        assert!(!meta_enveloped, "settled-metadata must NOT be enveloped");
        // meta text contains expected fields
        let has_meta = out.0.iter().any(|c| {
            matches!(c, OutContent::Text(t) if t.contains("\"settled\"") && t.contains("\"width\"") && t.contains("\"height\""))
        });
        assert!(has_meta, "settled-metadata text must be present");
    }

    #[test]
    fn wait_stable_text_only_has_no_image_note() {
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![Frame::solid(
            4,
            4,
            [0, 0, 0, 255],
        )]));
        let a = WaitStableArgs {
            interval_ms: Some(1),
            settle_frames: Some(1),
            tolerance: None,
            timeout_ms: Some(200),
            region: None,
            stability_region: None,
            include_image: Some(false),
            window_id: None,
        };
        let out = wait_stable(&mut g, &a).unwrap();
        // no image -> no IMAGE_NOTE
        let has_note = out
            .0
            .iter()
            .any(|c| matches!(c, OutContent::Text(t) if t == crate::untrusted::IMAGE_NOTE));
        assert!(
            !has_note,
            "IMAGE_NOTE must NOT appear in text-only (include_image=false) result"
        );
        // no envelope markers
        let has_envelope = out
            .0
            .iter()
            .any(|c| matches!(c, OutContent::Text(t) if t.contains("⟦untrusted:")));
        assert!(!has_envelope, "no envelope markers in text-only result");
    }

    // ── untrusted-marking tests ────────────────────────────────────────────

    #[test]
    fn screenshot_image_note_present_and_meta_unmarked() {
        let frame = Frame::solid(4, 4, [1, 2, 3, 255]);
        let mut g = started_with(FakePlatform::new(4, 4).with_frames(vec![frame]));
        let out = screenshot(
            &mut g,
            &ScreenshotArgs {
                region: None,
                window_id: None,
            },
        )
        .unwrap();
        // must have at least 3 items: Image, meta-Text, note-Text
        assert!(
            out.0.len() >= 3,
            "expected [Image, meta, IMAGE_NOTE], got {} items",
            out.0.len()
        );
        // third item is the IMAGE_NOTE
        match &out.0[2] {
            OutContent::Text(t) => assert_eq!(
                t,
                crate::untrusted::IMAGE_NOTE,
                "third item must be IMAGE_NOTE"
            ),
            _ => panic!("expected IMAGE_NOTE text as third item"),
        }
        // meta (second item) must NOT be enveloped
        match &out.0[1] {
            OutContent::Text(t) => {
                assert!(t.contains("\"width\":4"), "meta must contain width");
                assert!(
                    !t.contains("⟦untrusted:"),
                    "meta must NOT be enveloped: {t}"
                );
            }
            _ => panic!("expected meta text as second item"),
        }
    }

    #[test]
    fn diff_with_image_has_note_and_metrics_unmarked() {
        let base = Frame::solid(2, 2, [0, 0, 0, 255]);
        let mut changed = base.clone();
        changed.pixels[0] = 255;
        let mut g = started_with(FakePlatform::new(2, 2).with_frames(vec![base, changed]));
        baseline_save(&mut g, &BaselineSaveArgs { name: "m".into() }).unwrap();
        let out = diff(
            &mut g,
            &DiffArgs {
                region: None,
                name: "m".into(),
                mode: None,
                threshold: None,
                tolerance: None,
                include_image: Some(true),
            },
        )
        .unwrap();
        // [Image, metrics-Text, IMAGE_NOTE-Text]
        assert!(
            out.0.len() >= 3,
            "expected [Image, metrics, IMAGE_NOTE], got {} items",
            out.0.len()
        );
        // IMAGE_NOTE is present
        let has_note = out
            .0
            .iter()
            .any(|c| matches!(c, OutContent::Text(t) if t == crate::untrusted::IMAGE_NOTE));
        assert!(
            has_note,
            "IMAGE_NOTE must be present when image is included"
        );
        // metrics text is NOT enveloped
        let metrics_enveloped = out.0.iter().any(|c| matches!(c, OutContent::Text(t) if t.contains("changed_pixels") && t.contains("⟦untrusted:")));
        assert!(!metrics_enveloped, "metrics text must NOT be enveloped");
    }

    #[test]
    fn diff_no_change_no_note_no_envelope() {
        let base = Frame::solid(2, 2, [9, 9, 9, 255]);
        let mut g = started_with(FakePlatform::new(2, 2).with_frames(vec![base.clone(), base]));
        baseline_save(&mut g, &BaselineSaveArgs { name: "m".into() }).unwrap();
        let out = diff(
            &mut g,
            &DiffArgs {
                region: None,
                name: "m".into(),
                mode: None,
                threshold: None,
                tolerance: None,
                include_image: Some(true),
            },
        )
        .unwrap();
        // no image -> no note
        let has_note = out
            .0
            .iter()
            .any(|c| matches!(c, OutContent::Text(t) if t == crate::untrusted::IMAGE_NOTE));
        assert!(!has_note, "no IMAGE_NOTE when nothing changed");
        // no envelope markers anywhere
        let has_envelope = out
            .0
            .iter()
            .any(|c| matches!(c, OutContent::Text(t) if t.contains("⟦untrusted:")));
        assert!(!has_envelope, "no envelope markers on metrics-only result");
    }
}
