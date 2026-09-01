//! Sandboxie **Classic** containment provider (cfg(windows)).
//!
//! Drives Sandboxie via its CLI (`Start.exe` / `SbieIni.exe`) as subprocesses — no FFI,
//! no linking against Sandboxie. The recipe is the on-box-validated one:
//!
//! - Per-session box `glass_<pid>` configured via `SbieIni.exe set/append` from the pure
//!   policy in [`super::config`], plus the compat templates (without which PowerShell etc.
//!   break inside the box) and, for `strict`, a `ClosedFilePath \Device\Afd*` to belt the
//!   `AllowNetworkAccess=n` policy.
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

use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Armed by `configure()` once it begins writing this box's `Sandboxie.ini`
    /// section; disarmed by a successful `launch()` (which hands the clear to
    /// [`SandboxieApp::kill`]). While armed, dropping `Sandboxie` clears the
    /// section, so a failure anywhere between `configure()` and a successful
    /// `launch()` never orphans a per-session `glass_<pid>` section in the
    /// shared `Sandboxie.ini`.
    section_armed: AtomicBool,
}

/// Remove a box's entire config section from `Sandboxie.ini`
/// (`SbieIni set <box> * ""` — the maintainer's documented box-clear), so
/// per-session `glass_<pid>` boxes don't accumulate. Best-effort.
fn clear_box_section(dir: &str, box_name: &str) {
    let _ = Command::new(sbieini(dir))
        .args(["set", box_name, "*", ""])
        .status();
}

impl Drop for Sandboxie {
    fn drop(&mut self) {
        if self.section_armed.load(Ordering::Relaxed) {
            clear_box_section(&self.dir, &self.box_name);
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
        // This box's persistent ini section is about to exist; arm the guard so
        // any failure before a successful launch() clears it (see the struct doc).
        self.section_armed.store(true, Ordering::Relaxed);
        let dir = self.dir.clone();
        let sbieini = sbieini(&dir);

        // 1. strict global gate — never write [GlobalSettings], only read it.
        if level == SandboxLevel::Strict {
            let out = Command::new(&sbieini)
                .args(["query", "GlobalSettings", "PromptForInternetAccess"])
                .output()
                .map_err(|e| {
                    GlassError::SandboxUnavailable(format!(
                        "querying GlobalSettings PromptForInternetAccess: {e}"
                    ))
                })?;
            let value = String::from_utf8_lossy(&out.stdout)
                .trim()
                .to_ascii_lowercase();
            if value == "y" {
                return Err(GlassError::SandboxUnavailable(
                    "Sandboxie GlobalSettings PromptForInternetAccess=y would deadlock strict; \
                     set it to n, or use sandbox=default/off"
                        .into(),
                ));
            }
        }

        // 2. per-box policy.
        for (key, value) in config::box_settings(level) {
            self.run_sbie(&sbieini, &["set", &self.box_name, key, value])?;
        }

        // 3-5. Compatibility templates, strict AFD closure, then protected host paths.
        for append in box_appends(level, protected_paths) {
            let (key, value) = match append {
                BoxAppend::Template(value) => ("Template", value),
                BoxAppend::ClosedFilePath(value) => ("ClosedFilePath", value),
            };
            let status = Command::new(&sbieini)
                .arg("append")
                .arg(&self.box_name)
                .arg(key)
                .arg(&value)
                .status()
                .map_err(|e| {
                    clear_box_section(&self.dir, &self.box_name);
                    self.section_armed.store(false, Ordering::Relaxed);
                    GlassError::SandboxUnavailable(format!(
                        "installing protected host path in Sandboxie: {e}"
                    ))
                })?;
            if !status.success() {
                clear_box_section(&self.dir, &self.box_name);
                self.section_armed.store(false, Ordering::Relaxed);
                return Err(GlassError::SandboxUnavailable(format!(
                    "installing protected host path in Sandboxie failed with status {status}"
                )));
            }
        }

        // 6. Reload only after every closed path has been installed.
        if let Err(error) = self.run_sbie(&start_exe(&dir), &["/reload"]) {
            clear_box_section(&self.dir, &self.box_name);
            self.section_armed.store(false, Ordering::Relaxed);
            return Err(GlassError::SandboxUnavailable(format!(
                "reloading protected Sandboxie policy: {error}"
            )));
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
        let logdir_str = logdir.to_string_lossy().into_owned();
        self.run_sbie(
            &sbieini(&self.dir),
            &["set", &self.box_name, "OpenFilePath", &logdir_str],
        )?;
        self.run_sbie(&start_exe(&self.dir), &["/reload"])?;

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

    /// Tear the box down, ordered so no log line is lost and no tailer outlives teardown:
    /// `/terminate` (stop the box producing output) → signal stop → **join** the tailers
    /// (each does a final `drain()` of the real log files) → kill+reap the wrapper → stop
    /// the clipboard pipe server → remove the log dir (only now that the tailers have exited
    /// and read everything) → clear the box's config section so per-session `glass_<pid>`
    /// boxes don't accumulate in `Sandboxie.ini` (`SbieIni set <box> * ""` — the
    /// maintainer's documented box-clear).
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
        clear_box_section(&self.dir, &self.box_name);
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
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use glass_core::SandboxLevel;
    use glass_core::platform::ProtectedHostPath;

    use super::{BoxAppend, box_appends, closed_file_paths, validate_closed_path};

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
}
