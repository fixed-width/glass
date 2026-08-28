#[path = "common/apple_smoke.rs"]
mod apple_smoke;

use apple_smoke::{ElementRoi, assert_roi_changed, unique_named_element};
use image::{Rgba, RgbaImage};

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

    let error = assert_roi_changed(&before, &after, roi, 16, 1, "fixture status")
        .expect_err("a pre-action terminal frame must fail the temporal proof");
    assert!(error.contains("stale"), "{error}");
}

#[test]
fn terminal_proof_counts_only_significant_changes_inside_the_fixture_roi() {
    let before = RgbaImage::from_pixel(2, 2, Rgba([20, 30, 40, 255]));
    let mut after = RgbaImage::from_pixel(4, 4, Rgba([20, 30, 40, 255]));
    after.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
    after.put_pixel(1, 1, Rgba([60, 30, 40, 255]));
    after.put_pixel(2, 2, Rgba([30, 30, 40, 255]));
    let roi = ElementRoi {
        id: 7,
        x: 1,
        y: 1,
        width: 2,
        height: 2,
    };

    assert_eq!(
        assert_roi_changed(&before, &after, roi, 16, 1, "fixture status").unwrap(),
        1
    );
}
