//! WGC capture probe (validation items 1, 2, and the minimized half of 7).
//! Captures one window by title substring via xcap (Windows.Graphics.Capture
//! under the hood), saves a PNG, and asserts the frame is non-blank.

use anyhow::{bail, Context};
use windows::Win32::UI::WindowsAndMessaging::IsIconic;

use crate::util::find_by_title;

pub fn run(needle: &str, out: &str) -> anyhow::Result<()> {
    let windows = xcap::Window::all().context("xcap::Window::all() failed")?;
    let n = needle.to_lowercase();

    let mut matches: Vec<&xcap::Window> =
        windows.iter().filter(|w| w.title().to_lowercase().contains(&n)).collect();

    if matches.is_empty() {
        eprintln!("no window title contains {needle:?}. Visible titles:");
        for w in &windows {
            let t = w.title();
            if !t.is_empty() {
                eprintln!("  - {t}");
            }
        }
        bail!("no matching window");
    }
    if matches.len() > 1 {
        println!("note: {} windows matched; capturing the first.", matches.len());
    }
    let win = matches.remove(0);

    println!(
        "capturing '{}'  ({}x{}) at ({}, {})",
        win.title(),
        win.width(),
        win.height(),
        win.x(),
        win.y()
    );

    // item 7: a minimized window yields a stale/frozen (often blank) frame — the real
    // backend must detect IsIconic and error/restore, never return it as a live frame.
    let mut iconic = false;
    for w in find_by_title(needle) {
        if unsafe { IsIconic(w.hwnd()) }.as_bool() {
            iconic = true;
        }
    }
    if win.is_minimized() || iconic {
        println!("WARNING: target is MINIMIZED — WGC returns a stale/blank frame here (item 7).");
    }

    let img = win.capture_image().context("WGC capture_image() failed")?;
    let (w, h) = (img.width(), img.height());
    img.save(out).with_context(|| format!("saving {out}"))?;

    let blank = is_blank(&img);
    println!("saved {out}  ({w}x{h})");
    if blank {
        println!("FAIL: frame is blank/uniform — WGC did not return real pixels here.");
    } else {
        println!("PASS: captured NON-BLANK {w}x{h} pixels.");
    }
    Ok(())
}

/// True if every pixel is identical (a uniform/blank frame).
fn is_blank(img: &image::RgbaImage) -> bool {
    let mut pixels = img.pixels();
    let Some(first) = pixels.next() else {
        return true;
    };
    pixels.all(|p| p == first)
}
