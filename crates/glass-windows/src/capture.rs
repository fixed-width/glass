//! WGC (Windows.Graphics.Capture) one-shot capture: grab the active window's
//! pixels via `windows-capture`'s FrameArrived callback, read them back to the
//! CPU, swizzle BGRA -> RGBA, and hand back a `glass_core::frame::Frame`.
//!
//! The capture runs on a dedicated thread (`start_free_threaded`) that owns the
//! WGC message pump, so a synchronous one-shot never pumps a message loop on the
//! caller's thread. The first frame is copied across an mpsc channel and the
//! capture is then stopped. Every failure becomes `GlassError::CaptureFailed` —
//! we never return a blank or stale frame (repo invariant: no silent fallbacks).
//!
//! We request `ColorFormat::Bgra8` (WGC's native layout); the captured alpha is
//! unreliable for opaque windows and is normalized to opaque (255) downstream by
//! [`crate::pixels::bgra_to_rgba`], which also does the BGRA -> RGBA swizzle.

use std::sync::mpsc::{channel, Receiver, Sender};

use glass_core::frame::{Frame, Region};
use glass_core::{GlassError, Result};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::IsIconic;

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame as WgcFrame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

/// Max wait for WGC to deliver the first frame. WGC normally delivers in <100ms; the
/// long tail is GPU spin-up / capture-permission resolution. Blocks the synchronous caller.
const FIRST_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What `on_frame_arrived` ships back to the caller: tightly-packed BGRA bytes
/// plus the frame dimensions.
type FramePayload = (Vec<u8>, u32, u32);

/// A one-shot WGC handler: copies the first frame's BGRA bytes to a channel,
/// then stops the capture.
struct OneShot {
    tx: Sender<FramePayload>,
}

impl GraphicsCaptureApiHandler for OneShot {
    type Flags = Sender<FramePayload>;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
        Ok(Self { tx: ctx.flags })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut WgcFrame,
        capture_control: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        let (w, h) = (frame.width(), frame.height());
        let fb = frame.buffer()?;
        // Tightly-packed (RowPitch padding removed): width*height*4 BGRA bytes. windows-capture 2
        // writes the de-padded bytes into a caller-provided buffer and returns a borrowed slice.
        let mut packed = Vec::new();
        let owned = fb.as_nopadding_buffer(&mut packed).to_vec();
        // If the receiver is gone the caller already bailed; just stop cleanly.
        let _ = self.tx.send((owned, w, h));
        capture_control.stop();
        Ok(())
    }
}

/// Grab one BGRA frame for `hwnd` via WGC. Returns (bgra, width, height).
fn wgc_one_frame(hwnd: HWND) -> std::result::Result<FramePayload, String> {
    let (tx, rx): (Sender<FramePayload>, Receiver<FramePayload>) = channel();

    let window = Window::from_raw_hwnd(hwnd.0);
    let settings = Settings::new(
        window,
        CursorCaptureSettings::WithoutCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        tx,
    );

    let control =
        OneShot::start_free_threaded(settings).map_err(|e| format!("start capture: {e}"))?;
    // Wait for the first frame; a missing display/permission stalls here, so cap it.
    let result = rx.recv_timeout(FIRST_FRAME_TIMEOUT);
    // Tear the capture thread down on EVERY path. On timeout/disconnect the handler
    // never signalled stop, so without this the capture thread (+ its D3D11 device and
    // WGC session) detaches and loops in GetMessageW forever — CaptureControl has no Drop.
    // stop() sets `halt`, posts WM_QUIT until the thread accepts it (special-casing a dead
    // thread), then joins — so it cleanly unblocks a thread waiting on frames that never come.
    let _ = control.stop();
    let got = result.map_err(|e| format!("WGC delivered no frame: {e}"))?;
    Ok(got)
}

/// Capture `hwnd` as an RGBA `Frame`, optionally cropped (window-relative).
pub(crate) fn capture_window(hwnd: HWND, region: Option<&Region>) -> Result<Frame> {
    // SAFETY: IsIconic is a pure query on an HWND with no preconditions; it
    // returns a BOOL and touches no memory we own.
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return Err(GlassError::CaptureFailed(
            "target window is minimized; WGC returns a stale/blank frame — restore it first".into(),
        ));
    }

    let (mut bgra, w, h) = wgc_one_frame(hwnd)
        .map_err(|e| GlassError::CaptureFailed(format!("WGC capture failed: {e}")))?;
    crate::pixels::bgra_to_rgba(&mut bgra);
    let frame = Frame::new(w, h, bgra)?; // validates len; propagates CaptureFailed on mismatch
    match region {
        Some(r) => frame.crop(r), // propagates InvalidRegion; never clamps
        None => Ok(frame),
    }
}
