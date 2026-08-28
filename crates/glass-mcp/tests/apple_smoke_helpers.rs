#[path = "common/apple_smoke.rs"]
mod apple_smoke;

use apple_smoke::{ElementRoi, assert_roi_changed, unique_named_element};
use image::{Rgba, RgbaImage};

fn frames_with_significant_changes(changed_pixels: u32) -> (RgbaImage, RgbaImage, ElementRoi) {
    let before = RgbaImage::from_pixel(4, 2, Rgba([20, 30, 40, 255]));
    let mut after = RgbaImage::from_pixel(7, 6, Rgba([20, 30, 40, 255]));
    let roi = ElementRoi {
        id: 7,
        x: 2,
        y: 3,
        width: 4,
        height: 2,
    };
    for index in 0..changed_pixels {
        let x = index % roi.width;
        let y = index / roi.width;
        after.put_pixel(roi.x + x, roi.y + y, Rgba([45, 30, 40, 255]));
    }
    (before, after, roi)
}

#[test]
fn named_element_parser_retains_public_id_and_window_relative_roi() {
    let outline = r#"
  #3 CheckBox "Enable" (20,40 120x24) [enabled,visible,checkable]
  #4 Button "Other" (20,80 80x24) [enabled,visible]
"#;

    assert_eq!(
        unique_named_element(outline, "Enable").unwrap(),
        ElementRoi {
            id: 3,
            x: 20,
            y: 40,
            width: 120,
            height: 24,
        }
    );
}

#[test]
fn unchanged_terminal_roi_is_rejected_as_stale() {
    let before = RgbaImage::from_pixel(2, 2, Rgba([20, 30, 40, 255]));
    let after = RgbaImage::from_pixel(4, 4, Rgba([20, 30, 40, 255]));
    let roi = ElementRoi {
        id: 3,
        x: 1,
        y: 1,
        width: 2,
        height: 2,
    };

    let error = assert_roi_changed(&before, &after, roi, "fixture status")
        .expect_err("a pre-action terminal frame must fail the temporal proof");
    assert!(error.contains("stale"), "{error}");
}

#[test]
fn production_policy_rejects_one_through_seven_significant_pixel_changes() {
    for changed_pixels in 1..=7 {
        let (before, after, roi) = frames_with_significant_changes(changed_pixels);

        let error = assert_roi_changed(&before, &after, roi, "fixture status")
            .expect_err("the production eight-pixel floor must reject this change count");

        assert!(
            error.contains(&format!(
                "changed {changed_pixels} pixels, expected at least 8"
            )),
            "unexpected rejection for {changed_pixels} changed pixels: {error}"
        );
    }
}

#[test]
fn production_policy_counts_delta_25_but_not_delta_24_at_the_eight_pixel_floor() {
    let before = RgbaImage::from_pixel(3, 3, Rgba([20, 30, 40, 255]));
    let mut after = RgbaImage::from_pixel(6, 6, Rgba([20, 30, 40, 255]));
    let roi = ElementRoi {
        id: 7,
        x: 1,
        y: 2,
        width: 3,
        height: 3,
    };
    for index in 0..8 {
        after.put_pixel(
            roi.x + index % 3,
            roi.y + index / 3,
            Rgba([45, 30, 40, 255]),
        );
    }
    after.put_pixel(roi.x + 2, roi.y + 2, Rgba([44, 30, 40, 255]));

    assert_eq!(
        assert_roi_changed(&before, &after, roi, "fixture status").unwrap(),
        8
    );
}

#[test]
fn production_policy_scales_the_minimum_for_larger_rois() {
    let before = RgbaImage::from_pixel(100, 100, Rgba([20, 30, 40, 255]));
    let mut after = RgbaImage::from_pixel(102, 103, Rgba([20, 30, 40, 255]));
    let roi = ElementRoi {
        id: 7,
        x: 1,
        y: 2,
        width: 100,
        height: 100,
    };
    for x in 0..9 {
        after.put_pixel(roi.x + x, roi.y, Rgba([45, 30, 40, 255]));
    }

    let error = assert_roi_changed(&before, &after, roi, "fixture status")
        .expect_err("a 10,000-pixel ROI must require ten significant changes");

    assert!(
        error.contains("changed 9 pixels, expected at least 10"),
        "{error}"
    );
}

#[test]
fn terminal_proof_maps_asymmetric_nonuniform_roi_axes_and_extents() {
    let roi = ElementRoi {
        id: 7,
        x: 2,
        y: 5,
        width: 4,
        height: 3,
    };
    let mut before = RgbaImage::new(roi.width, roi.height);
    for y in 0..roi.height {
        for x in 0..roi.width {
            before.put_pixel(
                x,
                y,
                Rgba([
                    (10 + x * 20 + y * 3) as u8,
                    (20 + y * 30 + x) as u8,
                    (30 + x * 7 + y * 11) as u8,
                    255,
                ]),
            );
        }
    }
    let mut after = RgbaImage::from_pixel(6, 8, Rgba([250, 251, 252, 255]));
    for y in 0..roi.height {
        for x in 0..roi.width {
            after.put_pixel(roi.x + x, roi.y + y, *before.get_pixel(x, y));
        }
    }
    for index in 0..8 {
        let x = index % roi.width;
        let y = index / roi.width;
        let before_pixel = before.get_pixel(x, y);
        after.put_pixel(
            roi.x + x,
            roi.y + y,
            Rgba([
                before_pixel[0] + 25,
                before_pixel[1],
                before_pixel[2],
                before_pixel[3],
            ]),
        );
    }

    assert_eq!(
        assert_roi_changed(&before, &after, roi, "fixture status").unwrap(),
        8
    );
}
