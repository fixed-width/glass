use super::{DisplayProvider, ProvisionedDisplay};
use glass_core::Result;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
};

/// Use whatever display the logged-in interactive session already presents — a real monitor,
/// a dummy plug, or a virtual monitor from an indirect-display driver (e.g. mttvdd) on a
/// headless box. No provisioning. See docs/running-on-windows.md.
pub(crate) struct ExistingDesktop;

impl DisplayProvider for ExistingDesktop {
    fn ensure(&mut self) -> Result<ProvisionedDisplay> {
        // SAFETY: GetSystemMetrics is a pure query with no preconditions.
        let w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(0) as u32;
        // SAFETY: GetSystemMetrics is a pure query with no preconditions.
        let h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(0) as u32;
        Ok(ProvisionedDisplay {
            width: w,
            height: h,
        })
    }
}
