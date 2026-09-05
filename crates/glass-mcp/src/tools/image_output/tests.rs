use super::*;
use crate::params::*;
use crate::tools::{self, OutContent, ToolOutput, testutil::*};
use glass_core::{Glass, WindowId, frame_from_webp};
use proptest::prelude::*;
use std::sync::{Arc, Mutex};

fn started(platform: FakePlatform) -> Glass {
    let mut glass = glass_with(platform);
    tools::start(
        &mut glass,
        &serde_json::from_value(json!({"run":["app"]})).unwrap(),
    )
    .unwrap();
    glass
}

fn image(output: &ToolOutput) -> Frame {
    let bytes = output
        .0
        .iter()
        .find_map(|block| match block {
            OutContent::Image(bytes) => Some(bytes),
            _ => None,
        })
        .expect("image content");
    frame_from_webp(bytes).unwrap()
}

fn result(output: &ToolOutput) -> Value {
    output
        .0
        .iter()
        .find_map(|block| match block {
            OutContent::Envelope(envelope) => Some(envelope.result.clone()),
            _ => None,
        })
        .expect("result envelope")
}

#[test]
fn fitting_covers_portrait_rounding_single_pixels_and_extreme_dimensions() {
    for (w, h, mw, mh, expected) in [
        (3840, 2160, 1280, 0, (1280, 720)),
        (2160, 3840, 0, 1280, (720, 1280)),
        (1001, 667, 500, 0, (500, 333)),
        (667, 1001, 0, 500, (333, 500)),
        (12, 8, 8, 4, (6, 4)),
        (12, 8, 3, 4, (3, 2)),
        (1, 999, 5, 1, (1, 1)),
        (999, 1, 1, 5, (1, 1)),
        (4, 6, 4, 6, (4, 6)),
        (4, 6, 10, 20, (4, 6)),
        (4, 6, 0, 0, (4, 6)),
        (u32::MAX, u32::MAX, 1, 0, (1, 1)),
        (u32::MAX, 1, 0, u32::MAX, (u32::MAX, 1)),
        (
            u32::MAX,
            u32::MAX - 1,
            u32::MAX - 1,
            0,
            (u32::MAX - 1, u32::MAX - 2),
        ),
    ] {
        assert_eq!(
            fit_dimensions(w, h, NonZeroU32::new(mw), NonZeroU32::new(mh)).unwrap(),
            expected
        );
    }
}

proptest! {
    #[test]
    fn sampled_pixel_matches_the_center_mapping_over_the_full_dimension_range(
        source in 1..=u32::MAX, bound in 1..=u32::MAX, seed in any::<u32>()
    ) {
        let returned=bound.min(source);
        let position=seed%returned;
        let expected=((2*u128::from(position)+1)*u128::from(source))/(2*u128::from(returned));
        let sampled=source_pixel(position,source,returned);
        prop_assert_eq!(u128::from(sampled),expected);
        prop_assert!(sampled<source);
    }

    #[test]
    fn fitted_dimensions_never_enlarge_or_exceed_bounds(
        w in 1..=u32::MAX, h in 1..=u32::MAX, mw in 1..=u32::MAX, mh in 1..=u32::MAX
    ) {
        let (outw,outh) = fit_dimensions(w,h,NonZeroU32::new(mw),NonZeroU32::new(mh)).unwrap();
        prop_assert!(outw >= 1 && outw <= w.min(mw));
        prop_assert!(outh >= 1 && outh <= h.min(mh));
        prop_assert!(outw == w.min(mw) || outh == h.min(mh));
        let cross = (i128::from(outw)*i128::from(h)-i128::from(outh)*i128::from(w)).abs();
        prop_assert!(cross < i128::from(w.max(h)));
    }
}

