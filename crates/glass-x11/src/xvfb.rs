//! A private headless `Xvfb` the X11 backend spawns when no display is given,
//! so the default path is isolated and never touches the user's real desktop.
//! Uses `-displayfd`: the server picks a free display and reports it once ready,
//! avoiding display-number and readiness races.

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};

use glass_core::{GlassError, Result};

pub struct Xvfb {
    child: Child,
    /// The chosen display, formatted `:N`.
    pub display: String,
    // Held open for the server's lifetime so Xvfb never gets SIGPIPE on the fd.
    #[allow(dead_code)]
    displayfd: ChildStdout,
}

impl Xvfb {
    /// Spawn a private Xvfb on a server-chosen free display, returning once it is
    /// ready. `screen` is a `WxHxDepth` string (e.g. `"1280x800x24"`).
    pub fn start(screen: &str) -> Result<Xvfb> {
        let xvfb = glass_core::tool_path("GLASS_XVFB", "Xvfb");
        let mut child = Command::new(&xvfb)
            .args(["-displayfd", "1", "-screen", "0", screen])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                GlassError::Backend(format!(
                    "could not spawn {xvfb} ({e}); install it (e.g. `apt install xvfb`), \
                     set GLASS_XVFB to its path, or set GLASS_DISPLAY=:N to attach to an \
                     existing display"
                ))
            })?;

        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GlassError::Backend(
                "Xvfb exited without reporting a display (failed to start)".into(),
            ));
        }
        let num: u32 = match line.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GlassError::Backend(format!(
                    "unexpected Xvfb -displayfd output: {line:?}"
                )));
            }
        };

        Ok(Xvfb { child, display: format!(":{num}"), displayfd: reader.into_inner() })
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // SIGKILL doesn't let Xvfb remove its own lock/socket; clean them up.
        if let Some(num) = self.display.strip_prefix(':') {
            let _ = std::fs::remove_file(format!("/tmp/.X{num}-lock"));
            let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{num}"));
        }
    }
}
