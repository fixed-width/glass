//! `AndroidA11y` — the Android accessibility reader. Drives `uiautomator dump`
//! over adb and maps the result via `crate::axmap`. Resolves its own device
//! lazily, since the `Accessibility` trait is handed only an `AxContext`.

use glass_core::accessibility::{Accessibility, AxContext, AxTree};
use glass_core::Result;

use crate::adb::Adb;
use crate::axmap::{build_tree, check_dump_status};
use crate::target::{choose_serial, parse_devices};

const DUMP_PATH: &str = "/sdcard/glass_dump.xml";

/// Reads the active window's accessibility tree via `uiautomator`.
pub struct AndroidA11y {
    adb: Adb,
    resolved: bool,
}

impl AndroidA11y {
    pub fn new() -> Self {
        Self { adb: Adb::from_env(), resolved: false }
    }

    /// Bind the adb client to a device serial on first use (lazy).
    fn ensure_adb(&mut self) -> Result<Adb> {
        if !self.resolved {
            let listing = self.adb.run(["devices"])?;
            let online: Vec<_> = parse_devices(&listing)
                .into_iter()
                .filter(|d| d.state == "device")
                .collect();
            let serial = choose_serial(std::env::var("GLASS_ANDROID_SERIAL").ok().as_deref(), &online)?;
            self.adb = self.adb.with_serial(serial);
            self.resolved = true;
        }
        Ok(self.adb.clone())
    }
}

impl Default for AndroidA11y {
    fn default() -> Self {
        Self::new()
    }
}

impl Accessibility for AndroidA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let window = ctx.window.clone();
        let adb = self.ensure_adb()?;
        let status = adb.run(["shell", "uiautomator", "dump", DUMP_PATH])?;
        check_dump_status(&status)?;
        let xml = adb.run(["shell", "cat", DUMP_PATH])?;
        build_tree(&xml, &window)
    }
}