#[test]
fn native_and_nonshrinking_requests_preserve_exact_encoding() {
    let frame = Frame::new(3, 2, (0..6).flat_map(|n| [n, 20, 30, n * 40]).collect()).unwrap();
    let original = frame_to_webp(&frame).unwrap();
    for (mw, mh) in [(0, 0), (3, 0), (0, 2), (4, 9), (u32::MAX, u32::MAX)] {
        let output = encode_image(
            frame.clone(),
            (7, 9),
            NonZeroU32::new(mw),
            NonZeroU32::new(mh),
        )
        .unwrap();
        assert_eq!(output.bytes, original);
        assert_eq!((output.width, output.height), (3, 2));
        if mw == 0 && mh == 0 {
            assert!(output.metadata.is_none());
        } else {
            assert_eq!(
                output.metadata.unwrap(),
                json!({
                    "source":{"x":7,"y":9,"width":3,"height":2},"width":3,"height":2,
                    "scale_x":1.0,"scale_y":1.0,"resized":false,"pixel_exact":true,"encoding":"lossless_webp"
                })
            );
        }
    }
}

#[test]
fn resized_webp_preserves_selected_rgba_and_reports_rounded_axis_ratios() {
    let frame = Frame::new(5, 3, (0..15).flat_map(|n| [n, 40, 200, n * 17]).collect()).unwrap();
    let output = encode_image(frame, (0, 0), NonZeroU32::new(2), None).unwrap();
    let decoded = frame_from_webp(&output.bytes).unwrap();
    assert_eq!(
        decoded,
        Frame::new(
            2,
            1,
            [6, 8]
                .into_iter()
                .flat_map(|n| [n, 40, 200, n * 17])
                .collect()
        )
        .unwrap()
    );
    let metadata = output.metadata.unwrap();
    assert_eq!(metadata["scale_x"], json!(0.4));
    assert_eq!(metadata["scale_y"], json!(1.0 / 3.0));
}

#[test]
fn large_frame_is_returned_with_the_requested_decoded_dimensions() {
    let output = encode_image(
        Frame::solid(3840, 2160, [10, 20, 30, 255]),
        (0, 0),
        NonZeroU32::new(1280),
        None,
    )
    .unwrap();
    assert_eq!(
        frame_from_webp(&output.bytes).unwrap(),
        Frame::solid(1280, 720, [10, 20, 30, 255])
    );
}

#[test]
fn sampling_matches_the_documented_pixel_center_at_exact_boundaries() {
    let frame = Frame::new(26, 2, (0..52).flat_map(|n| [n % 26, 0, 0, 255]).collect()).unwrap();
    let output = encode_image(frame, (0, 0), NonZeroU32::new(11), None).unwrap();
    let decoded = frame_from_webp(&output.bytes).unwrap();
    assert_eq!(
        decoded.pixels,
        [1, 3, 5, 8, 10, 13, 15, 17, 20, 22, 24]
            .into_iter()
            .flat_map(|n| [n, 0, 0, 255])
            .collect::<Vec<_>>()
    );
}

#[test]
fn malformed_frames_fail_without_allocating_from_invalid_dimensions() {
    for frame in [
        Frame {
            width: 0,
            height: 1,
            pixels: vec![],
        },
        Frame {
            width: 1,
            height: 0,
            pixels: vec![],
        },
        Frame {
            width: 2,
            height: 2,
            pixels: vec![0; 4],
        },
        Frame {
            width: 2,
            height: 2,
            pixels: vec![0; 20],
        },
        Frame {
            width: u32::MAX,
            height: u32::MAX,
            pixels: vec![],
        },
    ] {
        for bound in [None, NonZeroU32::new(1)] {
            assert!(encode_image(frame.clone(), (0, 0), bound, bound).is_err());
        }
    }
}

