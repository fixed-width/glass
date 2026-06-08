//! PrintWindow probe (validation item 8): GDI `PrintWindow(PW_RENDERFULLCONTENT)`.
//! Run against a plain Win32 app (expect real content) and a GPU/Chromium/Electron
//! app (expect BLACK) — confirming the fallback is partial-only.

use std::ffi::c_void;
use anyhow::bail;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};

use crate::util::{find_by_title, window_rect};

const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(0x0000_0002);

pub fn run(needle: &str, out: &str) -> anyhow::Result<()> {
    let hits = find_by_title(needle);
    let Some(win) = hits.first() else {
        bail!("no app window title contains {needle:?}");
    };
    let hwnd = win.hwnd();
    let Some(r) = window_rect(hwnd) else {
        bail!("could not read window rect");
    };
    let (w, h) = ((r.right - r.left).max(1), (r.bottom - r.top).max(1));
    println!("PrintWindow '{}'  ({w}x{h})", win.title);

    let pixels = capture_gdi(hwnd, w, h)?;
    let img = image::RgbaImage::from_raw(w as u32, h as u32, pixels)
        .ok_or_else(|| anyhow::anyhow!("pixel buffer size mismatch"))?;
    img.save(out)?;

    let black = img.pixels().all(|p| p.0[0] == 0 && p.0[1] == 0 && p.0[2] == 0);
    println!("saved {out}");
    if black {
        println!("BLACK: GPU/hardware-composited content (expected for Chromium/Electron/D3D).");
        println!("  -> confirms PrintWindow is only a PARTIAL fallback; WGC must stay primary.");
    } else {
        println!("CONTENT: PrintWindow returned real pixels (plain GDI/Win32 window).");
    }
    Ok(())
}

/// PrintWindow into a compatible bitmap and read back BGRA -> RGBA.
fn capture_gdi(hwnd: HWND, w: i32, h: i32) -> anyhow::Result<Vec<u8>> {
    // SAFETY: GDI handle dance — each created object is selected out and deleted before return.
    unsafe {
        let screen = GetDC(HWND(std::ptr::null_mut()));
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));

        let ok = PrintWindow(hwnd, mem, PW_RENDERFULLCONTENT);

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // negative = top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );

        let _ = SelectObject(mem, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        ReleaseDC(HWND(std::ptr::null_mut()), screen);

        if !ok.as_bool() {
            eprintln!("warning: PrintWindow returned FALSE");
        }
        if lines == 0 {
            bail!("GetDIBits read 0 lines");
        }

        // BGRA -> RGBA, force opaque alpha.
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }
        Ok(buf)
    }
}
