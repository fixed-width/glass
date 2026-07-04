//! Set-of-Mark overlay: render the interactable elements of an `AxTree` as a
//! numbered overlay on a captured `Frame`. Pure pixel work — no OS/display deps.
//!
//! Each marked element gets a colored outline plus a small numbered chip anchored
//! just **outside** its top-left corner (so it never covers a small element's own
//! pixels, e.g. an icon button). The chip number is the element's `AxNodeId`, so
//! `glass_click_element` clicks a mark directly. All drawing is clipped to the frame.

use crate::accessibility::{AxNode, AxNodeId, AxRect, AxRole, AxTree};
use crate::frame::Frame;

/// One rendered mark, for the text legend returned alongside the image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub id: AxNodeId,
    pub role: AxRole,
    pub name: Option<String>,
}

const SCALE: i32 = 2; // integer upscale of the 3x5 font cells
const PAD: i32 = 1; // chip padding around the digits, in output pixels
const DIGIT_W: i32 = 3; // font cell width
const DIGIT_H: i32 = 5; // font cell height
const DIGIT_GAP: i32 = 1; // gap between digits, in font cells
const OUTLINE: [u8; 4] = [255, 0, 255, 255]; // magenta — element outline + chip bg
const DIGIT_FG: [u8; 4] = [255, 255, 255, 255]; // white digits

/// 3x5 bitmap font, digits 0-9. Each row is 3 bits; bit2 = leftmost column.
const FONT_3X5: [[u8; 5]; 10] = [
    [7, 5, 5, 5, 7], // 0
    [2, 6, 2, 2, 7], // 1
    [7, 1, 7, 4, 7], // 2
    [7, 1, 7, 1, 7], // 3
    [5, 5, 7, 1, 1], // 4
    [7, 4, 7, 1, 7], // 5
    [7, 4, 7, 5, 7], // 6
    [7, 1, 2, 2, 2], // 7
    [7, 5, 7, 5, 7], // 8
    [7, 5, 7, 1, 7], // 9
];

/// Render the tree's interactable elements as a numbered overlay onto a clone of
/// `frame`. Returns the annotated frame and the legend, in ascending-id (DFS) order.
/// Nodes without bounds or with zero area are skipped; all drawing is clipped.
pub fn render(frame: &Frame, tree: &AxTree) -> (Frame, Vec<Mark>) {
    let mut out = frame.clone();
    let mut legend = Vec::new();
    collect(&tree.root, &mut out, &mut legend);
    (out, legend)
}

fn collect(node: &AxNode, frame: &mut Frame, legend: &mut Vec<Mark>) {
    if node.role.is_interactable() {
        if let Some(b) = node.bounds {
            if b.width > 0 && b.height > 0 {
                draw_mark(frame, b, node.id.0);
                legend.push(Mark {
                    id: node.id,
                    role: node.role,
                    name: node.name.clone(),
                });
            }
        }
    }
    for child in &node.children {
        collect(child, frame, legend);
    }
}

fn draw_mark(frame: &mut Frame, b: AxRect, id: u32) {
    // 1. element outline
    draw_rect_outline(frame, b.x, b.y, b.width as i32, b.height as i32, OUTLINE);

    // 2. numbered chip, anchored OUTSIDE the element's top-left (up & left), then
    //    clamped into the frame so an edge-hugging element still shows its chip.
    let ds = digits_of(id);
    let n = ds.len() as i32;
    let chip_w = PAD * 2 + n * DIGIT_W * SCALE + (n - 1) * DIGIT_GAP * SCALE;
    let chip_h = PAD * 2 + DIGIT_H * SCALE;
    let cx = (b.x - chip_w).max(0);
    let cy = (b.y - chip_h).max(0);
    fill_rect(frame, cx, cy, chip_w, chip_h, OUTLINE);

    let mut dx = cx + PAD;
    let dy = cy + PAD;
    for d in ds {
        draw_digit(frame, d, dx, dy, DIGIT_FG);
        dx += (DIGIT_W + DIGIT_GAP) * SCALE;
    }
}

fn digits_of(mut n: u32) -> Vec<u8> {
    if n == 0 {
        return vec![0];
    }
    let mut v = Vec::new();
    while n > 0 {
        v.push((n % 10) as u8);
        n /= 10;
    }
    v.reverse();
    v
}

/// Set one pixel; out-of-frame coordinates are ignored (clipping).
fn put_px(frame: &mut Frame, x: i32, y: i32, rgba: [u8; 4]) {
    if x < 0 || y < 0 || x >= frame.width as i32 || y >= frame.height as i32 {
        return;
    }
    let i = (y as usize * frame.width as usize + x as usize) * 4;
    frame.pixels[i..i + 4].copy_from_slice(&rgba);
}

