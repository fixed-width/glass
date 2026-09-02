//! Sandboxie **Classic** containment provider (cfg(windows)).
//!
//! Drives Sandboxie via its CLI (`Start.exe` / `SbieIni.exe`) as subprocesses — no FFI,
//! no linking against Sandboxie. The recipe is the on-box-validated one:
//!
//! - Each fixed-width base-36 box gets [`super::config`] policy and compatibility templates.
//! - `strict` adds `ClosedFilePath \Device\Afd*` as a network-policy backstop.
//! - `strict` additionally gates on the **global** `PromptForInternetAccess`: a `y` there
//!   would deadlock a no-network box on a UI prompt, so we detect it and fail closed. We
//!   never write `[GlobalSettings]`.
//! - The build step runs UNCONFINED (host) at every level — only the launched run is contained.
//! - **Logs use a file fallback**: stdio pipes do NOT forward through `Start.exe`
//!   (gate-proven), so the app is launched via a generated `launch.cmd` that redirects its
//!   stdout/stderr to files in a per-session log dir glass owns, and reader threads tail
//!   those files into the `LogSink`.
//! - Discovery unions `Start.exe /listpids` with a Toolhelp descendant walk of the wrapper.
//! - Teardown is `Start.exe /box:<box> /terminate`, then the wrapper is reaped, tailers
//!   stopped, and the log dir removed.

use std::ffi::{OsStr, OsString};
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use glass_clip_shim_windows::store::PrivateClipboard;
use glass_core::logbuf::Stream;
use glass_core::platform::{ProtectedHostPath, ProtectedHostPathKind};
use glass_core::{AppSpec, Deadline, GlassError, Result, SandboxLevel};

use super::clip_server::ClipServer;
use super::config;
use super::imp::LogSink;

/// Compat templates appended to every glass box. REQUIRED — without these, common host
/// programs (PowerShell, etc.) fail to run inside the box.
pub(crate) const COMPAT_TEMPLATES: &[&str] = &["SkipHook", "FileCopy", "qWave", "LingerPrograms"];

#[derive(Clone, Debug, PartialEq, Eq)]
enum BoxAppend {
    Template(OsString),
    ClosedFilePath(OsString),
}

fn box_appends(level: SandboxLevel, protected_paths: &[ProtectedHostPath]) -> Vec<BoxAppend> {
    let mut appends = COMPAT_TEMPLATES
        .iter()
        .map(|template| BoxAppend::Template(OsString::from(template)))
        .collect::<Vec<_>>();
    if config::box_net(level).close_afd {
        appends.push(BoxAppend::ClosedFilePath(OsString::from(r"\Device\Afd*")));
    }
    appends.extend(
        closed_file_paths(level, protected_paths)
            .into_iter()
            .map(|path| BoxAppend::ClosedFilePath(path.into_os_string())),
    );
    appends
}

