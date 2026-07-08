//! `IosPlatform`: the `glass_core::Platform` implementation that drives a single
//! foreground app on an iOS Simulator over `xcrun simctl`.

use std::io::Write;
use std::process::{Command, Stdio};

use glass_core::{
    AppSpec, Frame, GlassError, KeyEvent, Platform, PointerEvent, Region, Result, Stream,
    WindowGeometry, WindowId, WindowInfo, WindowOp,
};

use crate::capture::screenshot;
use crate::logs::LogStream;
use crate::simctl::Simctl;
use crate::target::{SimTarget, SimulatorRegistry};

/// The single foreground app this backend drives.
struct RunningApp {
    bundle_id: String,
    geometry: WindowGeometry,
    logs: LogStream,
}

/// Drives a native app on an iOS Simulator over `xcrun simctl`. A single foreground app at a
/// time, reported as one fullscreen window; there is no window management to speak of.
pub struct IosPlatform {
    simctl: Simctl,
    udid: String,
    app: Option<RunningApp>,
}

/// Split `spec.run` into `(optional install path, bundle id)`. `run[0]` naming a path (it
/// ends in `.app` or contains a `/`) is installed via `simctl install`, with its bundle id
/// read from the bundle's `Info.plist`; otherwise `run[0]` is used directly as the bundle id
/// of an app already installed on the simulator.
pub(crate) fn bundle_id_from_run(run: &[String]) -> Result<(Option<String>, String)> {
    let first = run
        .first()
        .ok_or_else(|| GlassError::Backend("cannot start app: run command is empty".into()))?;
    if first.ends_with(".app") || first.contains('/') {
        let plist = format!("{first}/Info.plist");
        let out = Command::new("plutil")
            .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-", &plist])
            .output()
            .map_err(|e| GlassError::Backend(format!("plutil: {e}")))?;
        if !out.status.success() {
            return Err(GlassError::Backend(format!(
                "could not read CFBundleIdentifier from {plist}"
            )));
        }
        let bundle_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((Some(first.clone()), bundle_id))
    } else {
        Ok((None, first.clone()))
    }
}

impl IosPlatform {
    /// Resolve (attaching to or booting) a simulator per the `GLASS_IOS_*` env vars.
    pub fn from_env(reg: &SimulatorRegistry) -> Result<Self> {
        let target = SimTarget::from_env(reg)?;
        Ok(Self {
            simctl: target.simctl().clone(),
            udid: target.udid().to_string(),
            app: None,
        })
    }

    fn running(&self) -> Result<&RunningApp> {
        self.app.as_ref().ok_or(GlassError::NoActiveSession)
    }
}

impl Platform for IosPlatform {
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        if let Some(build) = &spec.build {
            let status = Command::new("sh")
                .arg("-c")
                .arg(build)
                .status()
                .map_err(|e| GlassError::Backend(format!("build step: {e}")))?;
            if !status.success() {
                return Err(GlassError::Backend("build step failed".into()));
            }
        }

        let (install, bundle_id) = bundle_id_from_run(&spec.run)?;
        if let Some(path) = install {
            self.simctl.run(&["install", &self.udid, &path])?;
        }
        self.simctl.run(&[
            "launch",
            "--terminate-running-process",
            &self.udid,
            &bundle_id,
        ])?;

        // Capture once, purely to learn the device's pixel dimensions for the geometry we report.
        let frame = screenshot(&self.simctl, &self.udid)?;
        let geometry = WindowGeometry {
            x: 0,
            y: 0,
            width: frame.width,
            height: frame.height,
        };
        self.app = Some(RunningApp {
            bundle_id,
            geometry: geometry.clone(),
            logs: LogStream::spawn(&self.udid),
        });
        Ok(geometry)
    }

    fn stop_app(&mut self) -> Result<()> {
        if let Some(app) = self.app.take() {
            let _ = self.simctl.run(&["terminate", &self.udid, &app.bundle_id]);
        }
        Ok(())
    }

    fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
        let frame = screenshot(&self.simctl, &self.udid)?;
        match region {
            None => Ok(frame),
            Some(r) => frame.crop(r),
        }
    }

    fn send_pointer(&mut self, _event: &PointerEvent) -> Result<()> {
        Err(GlassError::Unsupported(
            "pointer input on the iOS backend".into(),
        ))
    }

    fn send_key(&mut self, _event: &KeyEvent) -> Result<()> {
        Err(GlassError::Unsupported(
            "keyboard input on the iOS backend".into(),
        ))
    }

    fn get_clipboard(&mut self) -> Result<String> {
        self.simctl.run(&["pbpaste", &self.udid])
    }

    fn set_clipboard(&mut self, text: &str) -> Result<()> {
        // `simctl pbcopy` reads the clipboard text from stdin, so it needs a piped child
        // rather than the `Simctl::run` one-shot-output helper.
        let mut child = Command::new(self.simctl.program())
            .args(self.simctl.full_args(&["pbcopy", &self.udid]))
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| GlassError::Backend(format!("pbcopy: {e}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| GlassError::Backend("pbcopy: failed to open stdin".into()))?;
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| GlassError::Backend(format!("pbcopy write: {e}")))?;
        // Close our end so `pbcopy` sees EOF and exits; holding it open past this point
        // would deadlock the `wait()` below.
        drop(stdin);
        let status = child
            .wait()
            .map_err(|e| GlassError::Backend(format!("pbcopy wait: {e}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(GlassError::Backend("pbcopy failed".into()))
        }
    }

    fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
        let geometry = self.running()?.geometry.clone();
        match op {
            WindowOp::Geometry | WindowOp::Focus => Ok(geometry),
            WindowOp::Resize { .. } | WindowOp::Move { .. } => Err(GlassError::Unsupported(
                "window resize/move (iOS Simulator apps are fullscreen)".into(),
            )),
        }
    }

    fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        let app = self.running()?;
        Ok(vec![WindowInfo {
            id: WindowId(1),
            title: Some(app.bundle_id.clone()),
            class: None,
            geometry: app.geometry.clone(),
            active: true,
        }])
    }

    fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
        if id == WindowId(1) {
            Ok(self.running()?.geometry.clone())
        } else {
            Err(GlassError::WindowNotFound)
        }
    }

    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        self.app
            .as_ref()
            .map(|a| a.logs.drain())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_bundle_id_passthrough() {
        // A reverse-DNS bundle id (no path) is used directly, nothing to install.
        let (install, bid) = bundle_id_from_run(&["tech.fixedwidth.demo".into()]).unwrap();
        assert_eq!(install, None);
        assert_eq!(bid, "tech.fixedwidth.demo");
    }

    #[test]
    fn run_empty_is_error() {
        assert!(bundle_id_from_run(&[]).is_err());
    }
}