fn fill_rect(frame: &mut Frame, x: i32, y: i32, w: i32, h: i32, rgba: [u8; 4]) {
    for yy in y..y + h {
        for xx in x..x + w {
            put_px(frame, xx, yy, rgba);
        }
    }
}

fn draw_rect_outline(frame: &mut Frame, x: i32, y: i32, w: i32, h: i32, rgba: [u8; 4]) {
    if w <= 0 || h <= 0 {
        return;
    }
    for xx in x..x + w {
        put_px(frame, xx, y, rgba);
        put_px(frame, xx, y + h - 1, rgba);
    }
    for yy in y..y + h {
        put_px(frame, x, yy, rgba);
        put_px(frame, x + w - 1, yy, rgba);
    }
}

/// Blit one digit (0-9) at (x,y), each font pixel a `SCALE`x`SCALE` block.
fn draw_digit(frame: &mut Frame, digit: u8, x: i32, y: i32, rgba: [u8; 4]) {
    for (row, &bits) in FONT_3X5[digit as usize].iter().enumerate() {
        for col in 0u8..3 {
            if bits & (1u8 << (2 - col)) != 0 {
                fill_rect(
                    frame,
                    x + col as i32 * SCALE,
                    y + row as i32 * SCALE,
                    SCALE,
                    SCALE,
                    rgba,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * frame.width as usize + x as usize) * 4;
        frame.pixels[i..i + 4].try_into().unwrap()
    }

    fn node(id: u32, role: AxRole, name: &str, bounds: Option<AxRect>) -> AxNode {
        AxNode {
            id: AxNodeId(id),
            role,
            raw_role: String::new(),
            name: Some(name.into()),
            value: None,
            states: Default::default(),
            bounds,
            children: vec![],
        }
    }

    /// Window (not interactable) containing a Button and a Label.
    fn tree() -> AxTree {
        let button = node(
            0,
            AxRole::Button,
            "Save",
            Some(AxRect {
                x: 10,
                y: 10,
                width: 20,
                height: 16,
            }),
        );
        let label = node(
            0,
            AxRole::Label,
            "Ready",
            Some(AxRect {
                x: 10,
                y: 40,
                width: 30,
                height: 10,
            }),
        );
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "frame".into(),
            name: Some("Win".into()),
            value: None,
            states: Default::default(),
            bounds: Some(AxRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }),
            children: vec![button, label],
        };
        let mut t = AxTree { root, count: 0 };
        t.assign_ids();
        t
    }

    #[test]
    fn fill_and_outline_clip_at_edges() {
        let mut f = Frame::solid(4, 4, [0, 0, 0, 255]);
        fill_rect(&mut f, 2, 2, 10, 10, [1, 2, 3, 255]);
        assert_eq!(px(&f, 3, 3), [1, 2, 3, 255]);
        assert_eq!(px(&f, 0, 0), [0, 0, 0, 255]);
        draw_rect_outline(&mut f, -5, -5, 7, 7, [9, 9, 9, 255]); // bottom-right corner at (1,1)
        assert_eq!(px(&f, 1, 1), [9, 9, 9, 255]);
    }

    #[test]
    fn render_marks_only_interactables_and_builds_legend() {
        let (out, legend) = render(&Frame::solid(100, 100, [0, 0, 0, 255]), &tree());
        assert_eq!(legend.len(), 1);
        assert_eq!(legend[0].id, AxNodeId(1));
        assert_eq!(legend[0].role, AxRole::Button);
        assert_eq!(legend[0].name.as_deref(), Some("Save"));
        assert_eq!(px(&out, 10, 10), OUTLINE);
        assert_eq!(px(&out, 10, 40), [0, 0, 0, 255]);
    }

    #[test]
    fn render_with_no_interactables_returns_clone_and_empty_legend() {
        let label_only = {
            let root = node(
                0,
                AxRole::Label,
                "x",
                Some(AxRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                }),
            );
            let mut t = AxTree { root, count: 0 };
            t.assign_ids();
            t
        };
        let frame = Frame::solid(8, 8, [7, 7, 7, 255]);
        let (out, legend) = render(&frame, &label_only);
        assert!(legend.is_empty());
        assert_eq!(out, frame);
    }

    #[test]
    fn offscreen_element_does_not_panic() {
        let off = {
            let root = node(
                0,
                AxRole::Button,
                "b",
                Some(AxRect {
                    x: 90,
                    y: 90,
                    width: 40,
                    height: 40,
                }),
            );
            let mut t = AxTree { root, count: 0 };
            t.assign_ids();
            t
        };
        let (_out, legend) = render(&Frame::solid(8, 8, [0, 0, 0, 255]), &off);
        assert_eq!(legend.len(), 1);
    }
}