#[test]
fn bounds_parse_on_every_surface_and_reject_invalid_terminal_values() {
    for field in ["max_width", "max_height"] {
        for (value, valid) in [
            (json!(null), true),
            (json!(1), true),
            (json!(u32::MAX), true),
            (json!(0), false),
            (json!(-1), false),
            (json!(1.5), false),
            (json!("2"), false),
            (json!(true), false),
            (json!(4294967296_u64), false),
        ] {
            let args = json!({field:value,"name":"base","include_image":false});
            assert_eq!(
                serde_json::from_value::<ScreenshotArgs>(args.clone()).is_ok(),
                valid,
                "{args}"
            );
            assert_eq!(
                serde_json::from_value::<WaitStableArgs>(args.clone()).is_ok(),
                valid,
                "{args}"
            );
            assert_eq!(
                serde_json::from_value::<DiffArgs>(args.clone()).is_ok(),
                valid,
                "{args}"
            );
            assert_eq!(
                serde_json::from_value::<WaitForRegionArgs>(args.clone()).is_ok(),
                valid,
                "{args}"
            );
            for terminal in ["screenshot", "diff"] {
                let batch =
                    json!({"actions":[{"action":"key","chord":"Return"}],"then":{terminal:args}});
                assert_eq!(
                    serde_json::from_value::<DoArgs>(batch.clone()).is_ok(),
                    valid,
                    "{batch}"
                );
            }
        }
    }
}

#[test]
fn bounded_observation_of_another_window_preserves_the_active_window() {
    let active = Frame::solid(4, 4, [1, 2, 3, 255]);
    let other = Frame::solid(8, 6, [40, 50, 60, 255]);
    let mut g = started(
        FakePlatform::new(4, 4)
            .with_frames(vec![active.clone()])
            .with_window_frame(WindowId(7), other),
    );
    let args = serde_json::from_value(json!({"window_id":7,"max_height":3})).unwrap();
    let output = tools::screenshot(&mut g, &args).unwrap();
    assert_eq!(image(&output), Frame::solid(4, 3, [40, 50, 60, 255]));
    assert_eq!(
        result(&output)["image"]["source"],
        json!({"x":0,"y":0,"width":8,"height":6})
    );
    let native = tools::screenshot(&mut g, &serde_json::from_value(json!({})).unwrap()).unwrap();
    assert_eq!(image(&native), active);
    assert_eq!(result(&native), json!({"width":4,"height":4}));
}

#[test]
fn bounded_settle_uses_native_motion_and_only_sizes_the_final_crop() {
    for include_image in [true, false] {
        let base = Frame::solid(8, 6, [0, 0, 0, 255]);
        let mut changed = base.clone();
        changed.pixels[0] = 255;
        let captures = Arc::new(Mutex::new(0));
        let mut g = started(
            FakePlatform::new(8, 6)
                .with_frames(vec![base, changed.clone(), changed])
                .with_capture_log(captures.clone()),
        );
        let args = serde_json::from_value(json!({"max_width":2,"include_image":include_image,
            "interval_ms":0,"settle_frames":1,"region":{"x":2,"y":2,"width":4,"height":2}}))
        .unwrap();
        let out = tools::wait_stable(&mut g, &args).unwrap();
        let meta = result(&out);
        assert_eq!(meta["settled"], true);
        assert_eq!(meta["saw_motion"], true);
        assert_eq!(*captures.lock().unwrap(), 3);
        if include_image {
            assert_eq!(image(&out), Frame::solid(2, 1, [0, 0, 0, 255]));
            assert_eq!(
                meta["image"]["source"],
                json!({"x":2,"y":2,"width":4,"height":2})
            );
            assert_eq!(
                (meta["width"].as_u64(), meta["height"].as_u64()),
                (Some(2), Some(1))
            );
        } else {
            assert_eq!(out.0.len(), 1);
            assert!(meta.get("image").is_none());
            assert_eq!(
                (meta["width"].as_u64(), meta["height"].as_u64()),
                (Some(8), Some(6))
            );
        }
    }
}

