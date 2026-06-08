//! Xvfb harness (same approach as glass-testapp): the MCP server connects to an
//! X display at startup, so the smoke test gives it a private Xvfb.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};

pub struct Xvfb {
    child: Child,
    pub display: String,
    _displayfd: ChildStdout,
}

impl Xvfb {
    pub fn start() -> Xvfb {
        let mut child = Command::new("Xvfb")
            .args(["-displayfd", "1", "-screen", "0", "1024x768x24"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("could not spawn Xvfb (is it installed?): {e}"));
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Xvfb exited without reporting a display");
        }
        let num: u32 = line.trim().parse().unwrap_or_else(|_| {
            let _ = child.kill();
            panic!("unexpected Xvfb -displayfd output: {line:?}");
        });
        Xvfb { child, display: format!(":{num}"), _displayfd: reader.into_inner() }
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(num) = self.display.strip_prefix(':') {
            let _ = std::fs::remove_file(format!("/tmp/.X{num}-lock"));
            let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{num}"));
        }
    }
}
