use std::process::{Command, Output};

use glass_core::{GlassError, Result};

/// A thin wrapper over the `adb` binary, targeting one device serial.
#[derive(Clone, Debug)]
pub struct Adb {
    bin: String,
    serial: Option<String>,
}

impl Adb {
    /// Resolve the `adb` binary: `GLASS_ADB`, else `$SDK/platform-tools/adb` from a
    /// discovered SDK root (env or a common install location), else `"adb"` on `PATH`.
    /// Serial is unset until a target resolves it.
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok();
        let bin = crate::sdk::resolve_adb(&get, &|p| p.exists()).bin();
        Self { bin, serial: None }
    }

    /// Return a copy bound to `serial`.
    pub fn with_serial(&self, serial: impl Into<String>) -> Self {
        Self { bin: self.bin.clone(), serial: Some(serial.into()) }
    }

    pub fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }

    /// The resolved adb binary, for callers spawning their own long-lived adb process.
    pub fn bin(&self) -> &str {
        &self.bin
    }

    /// Run adb with captured text stdout; `Backend` error on spawn failure or non-zero exit.
    pub fn run<'a, I>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let argv = build_argv(self.serial.as_deref(), &args.into_iter().collect::<Vec<_>>());
        let out = self.spawn(&argv)?;
        decode_text(&self.bin, &argv, out)
    }

    /// Run adb capturing raw stdout bytes (e.g. `exec-out screencap`).
    pub fn run_bytes<'a, I>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let argv = build_argv(self.serial.as_deref(), &args.into_iter().collect::<Vec<_>>());
        let out = self.spawn(&argv)?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(exit_error(&self.bin, &argv, &out))
        }
    }

    fn spawn(&self, argv: &[String]) -> Result<Output> {
        Command::new(&self.bin)
            .args(argv)
            .output()
            .map_err(|e| GlassError::Backend(format!("failed to run `{}`: {e}", self.bin)))
    }
}

/// Build the adb argument vector, prefixing `-s <serial>` when targeting a device.
/// Pure, so it is unit-tested without invoking adb.
pub(crate) fn build_argv(serial: Option<&str>, args: &[&str]) -> Vec<String> {
    let mut v = Vec::with_capacity(args.len() + 2);
    if let Some(s) = serial {
        v.push("-s".to_string());
        v.push(s.to_string());
    }
    v.extend(args.iter().map(|a| a.to_string()));
    v
}

fn decode_text(bin: &str, argv: &[String], out: Output) -> Result<String> {
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(exit_error(bin, argv, &out))
    }
}

fn exit_error(bin: &str, argv: &[String], out: &Output) -> GlassError {
    GlassError::Backend(format!(
        "`{bin} {}` failed: {}",
        argv.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::build_argv;

    #[test]
    fn argv_without_serial_is_passthrough() {
        assert_eq!(build_argv(None, &["devices"]), vec!["devices".to_string()]);
    }

    #[test]
    fn argv_with_serial_prefixes_dash_s() {
        assert_eq!(
            build_argv(Some("emulator-5554"), &["shell", "echo", "hi"]),
            vec!["-s", "emulator-5554", "shell", "echo", "hi"]
                .into_iter().map(String::from).collect::<Vec<_>>()
        );
    }
}