#[test]
fn native_diff_detects_detail_lost_by_a_preview_and_maps_its_region_bbox() {
    for mode in ["exact", "perceptual"] {
        let base = Frame::solid(8, 6, [0, 0, 0, 255]);
        let mut changed = base.clone();
        for (x, y) in [(2, 1), (5, 3)] {
            let i = (y * 8 + x) * 4;
            changed.pixels[i..i + 3].fill(255);
        }
        let captures = Arc::new(Mutex::new(0));
        let mut g = started(
            FakePlatform::new(8, 6)
                .with_frames(vec![base, changed])
                .with_capture_log(captures.clone()),
        );
        tools::baseline_save(
            &mut g,
            &BaselineSaveArgs {
                name: "base".into(),
            },
        )
        .unwrap();
        let preview = tools::screenshot(
            &mut g,
            &serde_json::from_value(json!({"max_width":1})).unwrap(),
        )
        .unwrap();
        assert_eq!(image(&preview), Frame::solid(1, 1, [0, 0, 0, 255]));
        let args = serde_json::from_value(
            json!({"name":"base","mode":mode,"include_image":true,"max_width":1,
            "region":{"x":1,"y":1,"width":6,"height":4},
            "ignore":[{"x":6,"y":4,"width":1,"height":1}]}),
        )
        .unwrap();
        let out = tools::diff(&mut g, &args).unwrap();
        let meta = result(&out);
        assert_eq!(meta["changed_pixels"], 2);
        assert_eq!(meta["total_pixels"], 24);
        assert_eq!(meta["ignored_pixels"], 1);
        assert_eq!(meta["bbox"], json!({"x":1,"y":0,"width":4,"height":3}));
        assert_eq!(
            meta["image"]["source"],
            json!({"x":2,"y":1,"width":4,"height":3})
        );
        assert_eq!(image(&out), Frame::solid(1, 1, [0, 0, 0, 255]));
        assert_eq!(*captures.lock().unwrap(), 3);
        let native = tools::diff(
            &mut g,
            &serde_json::from_value(json!({"name":"base","mode":"exact"})).unwrap(),
        )
        .unwrap();
        assert_eq!(result(&native)["changed_pixels"], 2);
        assert_eq!(result(&native)["total_pixels"], 48);
    }
}

#[test]
fn bounded_region_wait_keeps_native_predicates_and_window_relative_bbox() {
    for mode in ["exact", "perceptual"] {
        for until in ["changes", "matches"] {
            let base = Frame::solid(8, 6, [0, 0, 0, 255]);
            let mut changed = base.clone();
            changed.pixels[(2 * 8 + 2) * 4..(2 * 8 + 2) * 4 + 3].fill(255);
            let frames = if until == "matches" {
                vec![base.clone(), changed, base]
            } else {
                vec![base.clone(), base, changed]
            };
            let mut g = started(FakePlatform::new(8, 6).with_frames(frames));
            tools::baseline_save(
                &mut g,
                &BaselineSaveArgs {
                    name: "base".into(),
                },
            )
            .unwrap();
            let args = serde_json::from_value(json!({"baseline":"base","until":until,"mode":mode,
                "region":{"x":2,"y":2,"width":4,"height":2},"interval_ms":0,
                "include_image":true,"max_width":1}))
            .unwrap();
            let out = tools::wait_for_region(&mut g, &args).unwrap();
            let meta = result(&out);
            assert_eq!(meta["matched"], true);
            assert_eq!(image(&out), Frame::solid(1, 1, [0, 0, 0, 255]));
            assert_eq!(
                meta["image"]["source"],
                json!({"x":2,"y":2,"width":4,"height":2})
            );
            if until == "changes" {
                assert_eq!(meta["bbox"], json!({"x":2,"y":2,"width":1,"height":1}));
                assert_eq!(meta["changed_pct"], 12.5);
            }
        }
    }
}

#[test]
fn bounds_do_not_create_suppressed_or_unmatched_images() {
    let base = Frame::solid(4, 4, [0, 0, 0, 255]);
    let mut g = started(FakePlatform::new(4, 4).with_frames(vec![base]));
    tools::baseline_save(
        &mut g,
        &BaselineSaveArgs {
            name: "base".into(),
        },
    )
    .unwrap();
    let unchanged = tools::diff(
        &mut g,
        &serde_json::from_value(json!({"name":"base","include_image":true,"max_width":1})).unwrap(),
    )
    .unwrap();
    let unmatched = tools::wait_for_region(
        &mut g,
        &serde_json::from_value(
            json!({"baseline":"base","timeout_ms":0,"include_image":true,"max_width":1}),
        )
        .unwrap(),
    )
    .unwrap();
    let suppressed = tools::wait_for_region(
        &mut g,
        &serde_json::from_value(json!({"baseline":"base","until":"matches","max_width":1}))
            .unwrap(),
    )
    .unwrap();
    for output in [unchanged, unmatched, suppressed] {
        assert_eq!(output.0.len(), 1);
        assert!(result(&output).get("image").is_none());
    }
}

