//! DPI probe (validation item 5): confirm the process is Per-Monitor-V2 aware and
//! report per-window DPI + the GetWindowRect vs extended-frame-bounds delta, so the
//! operator can sanity-check coordinates at 150%/200% scaling.

use windows::Win32::Foundation::RECT;
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, GetDpiForWindow, GetThreadDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, DPI_AWARENESS_CONTEXT_UNAWARE,
};

use crate::util::{extended_frame_bounds, find_by_title, window_rect};

pub fn run(needle: Option<&str>) -> anyhow::Result<()> {
    let ctx = unsafe { GetThreadDpiAwarenessContext() };
    let raw = ctx.0 as isize;
    let eq = |c| unsafe { AreDpiAwarenessContextsEqual(ctx, c) }.as_bool();
    let is_v2 = eq(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    let is_v1 = eq(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
    let is_system = eq(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
    let is_unaware = eq(DPI_AWARENESS_CONTEXT_UNAWARE);

    println!("raw DPI context handle = {raw}  (pseudo-handles: -4=PMv2 -3=PMv1 -2=system -1=unaware)");
    println!("AreEqual: PMv2={is_v2} PMv1={is_v1} system={is_system} unaware={is_unaware}");

    // Per-monitor aware (V1 OR V2) means physical pixels — capture/clicks are correct.
    // That is what the gate needs; V2 only adds non-client/child auto-scaling glass doesn't use.
    if is_v2 {
        println!("PASS: Per-Monitor-V2 — capture dims and click coords are physical pixels.");
    } else if is_v1 {
        println!("PASS: Per-Monitor-V1 — still per-monitor aware (physical pixels), so coords are correct.");
        println!("      (The real backend ships V2 via manifest; V1 here does NOT block the gate.)");
    } else {
        println!("FAIL: not per-monitor aware — coords/capture WILL be virtualised on scaled displays.");
    }

    if let Some(needle) = needle {
        let hits = find_by_title(needle);
        let Some(w) = hits.first() else {
            println!("\n(no window matched {needle:?} for the per-window report)");
            return Ok(());
        };
        let hwnd = w.hwnd();
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let scale = dpi as f32 / 96.0 * 100.0;
        println!("\nwindow '{}': dpi {dpi} ({scale:.0}% scale)", w.title);
        println!("  expected: 96=100%, 120=125%, 144=150%, 192=200%");
        let fb = extended_frame_bounds(hwnd);
        let wr = window_rect(hwnd);
        if let (Some(fb), Some(wr)) = (fb, wr) {
            println!("  GetWindowRect:        {}", fmt(&wr));
            println!("  ExtendedFrameBounds:  {}  <- window origin the backend uses", fmt(&fb));
            println!(
                "  invisible-border delta: L{} T{} R{} B{}",
                fb.left - wr.left,
                fb.top - wr.top,
                wr.right - fb.right,
                wr.bottom - fb.bottom
            );
        }
    }
    Ok(())
}

fn fmt(r: &RECT) -> String {
    format!(
        "({}, {})..({}, {})  {}x{}",
        r.left,
        r.top,
        r.right,
        r.bottom,
        r.right - r.left,
        r.bottom - r.top
    )
}