#[derive(Debug)]
struct CommandOutcome {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CommandOutcome {
    #[cfg(test)]
    fn success(stdout: &[u8]) -> Self {
        Self {
            success: true,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    #[cfg(test)]
    fn failure(stderr: &[u8]) -> Self {
        Self {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        }
    }
}

fn run_command(exe: &OsStr, args: &[OsString]) -> std::io::Result<CommandOutcome> {
    Command::new(exe)
        .args(args)
        .output()
        .map(|output| CommandOutcome {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
}

fn bounded_diagnostic(bytes: &[u8]) -> String {
    const MAX: usize = 256;
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX)])
        .trim()
        .to_owned()
}

fn require_success(
    runner: &mut impl FnMut(&OsStr, &[OsString]) -> std::io::Result<CommandOutcome>,
    exe: &OsStr,
    args: &[OsString],
) -> std::result::Result<(), String> {
    let outcome = runner(exe, args).map_err(|error| format!("Sandboxie command spawn: {error}"))?;
    if outcome.success {
        Ok(())
    } else {
        Err(format!(
            "Sandboxie command failed: {}",
            bounded_diagnostic(&outcome.stderr)
        ))
    }
}

fn mix_process_identity(pid: u32, process_nonce: u64) -> u64 {
    let pid = u64::from(pid);
    let mut value = process_nonce ^ (pid << 32 | pid);
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn fixed_base36(mut value: u64) -> String {
    const WIDTH: usize = 13;
    let mut encoded = [b'0'; WIDTH];
    for digit in encoded.iter_mut().rev() {
        let remainder = (value % 36) as u8;
        *digit = if remainder < 10 {
            b'0' + remainder
        } else {
            b'a' + remainder - 10
        };
        value /= 36;
    }
    encoded.into_iter().map(char::from).collect()
}

fn box_name_for(pid: u32, process_nonce: u64, attempt: u64) -> String {
    format!(
        "glass_{}{}",
        fixed_base36(mix_process_identity(pid, process_nonce)),
        fixed_base36(attempt)
    )
}

pub(crate) fn unique_box_name() -> String {
    static ATTEMPT: AtomicU64 = AtomicU64::new(0);
    static PROCESS_NONCE: OnceLock<u64> = OnceLock::new();
    let nonce = *PROCESS_NONCE.get_or_init(|| {
        let state = std::collections::hash_map::RandomState::new();
        state.build_hasher().finish()
    });
    box_name_for(
        std::process::id(),
        nonce,
        ATTEMPT.fetch_add(1, Ordering::Relaxed),
    )
}

/// Resolve the Sandboxie install directory: explicit (none) > env `GLASS_SANDBOXIE_DIR` >
/// registry probe > the Classic default install path.
pub(crate) fn sandboxie_dir() -> String {
    config::pick_path(
        None,
        std::env::var("GLASS_SANDBOXIE_DIR").ok().as_deref(),
        registry_dir().as_deref(),
        r"C:\Program Files\Sandboxie",
    )
}

/// Best-effort `HKLM\SOFTWARE\Sandboxie` `InstallLocation` probe via `reg query`. Returns
/// `None` on any failure (the default path then applies). Kept simple — no Win32 registry FFI.
fn registry_dir() -> Option<String> {
    let out = Command::new("reg")
        .args(["query", r"HKLM\SOFTWARE\Sandboxie", "/v", "InstallLocation"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // A line looks like: "    InstallLocation    REG_SZ    C:\Program Files\Sandboxie"
    for line in text.lines() {
        if let Some(idx) = line.find("REG_SZ") {
            let value = line[idx + "REG_SZ".len()..].trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn start_exe(dir: &str) -> String {
    format!(r"{dir}\Start.exe")
}

fn sbieini(dir: &str) -> String {
    format!(r"{dir}\SbieIni.exe")
}

/// Whether Sandboxie is usable right now: `Start.exe` present in `dir` AND both services
/// (`SbieSvc`, `SbieDrv`) running.
pub(crate) fn available(dir: &str) -> bool {
    Path::new(&start_exe(dir)).exists() && service_running("SbieSvc") && service_running("SbieDrv")
}

/// True if the named Windows service reports RUNNING (`sc query <name>` stdout contains
/// "RUNNING"). No FFI.
fn service_running(name: &str) -> bool {
    match Command::new("sc").args(["query", name]).output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains("RUNNING"),
        Err(_) => false,
    }
}

/// A configured Sandboxie box for one session.
pub(crate) struct Sandboxie {
    pub dir: String,
    pub box_name: String,
    /// Tracks responsibility for clearing this box's `Sandboxie.ini` section until launch transfers
    /// it to [`SandboxieApp::kill`].
    section_armed: AtomicBool,
}

fn clear_box_section_with(
    dir: &str,
    box_name: &str,
    runner: &mut impl FnMut(&OsStr, &[OsString]) -> std::io::Result<CommandOutcome>,
) -> Result<()> {
    let args = ["set", box_name, "*", ""].map(OsString::from);
    let outcome = runner(OsStr::new(&sbieini(dir)), &args).map_err(|error| {
        GlassError::SandboxUnavailable(format!("clearing owned Sandboxie box section: {error}"))
    })?;
    if !outcome.success {
        return Err(GlassError::SandboxUnavailable(format!(
            "clearing owned Sandboxie box section failed: {}",
            bounded_diagnostic(&outcome.stderr)
        )));
    }
    Ok(())
}

impl Drop for Sandboxie {
    fn drop(&mut self) {
        if self.section_armed.load(Ordering::Relaxed) {
            let mut runner = run_command;
            if clear_box_section_with(&self.dir, &self.box_name, &mut runner).is_ok() {
                self.section_armed.store(false, Ordering::Relaxed);
            }
        }
    }
}

impl Sandboxie {
    /// A box handle for `box_name` under install `dir`. The section guard starts
    /// disarmed; `configure()` arms it.
    pub(crate) fn new(dir: String, box_name: String) -> Self {
        Self {
            dir,
            box_name,
            section_armed: AtomicBool::new(false),
        }
    }

    /// Configure the private-clipboard hook for this box (Layer 2). Returns `Some((store, server,
    /// pipe))` when the hook DLL is resolvable and the pipe server starts; `None` (Layer-1-only)
    /// otherwise. Never fails the launch — a missing hook leaves the app clipboard-less but the
    /// user's clipboard safe (Layer 1 already applied via box_settings).
    fn setup_private_clipboard(&self) -> Option<(PrivateClipboard, ClipServer, String)> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()));
        let env = config::shim_dll_env(&|k| std::env::var(k).ok());
        let dll = config::shim_dll_path(env.as_deref(), exe_dir.as_deref(), &|p| p.exists())?;
        if !Path::new(&dll).exists() {
            crate::disclose_clip_disabled(&dll);
            return None;
        }
        let pipe = config::clip_pipe_name(&self.box_name);
        let store = PrivateClipboard::new();
        let server = ClipServer::start(
            &pipe,
            store.clone(),
            self.dir.clone(),
            self.box_name.clone(),
        )
        .ok()?;
        let sbieini = sbieini(&self.dir);
        for (k, v) in config::clip_layer2_lines(&self.box_name, &dll) {
            if self
                .run_sbie(&sbieini, &["set", &self.box_name, &k, &v])
                .is_err()
            {
                return None;
            }
        }
        if self.run_sbie(&start_exe(&self.dir), &["/reload"]).is_err() {
            return None;
        }
        Some((store, server, pipe))
    }

    /// Run a Sandboxie CLI tool, mapping a spawn failure or non-zero exit to `Backend`.
    fn run_sbie(&self, exe: &str, args: &[&str]) -> Result<()> {
        let status = Command::new(exe)
            .args(args)
            .status()
            .map_err(|e| GlassError::Backend(format!("spawn {exe}: {e}")))?;
        if !status.success() {
            return Err(GlassError::Backend(format!(
                "{exe} {args:?} failed with status {status}"
            )));
        }
        Ok(())
    }

    /// Configure the box for `level`: strict global gate first, then the policy `set` pairs,
    /// the compat templates, the strict AFD device close, and a `/reload`.
    #[cfg(test)]
    pub(crate) fn configure(&self, level: SandboxLevel) -> Result<()> {
        self.configure_with_paths(level, &[])
    }

    pub(crate) fn configure_with_paths(
        &self,
        level: SandboxLevel,
        protected_paths: &[ProtectedHostPath],
    ) -> Result<()> {
        self.configure_with_runner(level, protected_paths, run_command)
    }

    fn configure_with_runner(
        &self,
        level: SandboxLevel,
        protected_paths: &[ProtectedHostPath],
        mut runner: impl FnMut(&OsStr, &[OsString]) -> std::io::Result<CommandOutcome>,
    ) -> Result<()> {
        let protected_paths = validate_protected_host_paths(protected_paths)?;
        let sbieini = sbieini(&self.dir);

        if level == SandboxLevel::Strict {
            let args = ["query", "GlobalSettings", "PromptForInternetAccess"].map(OsString::from);
            let outcome = match runner(OsStr::new(&sbieini), &args) {
                Ok(outcome) if outcome.success => outcome,
                Ok(outcome) => {
                    return Err(self.configuration_failed(
                        format!(
                            "querying Sandboxie global network prompt setting failed: {}",
                            bounded_diagnostic(&outcome.stderr)
                        ),
                        &mut runner,
                    ));
                }
                Err(error) => {
                    return Err(self.configuration_failed(
                        format!("querying Sandboxie global network prompt setting: {error}"),
                        &mut runner,
                    ));
                }
            };
            let value = String::from_utf8_lossy(&outcome.stdout)
                .trim()
                .to_ascii_lowercase();
            if value == "y" {
                return Err(self.configuration_failed(
                    "Sandboxie global network prompt setting is incompatible with strict mode"
                        .into(),
                    &mut runner,
                ));
            }
        }

        clear_box_section_with(&self.dir, &self.box_name, &mut runner)?;
        self.section_armed.store(true, Ordering::Relaxed);

        for (key, value) in config::box_settings(level) {
            let args = ["set", &self.box_name, key, value].map(OsString::from);
            if let Err(error) = require_success(&mut runner, OsStr::new(&sbieini), &args) {
                return Err(self.configuration_failed(error, &mut runner));
            }
        }

        for append in box_appends(level, &protected_paths) {
            let (key, value) = match append {
                BoxAppend::Template(value) => ("Template", value),
                BoxAppend::ClosedFilePath(value) => ("ClosedFilePath", value),
            };
            let args = [
                OsString::from("append"),
                OsString::from(&self.box_name),
                OsString::from(key),
                value,
            ];
            if let Err(error) = require_success(&mut runner, OsStr::new(&sbieini), &args) {
                return Err(self.configuration_failed(error, &mut runner));
            }
        }

        let args = [OsString::from("/reload")];
        if let Err(error) = require_success(&mut runner, OsStr::new(&start_exe(&self.dir)), &args) {
            return Err(self.configuration_failed(error, &mut runner));
        }
        Ok(())
    }

    fn configuration_failed(
        &self,
        original: String,
        runner: &mut impl FnMut(&OsStr, &[OsString]) -> std::io::Result<CommandOutcome>,
    ) -> GlassError {
        if !self.section_armed.load(Ordering::Relaxed) {
            return GlassError::SandboxUnavailable(original);
        }
        match clear_box_section_with(&self.dir, &self.box_name, runner) {
            Ok(()) => {
                self.section_armed.store(false, Ordering::Relaxed);
                GlassError::SandboxUnavailable(original)
            }
            Err(cleanup) => GlassError::SandboxUnavailable(format!(
                "{original}; owned box cleanup will be retried: {cleanup}"
            )),
        }
    }

    #[cfg(test)]
    fn retry_clear_with_runner(
        &self,
        mut runner: impl FnMut(&OsStr, &[OsString]) -> std::io::Result<CommandOutcome>,
    ) -> Result<()> {
        clear_box_section_with(&self.dir, &self.box_name, &mut runner)?;
        self.section_armed.store(false, Ordering::Relaxed);
        Ok(())
    }

    #[cfg(test)]
    fn section_is_armed(&self) -> bool {
        self.section_armed.load(Ordering::Relaxed)
    }

    fn configure_logdir_with_runner(
        &self,
        logdir: &Path,
        mut runner: impl FnMut(&OsStr, &[OsString]) -> std::io::Result<CommandOutcome>,
    ) -> Result<()> {
        let args = [
            OsString::from("set"),
            OsString::from(&self.box_name),
            OsString::from("OpenFilePath"),
            logdir.as_os_str().to_owned(),
        ];
        if let Err(error) = require_success(&mut runner, OsStr::new(&sbieini(&self.dir)), &args) {
            return Err(self.configuration_failed(error, &mut runner));
        }

        let args = [OsString::from("/reload")];
        if let Err(error) = require_success(&mut runner, OsStr::new(&start_exe(&self.dir)), &args) {
            return Err(self.configuration_failed(error, &mut runner));
        }
        Ok(())
    }

    /// Launch the app contained, redirecting its stdio to files in a per-session log dir and
    /// tailing those files into `logs`. Returns the live handle.
    pub(crate) fn launch(&self, spec: &AppSpec, logs: LogSink) -> Result<SandboxieApp> {
        // Layer-2 clipboard: configure the hook DLL + pipe server (best-effort; None = Layer-1-only).
        // This writes clipboard ini lines and does a /reload BEFORE the logdir reload below,
        // so both sets of ini changes land before the app is ever spawned.
        let clip = self.setup_private_clipboard();
        let clip_pipe = clip.as_ref().map(|(_, _, p)| p.clone());

        let logdir = std::env::temp_dir().join(&self.box_name);
        std::fs::create_dir_all(&logdir).map_err(|e| {
            GlassError::AppNotStarted(format!("create log dir {}: {e}", logdir.display()))
        })?;

        // Allow the box to write to the log dir on the host, then reload.
        if let Err(error) = self.configure_logdir_with_runner(&logdir, run_command) {
            let _ = std::fs::remove_dir_all(&logdir);
            return Err(error);
        }

        let out_log = logdir.join("out.log");
        let err_log = logdir.join("err.log");

        // Generate launch.cmd: optional cd, then the quoted exe + args with stdio redirected.
        // Passes the clipboard pipe name (if Layer 2 is active) as GLASS_CLIP_PIPE env.
        let cmd_path = logdir.join("launch.cmd");
        let script =
            super::config::build_launch_cmd_env(spec, &out_log, &err_log, clip_pipe.as_deref())?;
        std::fs::write(&cmd_path, script)
            .map_err(|e| GlassError::AppNotStarted(format!("write {}: {e}", cmd_path.display())))?;

        // Spawn the Start.exe wrapper, reusing the Job wrapper for teardown of the launcher
        // process itself. `Off` so no Job caps are applied to the wrapper.
        let cmd_path_str = cmd_path.to_string_lossy().into_owned();
        let mut cmd = Command::new(start_exe(&self.dir));
        cmd.args([
            &format!("/box:{}", self.box_name),
            "cmd",
            "/c",
            &cmd_path_str,
        ]);
        let inner = crate::process::spawn_suspended_in_job(&mut cmd, SandboxLevel::Off)?;
        inner.resume();

        // Tail the redirected stdio files into the sink. Keep the JoinHandles so kill() can
        // join them (final drain) BEFORE removing the log dir — never detached.
        let stop = Arc::new(AtomicBool::new(false));
        let tailers = vec![
            spawn_tailer(out_log, Stream::Stdout, logs.clone(), stop.clone()),
            spawn_tailer(err_log, Stream::Stderr, logs.clone(), stop.clone()),
        ];

        // Launch succeeded: the box now belongs to SandboxieApp, whose kill()
        // clears the section. Disarm so our Drop doesn't wipe a live box.
        self.section_armed.store(false, Ordering::Relaxed);
        Ok(SandboxieApp {
            dir: self.dir.clone(),
            box_name: self.box_name.clone(),
            logdir,
            inner,
            stop,
            tailers,
            clip: clip.map(|(store, server, _)| (store, server)),
        })
    }
}

pub(crate) fn closed_file_paths(
    level: SandboxLevel,
    protected_paths: &[ProtectedHostPath],
) -> Vec<PathBuf> {
    if level == SandboxLevel::Off {
        return Vec::new();
    }
    protected_paths
        .iter()
        .map(|path| path.path.clone())
        .collect()
}

pub(crate) fn validate_protected_host_paths(
    paths: &[ProtectedHostPath],
) -> Result<Vec<ProtectedHostPath>> {
    paths
        .iter()
        .map(|protected| {
            validate_closed_path(&protected.path)?;
            let valid_kind = match protected.kind {
                ProtectedHostPathKind::Directory => {
                    crate::open_directory_no_reparse(&protected.path).is_ok()
                }
                ProtectedHostPathKind::File => std::fs::symlink_metadata(&protected.path)
                    .map(|metadata| metadata.is_file() && metadata.file_attributes() & 0x400 == 0)
                    .unwrap_or(false),
            };
            if !valid_kind {
                return Err(GlassError::SandboxUnavailable(
                    "protected host path is missing, substituted, or has the wrong kind".into(),
                ));
            }
            Ok(protected.clone())
        })
        .collect()
}

pub(crate) fn validate_closed_path(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.parent().is_none() {
        return Err(GlassError::SandboxUnavailable(
            "protected host path must be an absolute non-root Windows path".into(),
        ));
    }
    Ok(())
}

/// Tail `path` from a byte offset, ~100ms poll, splitting complete CRLF/LF lines and pushing
/// `(stream, line)` into `sink`. On `stop`, drains once more then returns. Tolerates the file
/// not existing yet. Returns the `JoinHandle` so the owner can join it (final drain) before
/// removing the log dir.
fn spawn_tailer(
    path: PathBuf,
    stream: Stream,
    sink: LogSink,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut offset: u64 = 0;
        let mut pending = String::new();
        loop {
            let stopping = stop.load(Ordering::Relaxed);
            offset = drain(&path, offset, &mut pending, stream, &sink);
            if stopping {
                // Final drain done above; flush any trailing partial line.
                let line = std::mem::take(&mut pending);
                let line = line.trim_end_matches(['\r', '\n']);
                if !line.is_empty()
                    && let Ok(mut g) = sink.lock()
                {
                    g.push((stream, line.to_string()));
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    })
}

/// Read any new bytes from `path` past `offset`, append to `pending`, emit each complete
/// line into `sink`, and return the new offset. A read error / missing file leaves the
/// offset unchanged.
fn drain(path: &Path, offset: u64, pending: &mut String, stream: Stream, sink: &LogSink) -> u64 {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return offset,
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return offset;
    }
    let mut buf = Vec::new();
    let read = match file.read_to_end(&mut buf) {
        Ok(n) => n,
        Err(_) => return offset,
    };
    if read == 0 {
        return offset;
    }
    pending.push_str(&String::from_utf8_lossy(&buf));
    // Emit every complete line (terminated by '\n'); keep the trailing partial.
    while let Some(nl) = pending.find('\n') {
        let line: String = pending.drain(..=nl).collect();
        let line = line.trim_end_matches(['\r', '\n']);
        if let Ok(mut g) = sink.lock() {
            g.push((stream, line.to_string()));
        }
    }
    offset + read as u64
}

/// A launched, Sandboxie-contained app.
pub(crate) struct SandboxieApp {
    dir: String,
    box_name: String,
    logdir: PathBuf,
    inner: crate::process::LaunchedApp,
    stop: Arc<AtomicBool>,
    tailers: Vec<std::thread::JoinHandle<()>>,
    /// Layer-2 private clipboard: the host store + its pipe server. `None` = Layer-1-only
    /// (app clipboard disabled, user protected).
    clip: Option<(PrivateClipboard, ClipServer)>,
}

impl SandboxieApp {
    /// The wrapper (`Start.exe`) process pid — the launcher glass spawned. The contained app
    /// itself runs under a separate Sandboxie-managed pid (see [`Self::pids_by`]).
    pub(crate) fn root_pid(&self) -> u32 {
        self.inner.pid()
    }

    /// The contained app's process set: `Start.exe /listpids` ∪ a Toolhelp descendant walk
    /// of the wrapper, deduped under the caller's absolute deadline.
    pub(crate) fn pids_by(&self, deadline: Deadline) -> Result<Vec<u32>> {
        super::pid_discovery::collect_pids_by_with(
            deadline,
            |received| match super::pid_discovery::sandboxie_list_pids_by(
                &start_exe(&self.dir),
                &self.box_name,
                received,
            ) {
                Ok(pids) => Ok(pids),
                Err(error) if error.bound().is_some() => Err(error),
                Err(_) => Ok(Vec::new()),
            },
            |received| crate::process::descendant_pids_by(self.inner.pid(), received),
        )
    }

    /// The class prefix Sandboxie stamps on this box's app windows (`Sandbox:<box>:`). Window
    /// discovery requires it under containment so it adopts the real boxed app window and skips
    /// glass's own interposed launcher console (which Sandboxie leaves as `ConsoleWindowClass`).
    pub(crate) fn adoption_class_prefix(&self) -> String {
        format!("Sandbox:{}:", self.box_name)
    }

    /// Always `Ok(None)`: the `Start.exe` wrapper exits right after handing off to the box, so
    /// its exit does not signal the app's; and a `std::process::ExitStatus` for the contained
    /// app cannot be synthesized here. Discovery relies on `pids_by()` / the start timeout instead.
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        Ok(None)
    }

    /// The clipboard routing for this contained app: `Some(store)` when Layer 2 is active,
    /// `None` when Layer-1-only (the platform turns `None` into the "disabled" error — it must
    /// never fall back to the user's real clipboard for a contained app).
    pub(crate) fn private_clipboard(&self) -> Option<PrivateClipboard> {
        self.clip.as_ref().map(|(store, _)| store.clone())
    }

    /// Terminates the box, joins tailers after their final drain, reaps helpers, removes logs, and
    /// clears the per-attempt `Sandboxie.ini` section.
    pub(crate) fn kill(mut self) -> crate::process::Closed {
        let _ = Command::new(start_exe(&self.dir))
            .args([&format!("/box:{}", self.box_name), "/terminate"])
            .status();
        self.stop.store(true, Ordering::Relaxed);
        for h in self.tailers {
            let _ = h.join();
        }
        // The wrapper's own teardown. Its outcome is not the app's — `Start.exe` exits as soon
        // as it has handed off, so this reports on a process the user never sees.
        let _ = self.inner.kill();
        if let Some((_, server)) = self.clip.take() {
            server.stop();
        }
        let _ = std::fs::remove_dir_all(&self.logdir);
        let mut runner = run_command;
        if let Err(error) = clear_box_section_with(&self.dir, &self.box_name, &mut runner) {
            eprintln!("glass: {error}");
        }
        // A boxed app is terminated without ever being asked to close, unlike an unconfined
        // one. Measured on-box: `PostMessageW(WM_CLOSE)` to a boxed app's top-level windows
        // *succeeds* — two posts accepted, none refused — and the app neither closes nor runs
        // its shutdown path, because Sandboxie filters window messages across the box boundary.
        // The request has to come from inside the box to land, which needs an in-box helper
        // process; until then a contained app records an unclean exit the same way every
        // teardown did before this path existed.
        crate::process::Closed::TerminatedUnasked { refused: 0 }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::windows::fs::{symlink_dir, symlink_file};
    use std::path::{Path, PathBuf};

    use glass_core::SandboxLevel;
    use glass_core::platform::ProtectedHostPath;

    use super::{
        BoxAppend, CommandOutcome, Sandboxie, box_appends, box_name_for, closed_file_paths,
        mix_process_identity, unique_box_name, validate_closed_path,
    };

    #[derive(Clone, Copy)]
    enum FailureKind {
        Spawn(io::ErrorKind),
        Status,
    }

    #[derive(Clone)]
    struct CommandFailure {
        exe: OsString,
        args: Vec<OsString>,
        kind: FailureKind,
        skip_matches: usize,
        remaining: usize,
    }

    #[derive(Default)]
    struct FakeRunner {
        commands: Vec<(OsString, Vec<OsString>)>,
        failures: Vec<CommandFailure>,
    }

    impl FakeRunner {
        fn fail(&mut self, exe: &str, args: &[&str], kind: FailureKind) {
            self.failures.push(CommandFailure {
                exe: OsString::from(exe),
                args: args.iter().map(OsString::from).collect(),
                kind,
                skip_matches: 0,
                remaining: 1,
            });
        }

        fn fail_after_match(&mut self, exe: &str, args: &[&str], kind: FailureKind) {
            self.failures.push(CommandFailure {
                exe: OsString::from(exe),
                args: args.iter().map(OsString::from).collect(),
                kind,
                skip_matches: 1,
                remaining: 1,
            });
        }

        fn run(&mut self, exe: &OsStr, args: &[OsString]) -> io::Result<CommandOutcome> {
            self.commands.push((exe.to_owned(), args.to_vec()));
            if let Some(failure) = self
                .failures
                .iter_mut()
                .find(|failure| failure.remaining > 0 && failure.exe == exe && failure.args == args)
            {
                if failure.skip_matches > 0 {
                    failure.skip_matches -= 1;
                    return Ok(CommandOutcome::success(b""));
                }
                failure.remaining -= 1;
                return match failure.kind {
                    FailureKind::Spawn(kind) => Err(io::Error::new(kind, "injected")),
                    FailureKind::Status => Ok(CommandOutcome::failure(b"injected status")),
                };
            }
            Ok(CommandOutcome::success(
                if args.first().is_some_and(|arg| arg == "query") {
                    b"n"
                } else {
                    b""
                },
            ))
        }

        fn arguments(&self) -> Vec<Vec<String>> {
            self.commands
                .iter()
                .map(|(_, args)| {
                    args.iter()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect()
                })
                .collect()
        }
    }

    fn clear_command() -> Vec<String> {
        vec!["set", "box", "*", ""]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn default_policy_prefix() -> Vec<Vec<String>> {
        vec![
            clear_command(),
            vec!["set", "box", "Enabled", "y"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec!["set", "box", "KeepTokenIntegrity", "y"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec!["set", "box", "NotifyInternetAccessDenied", "n"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec!["set", "box", "NotifyStartRunAccessDenied", "n"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec!["set", "box", "AllowNetworkAccess", "y"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec!["set", "box", "OpenClipboard", "n"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        ]
    }

    fn protected_paths() -> Vec<ProtectedHostPath> {
        vec![
            ProtectedHostPath::directory(PathBuf::from(
                r"C:\Users\u\AppData\Local\glass\artifacts\server-a",
            )),
            ProtectedHostPath::file(PathBuf::from(
                r"C:\Users\u\AppData\Local\glass\artifacts\server-a.lease",
            )),
        ]
    }

    #[test]
    fn protected_paths_become_closed_file_path_entries_for_default_and_strict() {
        let paths = protected_paths();
        let expected = vec![
            PathBuf::from(r"C:\Users\u\AppData\Local\glass\artifacts\server-a"),
            PathBuf::from(r"C:\Users\u\AppData\Local\glass\artifacts\server-a.lease"),
        ];

        for level in [SandboxLevel::Default, SandboxLevel::Strict] {
            assert_eq!(closed_file_paths(level, &paths), expected);
        }
    }

    #[test]
    fn relative_windows_closed_path_fails_closed() {
        assert!(validate_closed_path(Path::new(r"relative\artifact")).is_err());
    }

    #[test]
    fn windows_root_closed_path_fails_closed() {
        assert!(validate_closed_path(Path::new(r"C:\")).is_err());
    }

    #[test]
    fn protected_closed_paths_follow_templates_and_strict_afd_closure() {
        let paths = protected_paths();
        let default = box_appends(SandboxLevel::Default, &paths);
        let strict = box_appends(SandboxLevel::Strict, &paths);
        let templates = ["SkipHook", "FileCopy", "qWave", "LingerPrograms"]
            .map(|value| BoxAppend::Template(OsString::from(value)));
        let protected = [
            r"C:\Users\u\AppData\Local\glass\artifacts\server-a",
            r"C:\Users\u\AppData\Local\glass\artifacts\server-a.lease",
        ]
        .map(|value| BoxAppend::ClosedFilePath(OsString::from(value)));

        assert_eq!(
            default,
            templates
                .clone()
                .into_iter()
                .chain(protected.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            strict,
            templates
                .into_iter()
                .chain([BoxAppend::ClosedFilePath(OsString::from(r"\Device\Afd*"))])
                .chain(protected)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn strict_query_nonzero_fails_before_any_box_mutation() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();
        runner.fail(
            r"S\SbieIni.exe",
            &["query", "GlobalSettings", "PromptForInternetAccess"],
            FailureKind::Status,
        );

        let error = sandboxie
            .configure_with_runner(SandboxLevel::Strict, &[], |exe, args| runner.run(exe, args))
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        assert_eq!(runner.commands.len(), 1);
    }

    #[test]
    fn strict_query_spawn_failure_fails_before_any_box_mutation() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();
        runner.fail(
            r"S\SbieIni.exe",
            &["query", "GlobalSettings", "PromptForInternetAccess"],
            FailureKind::Spawn(io::ErrorKind::NotFound),
        );

        let error = sandboxie
            .configure_with_runner(SandboxLevel::Strict, &[], |exe, args| runner.run(exe, args))
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        assert_eq!(runner.commands.len(), 1);
        assert!(!sandboxie.section_is_armed());
    }

    #[test]
    fn append_spawn_failure_clears_and_stops_commands() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();
        runner.fail(
            r"S\SbieIni.exe",
            &["append", "box", "Template", "SkipHook"],
            FailureKind::Spawn(io::ErrorKind::NotFound),
        );

        let error = sandboxie
            .configure_with_runner(SandboxLevel::Default, &[], |exe, args| {
                runner.run(exe, args)
            })
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        let mut expected = default_policy_prefix();
        expected.push(
            vec!["append", "box", "Template", "SkipHook"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        expected.push(clear_command());
        assert_eq!(runner.arguments(), expected);
        assert!(!sandboxie.section_is_armed());
    }

    #[test]
    fn append_nonzero_clears_and_stops_commands() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();
        runner.fail(
            r"S\SbieIni.exe",
            &["append", "box", "Template", "SkipHook"],
            FailureKind::Status,
        );

        let error = sandboxie
            .configure_with_runner(SandboxLevel::Default, &[], |exe, args| {
                runner.run(exe, args)
            })
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        let mut expected = default_policy_prefix();
        expected.push(
            vec!["append", "box", "Template", "SkipHook"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        expected.push(clear_command());
        assert_eq!(runner.arguments(), expected);
    }

    #[test]
    fn reload_spawn_failure_clears_and_stops_commands() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();
        runner.fail(
            r"S\Start.exe",
            &["/reload"],
            FailureKind::Spawn(io::ErrorKind::NotFound),
        );

        let error = sandboxie
            .configure_with_runner(SandboxLevel::Default, &[], |exe, args| {
                runner.run(exe, args)
            })
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        let mut expected = default_policy_prefix();
        for template in ["SkipHook", "FileCopy", "qWave", "LingerPrograms"] {
            expected.push(
                vec!["append", "box", "Template", template]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            );
        }
        expected.push(vec!["/reload"].into_iter().map(str::to_owned).collect());
        expected.push(clear_command());
        assert_eq!(runner.arguments(), expected);
    }

    #[test]
    fn successful_default_configuration_has_exact_command_order() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();

        sandboxie
            .configure_with_runner(SandboxLevel::Default, &[], |exe, args| {
                runner.run(exe, args)
            })
            .unwrap();

        let arguments = runner.arguments();
        assert_eq!(
            arguments,
            vec![
                vec!["set", "box", "*", ""],
                vec!["set", "box", "Enabled", "y"],
                vec!["set", "box", "KeepTokenIntegrity", "y"],
                vec!["set", "box", "NotifyInternetAccessDenied", "n"],
                vec!["set", "box", "NotifyStartRunAccessDenied", "n"],
                vec!["set", "box", "AllowNetworkAccess", "y"],
                vec!["set", "box", "OpenClipboard", "n"],
                vec!["append", "box", "Template", "SkipHook"],
                vec!["append", "box", "Template", "FileCopy"],
                vec!["append", "box", "Template", "qWave"],
                vec!["append", "box", "Template", "LingerPrograms"],
                vec!["/reload"],
            ]
        );
    }

    #[test]
    fn successful_strict_configuration_queries_then_preclears_before_policy() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();

        sandboxie
            .configure_with_runner(SandboxLevel::Strict, &[], |exe, args| runner.run(exe, args))
            .unwrap();

        assert_eq!(
            &runner.arguments()[..3],
            &[
                vec!["query", "GlobalSettings", "PromptForInternetAccess"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                clear_command(),
                vec!["set", "box", "Enabled", "y"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            ]
        );
    }

    #[test]
    fn reload_nonzero_and_failed_clear_leave_armed_for_retry() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();
        runner.fail(r"S\Start.exe", &["/reload"], FailureKind::Status);
        runner.fail_after_match(
            r"S\SbieIni.exe",
            &["set", "box", "*", ""],
            FailureKind::Status,
        );

        let error = sandboxie
            .configure_with_runner(SandboxLevel::Default, &[], |exe, args| {
                runner.run(exe, args)
            })
            .unwrap_err();
        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        assert!(sandboxie.section_is_armed());

        sandboxie
            .retry_clear_with_runner(|exe, args| runner.run(exe, args))
            .unwrap();
        assert!(!sandboxie.section_is_armed());
        assert_eq!(runner.arguments().last(), Some(&clear_command()));
    }

    #[test]
    fn box_names_are_unique_per_attempt() {
        assert_ne!(unique_box_name(), unique_box_name());
    }

    #[test]
    fn box_names_differ_across_process_generators_with_reused_pid_and_attempt() {
        let first = box_name_for(412, 0x1234, 0);
        let second = box_name_for(412, 0x5678, 0);

        assert_ne!(first, second);
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(
            second
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        );
    }

    #[test]
    fn maximum_inputs_fit_classic_box_name_limit_and_alphabet() {
        let name = box_name_for(u32::MAX, u64::MAX, u64::MAX);

        assert!(name.starts_with("glass_"));
        assert!(name.len() <= 32, "{} characters: {name}", name.len());
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn box_name_width_is_fixed_at_the_32_character_compatibility_limit() {
        for (pid, nonce, attempt) in [
            (0, 0, 0),
            (1, 1, 1),
            (u32::MAX, 0, u64::MAX),
            (u32::MAX, u64::MAX, 0),
        ] {
            assert_eq!(box_name_for(pid, nonce, attempt).len(), 32);
        }
    }

    #[test]
    fn wrapped_attempt_pair_remains_distinct() {
        let before_wrap = box_name_for(u32::MAX, u64::MAX, u64::MAX);
        let after_wrap = box_name_for(u32::MAX, u64::MAX, 0);

        assert_ne!(before_wrap, after_wrap);
    }

    #[test]
    fn different_attempts_produce_different_names() {
        assert_ne!(box_name_for(412, 0x1234, 7), box_name_for(412, 0x1234, 8));
    }

    #[test]
    fn process_identity_mix_responds_to_pid_and_nonce_boundaries() {
        assert_ne!(
            mix_process_identity(0, u64::MAX),
            mix_process_identity(u32::MAX, u64::MAX)
        );
        assert_ne!(
            mix_process_identity(u32::MAX, 0),
            mix_process_identity(u32::MAX, u64::MAX)
        );
    }

    #[test]
    fn preclear_spawn_failure_stops_before_policy_mutation() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();
        runner.fail(
            r"S\SbieIni.exe",
            &["set", "box", "*", ""],
            FailureKind::Spawn(io::ErrorKind::NotFound),
        );

        let error = sandboxie
            .configure_with_runner(SandboxLevel::Default, &[], |exe, args| {
                runner.run(exe, args)
            })
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        assert_eq!(runner.arguments(), vec![clear_command()]);
        assert!(!sandboxie.section_is_armed());
    }

    #[test]
    fn preclear_nonzero_stops_before_policy_mutation() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();
        runner.fail(
            r"S\SbieIni.exe",
            &["set", "box", "*", ""],
            FailureKind::Status,
        );

        let error = sandboxie
            .configure_with_runner(SandboxLevel::Default, &[], |exe, args| {
                runner.run(exe, args)
            })
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        assert_eq!(runner.arguments(), vec![clear_command()]);
        assert!(!sandboxie.section_is_armed());
    }

    #[test]
    fn logdir_set_spawn_failure_clears_and_stops_before_reload() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        sandboxie
            .section_armed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut runner = FakeRunner::default();
        runner.fail(
            r"S\SbieIni.exe",
            &["set", "box", "OpenFilePath", r"C:\logs"],
            FailureKind::Spawn(io::ErrorKind::NotFound),
        );

        let error = sandboxie
            .configure_logdir_with_runner(Path::new(r"C:\logs"), |exe, args| runner.run(exe, args))
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        assert_eq!(
            runner.arguments(),
            vec![
                vec!["set", "box", "OpenFilePath", r"C:\logs"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                clear_command(),
            ]
        );
        assert!(!sandboxie.section_is_armed());
    }

    #[test]
    fn logdir_set_nonzero_clears_and_stops_before_reload() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        sandboxie
            .section_armed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut runner = FakeRunner::default();
        runner.fail(
            r"S\SbieIni.exe",
            &["set", "box", "OpenFilePath", r"C:\logs"],
            FailureKind::Status,
        );

        let error = sandboxie
            .configure_logdir_with_runner(Path::new(r"C:\logs"), |exe, args| runner.run(exe, args))
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        assert_eq!(
            runner.arguments(),
            vec![
                vec!["set", "box", "OpenFilePath", r"C:\logs"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                clear_command(),
            ]
        );
    }

    #[test]
    fn logdir_reload_spawn_failure_clears_and_stops() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        sandboxie
            .section_armed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut runner = FakeRunner::default();
        runner.fail(
            r"S\Start.exe",
            &["/reload"],
            FailureKind::Spawn(io::ErrorKind::NotFound),
        );

        let error = sandboxie
            .configure_logdir_with_runner(Path::new(r"C:\logs"), |exe, args| runner.run(exe, args))
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        assert_eq!(
            runner.arguments(),
            vec![
                vec!["set", "box", "OpenFilePath", r"C:\logs"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                vec!["/reload"].into_iter().map(str::to_owned).collect(),
                clear_command(),
            ]
        );
    }

    #[test]
    fn logdir_reload_nonzero_clears_and_stops() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        sandboxie
            .section_armed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut runner = FakeRunner::default();
        runner.fail(r"S\Start.exe", &["/reload"], FailureKind::Status);

        let error = sandboxie
            .configure_logdir_with_runner(Path::new(r"C:\logs"), |exe, args| runner.run(exe, args))
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        assert_eq!(
            runner.arguments(),
            vec![
                vec!["set", "box", "OpenFilePath", r"C:\logs"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                vec!["/reload"].into_iter().map(str::to_owned).collect(),
                clear_command(),
            ]
        );
    }

    #[test]
    fn successful_logdir_configuration_sets_path_then_reloads() {
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        sandboxie
            .section_armed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut runner = FakeRunner::default();

        sandboxie
            .configure_logdir_with_runner(Path::new(r"C:\logs"), |exe, args| runner.run(exe, args))
            .unwrap();

        assert_eq!(
            runner.arguments(),
            vec![
                vec!["set", "box", "OpenFilePath", r"C:\logs"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
                vec!["/reload"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            ]
        );
        assert!(sandboxie.section_is_armed());
    }

    #[test]
    fn launch_boundary_rejects_paths_mutated_after_initial_validation_without_commands() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("process");
        let lease = root.path().join("process.lease");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(&lease, "lease").unwrap();
        let paths = vec![
            ProtectedHostPath::directory(directory.clone()),
            ProtectedHostPath::file(lease.clone()),
        ];
        super::validate_protected_host_paths(&paths).unwrap();
        std::fs::remove_file(&lease).unwrap();
        let sandboxie = Sandboxie::new("S".into(), "box".into());
        let mut runner = FakeRunner::default();

        let error = sandboxie
            .configure_with_runner(SandboxLevel::Default, &paths, |exe, args| {
                runner.run(exe, args)
            })
            .unwrap_err();

        assert!(matches!(
            error,
            glass_core::GlassError::SandboxUnavailable(_)
        ));
        assert!(runner.commands.is_empty());
    }

    #[test]
    fn launch_boundary_rejects_kind_and_reparse_substitutions_without_commands() {
        for mutation in [
            "directory-as-file",
            "file-as-directory",
            "directory-reparse",
            "file-reparse",
        ] {
            let root = tempfile::tempdir().unwrap();
            let directory = root.path().join("process");
            let lease = root.path().join("process.lease");
            let target_dir = root.path().join("target-dir");
            let target_file = root.path().join("target-file");
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(&lease, "lease").unwrap();
            std::fs::create_dir(&target_dir).unwrap();
            std::fs::write(&target_file, "target").unwrap();
            let paths = vec![
                ProtectedHostPath::directory(directory.clone()),
                ProtectedHostPath::file(lease.clone()),
            ];
            super::validate_protected_host_paths(&paths).unwrap();
            match mutation {
                "directory-as-file" => {
                    std::fs::remove_dir(&directory).unwrap();
                    std::fs::write(&directory, "file").unwrap();
                }
                "file-as-directory" => {
                    std::fs::remove_file(&lease).unwrap();
                    std::fs::create_dir(&lease).unwrap();
                }
                "directory-reparse" => {
                    std::fs::remove_dir(&directory).unwrap();
                    symlink_dir(&target_dir, &directory).unwrap();
                }
                "file-reparse" => {
                    std::fs::remove_file(&lease).unwrap();
                    symlink_file(&target_file, &lease).unwrap();
                }
                _ => unreachable!(),
            }
            let sandboxie = Sandboxie::new("S".into(), "box".into());
            let mut runner = FakeRunner::default();
            let error = sandboxie
                .configure_with_runner(SandboxLevel::Default, &paths, |exe, args| {
                    runner.run(exe, args)
                })
                .unwrap_err();
            assert!(
                matches!(error, glass_core::GlassError::SandboxUnavailable(_)),
                "{mutation}"
            );
            assert!(runner.commands.is_empty(), "{mutation}");
        }
    }
}