#[test]
fn bounded_batch_preserves_terminal_metadata_and_failure_after_one_input() {
    let events = Arc::new(Mutex::new(vec![]));
    let captures = Arc::new(Mutex::new(0));
    let mut g = started(
        FakePlatform::new(4, 4)
            .with_frames(vec![
                Frame::solid(4, 4, [1, 2, 3, 255]),
                Frame {
                    width: 4,
                    height: 4,
                    pixels: vec![0; 4],
                },
            ])
            .with_event_log(events.clone())
            .with_capture_log(captures.clone()),
    );
    let args: DoArgs =
        serde_json::from_value(json!({"actions":[{"action":"key","chord":"Return"}],
        "then":{"screenshot":{"max_width":2}}}))
        .unwrap();
    let output = tools::do_actions(&mut g, &args).unwrap();
    assert!(matches!(output.0[0], OutContent::Envelope(_)));
    assert_eq!(result(&output)["then"]["screenshot"]["image"]["width"], 2);
    assert_eq!(image(&output), Frame::solid(2, 2, [1, 2, 3, 255]));
    events.lock().unwrap().clear();
    *captures.lock().unwrap() = 0;
    let failure = tools::do_actions(&mut g, &args).unwrap_err();
    let rendered = failure
        .0
        .iter()
        .find_map(|b| {
            if let OutContent::Text(t) = b {
                Some(t.body.as_str())
            } else {
                None
            }
        })
        .unwrap();
    let envelope: Value = serde_json::from_str(rendered).unwrap();
    assert_eq!(envelope["error"]["code"], "terminal_observe_failed");
    assert_eq!(envelope["outcome"]["executed"], 1);
    assert_eq!(envelope["outcome"]["steps"][0]["status"], "completed");
    assert!(rendered.contains("do not replay"));
    assert_eq!(events.lock().unwrap().len(), 1);
    assert_eq!(*captures.lock().unwrap(), 1);
}

#[test]
fn bounded_batch_diff_and_screenshot_keep_separate_source_rectangles() {
    let base = Frame::solid(8, 8, [0, 0, 0, 255]);
    let mut changed = base.clone();
    for y in 2..6 {
        for x in 2..6 {
            changed.pixels[(y * 8 + x) * 4] = 200;
        }
    }
    let mut g = started(FakePlatform::new(8, 8).with_frames(vec![base, changed]));
    tools::baseline_save(
        &mut g,
        &BaselineSaveArgs {
            name: "base".into(),
        },
    )
    .unwrap();
    let args = serde_json::from_value(
        json!({"actions":[{"action":"key","chord":"Return"}],"then":{
        "diff":{"name":"base","mode":"exact","include_image":true,"max_height":2},
        "screenshot":{"max_width":4}}}),
    )
    .unwrap();
    let output = tools::do_actions(&mut g, &args).unwrap();
    let meta = result(&output);
    assert_eq!(
        meta["then"]["diff"]["image"]["source"],
        json!({"x":2,"y":2,"width":4,"height":4})
    );
    assert_eq!(
        meta["then"]["screenshot"]["image"]["source"],
        json!({"x":0,"y":0,"width":8,"height":8})
    );
    let dims = output
        .0
        .iter()
        .filter_map(|b| {
            if let OutContent::Image(bytes) = b {
                let image = frame_from_webp(bytes).unwrap();
                Some((image.width, image.height))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(dims, [(2, 2), (4, 4)]);
    assert_eq!(meta["terminal_steps"][0]["content_blocks"], json!([1, 2]));
    assert_eq!(meta["terminal_steps"][1]["content_blocks"], json!([3, 4]));
}
