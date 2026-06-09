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
//! - Build runs contained (`Start.exe /box:<box> /wait cmd /c <build>`).
//! - **Logs use a file fallback**: stdio pipes do NOT forward through `Start.exe`
//!   (gate-proven), so the app is launched via a generated `launch.cmd` that redirects its
//!   stdout/stderr to files in a per-session log dir glass owns, and reader threads tail
//!   those files into the `LogSink`.
//! - Discovery unions `Start.exe /listpids` with a Toolhelp descendant walk of the wrapper.
//! - Teardown is `Start.exe /box:<box> /terminate`, then the wrapper is reaped, tailers
//!   stopped, and the log dir removed.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use glass_clip_hook::store::PrivateClipboard;
use glass_core::logbuf::Stream;
use glass_core::{AppSpec, GlassError, Result, SandboxLevel};

use super::clip_server::ClipServer;
use super::config;
use super::imp::LogSink;

/// Compat templates appended to every glass box. REQUIRED — without these, common host
/// programs (PowerShell, etc.) fail to run inside the box.
pub(crate) const COMPAT_TEMPLATES: &[&str] = &["SkipHook", "FileCopy", "qWave", "LingerPrograms"];

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
        .args([
            "query",
            r"HKLM\SOFTWARE\Sandboxie",
            "/v",
            "InstallLocation",
        ])
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
    Path::new(&start_exe(dir)).exists()
        && service_running("SbieSvc")
        && service_running("SbieDrv")
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
}

impl Sandboxie {
    /// Configure the private-clipboard hook for this box (Layer 2). Returns `Some((store, server,
    /// pipe))` when the hook DLL is resolvable and the pipe server starts; `None` (Layer-1-only)
    /// otherwise. Never fails the launch — a missing hook leaves the app clipboard-less but the
    /// user's clipboard safe (Layer 1 already applied via box_settings).
    fn setup_private_clipboard(&self) -> Option<(PrivateClipboard, ClipServer, String)> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()));
        let dll = config::hook_dll_path(
            std::env::var("GLASS_CLIP_HOOK_DLL").ok().as_deref(),
            exe_dir.as_deref(),
        )?;
        if !Path::new(&dll).exists() {
            crate::disclose_clip_disabled(&dll);
            return None;
        }
        let pipe = config::clip_pipe_name(&self.box_name);
        let store = PrivateClipboard::new();
        let server = ClipServer::start(&pipe, store.clone()).ok()?;
        let sbieini = sbieini(&self.dir);
        for (k, v) in config::clip_layer2_lines(&self.box_name, &dll) {
            if self.run_sbie(&sbieini, &["set", &self.box_name, &k, &v]).is_err() {
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
    pub(crate) fn configure(&self, level: SandboxLevel) -> Result<()> {
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
            let value = String::from_utf8_lossy(&out.stdout).trim().to_ascii_lowercase();
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

        // 3. compat templates.
        for tmpl in COMPAT_TEMPLATES {
            self.run_sbie(&sbieini, &["append", &self.box_name, "Template", tmpl])?;
        }

        // 4. strict: belt the no-network policy by closing the AFD socket device.
        if config::box_net(level).close_afd {
            self.run_sbie(
                &sbieini,
                &["append", &self.box_name, "ClosedFilePath", r"\Device\Afd*"],
            )?;
        }

        // 5. reload so the service picks up the new box config.
        self.run_sbie(&start_exe(&dir), &["/reload"])?;
        Ok(())
    }

    /// Run the optional build step contained: `Start.exe /box:<box> /wait cmd /c <build>`.
    pub(crate) fn run_build(&self, spec: &AppSpec) -> Result<()> {
        let Some(build) = &spec.build else {
            return Ok(());
        };
        let mut cmd = Command::new(start_exe(&self.dir));
        cmd.args([
            &format!("/box:{}", self.box_name),
            "/wait",
            "cmd",
            "/c",
            build,
        ]);
        if let Some(dir) = &spec.cwd {
            cmd.current_dir(dir);
        }
        let status = cmd
            .status()
            .map_err(|e| GlassError::AppNotStarted(format!("contained build: {e}")))?;
        if !status.success() {
            return Err(GlassError::AppNotStarted(format!(
                "contained build failed with status {status}"
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
        let script = super::config::build_launch_cmd_env(spec, &out_log, &err_log, clip_pipe.as_deref())?;
        std::fs::write(&cmd_path, script).map_err(|e| {
            GlassError::AppNotStarted(format!("write {}: {e}", cmd_path.display()))
        })?;

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
                if !line.is_empty() {
                    if let Ok(mut g) = sink.lock() {
                        g.push((stream, line.to_string()));
                    }
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
fn drain(
    path: &Path,
    offset: u64,
    pending: &mut String,
    stream: Stream,
    sink: &LogSink,
) -> u64 {
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
    /// itself runs under a separate Sandboxie-managed pid (see [`Self::pids`]).
    pub(crate) fn root_pid(&self) -> u32 {
        self.inner.pid()
    }

    /// The contained app's process set: `Start.exe /listpids` ∪ a Toolhelp descendant walk
    /// of the wrapper, deduped.
    pub(crate) fn pids(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = Vec::new();
        if let Ok(out) = Command::new(start_exe(&self.dir))
            .args([&format!("/box:{}", self.box_name), "/listpids"])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            pids = config::parse_listpids(&text);
        }
        for pid in crate::process::descendant_pids(self.inner.pid()) {
            if !pids.contains(&pid) {
                pids.push(pid);
            }
        }
        pids
    }

    /// Always `Ok(None)`: the `Start.exe` wrapper exits right after handing off to the box, so
    /// its exit does not signal the app's; and a `std::process::ExitStatus` for the contained
    /// app cannot be synthesized here. Discovery relies on `pids()` / the start timeout instead.
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        Ok(None)
    }

    /// The clipboard routing for this contained app: `Some(store)` when Layer 2 is active,
    /// `None` when Layer-1-only (the platform turns `None` into the "disabled" error — it must
    /// never fall back to the user's real clipboard for a contained app).
    #[allow(dead_code)] // Task 9 wires this into the Platform get/set_clipboard route
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
    pub(crate) fn kill(mut self) {
        let _ = Command::new(start_exe(&self.dir))
            .args([&format!("/box:{}", self.box_name), "/terminate"])
            .status();
        self.stop.store(true, Ordering::Relaxed);
        for h in self.tailers {
            let _ = h.join();
        }
        self.inner.kill();
        if let Some((_, server)) = self.clip.take() {
            server.stop();
        }
        let _ = std::fs::remove_dir_all(&self.logdir);
        // Best-effort: remove the box's config section from Sandboxie.ini.
        let _ = Command::new(sbieini(&self.dir))
            .args(["set", self.box_name.as_str(), "*", ""])
            .status();
    }
}
