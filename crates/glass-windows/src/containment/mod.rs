//! Windows containment provider seam. Pure policy in [`config`]; the cfg(windows) providers
//! (Unconfined, Sandboxie) are added in later tasks.

pub(crate) mod config;

#[cfg(any(windows, test))]
mod pid_discovery;

#[cfg(windows)]
mod clip_server;

#[cfg(windows)]
mod sandboxie;

// On-box deterministic validation of the private-clipboard hook. Compiled only for the
// Windows test profile; the tests inside are `#[ignore]`d (need Sandboxie + the built DLL).
#[cfg(all(test, windows))]
mod clip_onbox;

// On-box validation that the build step runs unconfined even under Sandboxie. `#[ignore]`d
// (needs Sandboxie); launches no window, so it runs over SSH.
#[cfg(all(test, windows))]
mod build_onbox;

#[cfg(all(test, windows))]
mod artifact_onbox;

#[cfg(windows)]
pub(crate) use imp::{ClipboardRoute, Launched, LogSink, resolve_containment};

// Re-export the Sandboxie availability/dir probes so the doctor can report posture
// without reaching into the private `sandboxie` module path.
#[cfg(windows)]
pub(crate) use sandboxie::{available, sandboxie_dir};

#[cfg(windows)]
pub(crate) use sandboxie::validate_protected_host_paths;

/// Resolve the clip shim DLL path the way the launcher does (env > exe dir > None), for doctor.
#[cfg(windows)]
pub(crate) fn config_shim_dll_path(exe_dir: Option<&str>) -> Option<String> {
    let env = config::shim_dll_env(&|k| std::env::var(k).ok());
    config::shim_dll_path(env.as_deref(), exe_dir, &|p| p.exists())
}

#[cfg(windows)]
mod imp {
    use glass_core::platform::ProtectedHostPath;
    use glass_core::{AppSpec, Deadline, GlassError, Result};

    /// Log lines captured from the app, tagged by stream. One alias, defined with the readers
    /// that fill it, rather than a second structurally-equal one here.
    pub(crate) use crate::logtap::LogSink;

    use super::config::{Decision, ProviderChoice, decide};

    /// Read the provider choice (env `GLASS_WIN_SANDBOX_PROVIDER`, default `auto`).
    fn provider_choice() -> Result<ProviderChoice> {
        match std::env::var("GLASS_WIN_SANDBOX_PROVIDER") {
            Ok(s) => ProviderChoice::parse(&s).map_err(GlassError::SandboxUnavailable),
            Err(_) => Ok(ProviderChoice::Auto),
        }
    }

    /// The selected containment provider for a launch.
    pub(crate) enum Containment {
        Unconfined,
        Sandboxie(super::sandboxie::Sandboxie),
    }

    /// Resolve which provider to use, or fail closed.
    pub(crate) fn resolve_containment(
        spec: &AppSpec,
        protected_paths: &[ProtectedHostPath],
    ) -> Result<Containment> {
        let choice = provider_choice()?;
        let dir = super::sandboxie::sandboxie_dir();
        match decide(spec.sandbox, choice, super::sandboxie::available(&dir)) {
            Decision::Unconfined => Ok(Containment::Unconfined),
            Decision::Sandboxie => {
                let s = super::sandboxie::Sandboxie::new(dir, super::sandboxie::unique_box_name());
                s.configure_with_paths(spec.sandbox, protected_paths)?;
                Ok(Containment::Sandboxie(s))
            }
            Decision::FailClosed(msg) => Err(GlassError::SandboxUnavailable(msg)),
        }
    }

    impl Containment {
        /// The optional build step runs UNCONFINED at every containment level — only the launched
        /// *run* is the security boundary (`2026-06-11-unsandbox-build-design`; this completes the
        /// deferred Windows follow-on). Containing the build bought nothing real and made even a
        /// trivial build stall/fail under Sandboxie (the box isolates the toolchain/cache/network).
        pub(crate) fn run_build(&self, spec: &AppSpec) -> Result<()> {
            crate::process::run_build_unconfined(spec)
        }
        /// Launch the app and wire its log readers into `logs`; returns the handle.
        pub(crate) fn launch(&self, spec: &AppSpec, logs: LogSink) -> Result<Launched> {
            match self {
                Containment::Unconfined => {
                    let mut cmd = crate::process::build_command(spec);
                    let mut app = crate::process::spawn_suspended_in_job(&mut cmd, spec.sandbox)?;
                    // Wire readers before resume; abandon waits for termination on setup failure.
                    if let Err(e) = app.tap_logs(&logs) {
                        app.abandon();
                        return Err(GlassError::AppNotStarted(format!(
                            "started {:?} but could not read its output ({e}); the app was \
                             stopped rather than left to write into a pipe nobody drains — free \
                             up threads on the host",
                            spec.run
                        )));
                    }
                    app.resume();
                    Ok(Launched::Unconfined(app))
                }
                Containment::Sandboxie(s) => s.launch(spec, logs).map(Launched::Sandboxie),
            }
        }
    }

    /// A launched, contained app.
    pub(crate) enum Launched {
        Unconfined(crate::process::LaunchedApp),
        Sandboxie(super::sandboxie::SandboxieApp),
    }
    impl Launched {
        /// The root (launcher) process pid — the spawned child's own pid.
        pub(crate) fn root_pid(&self) -> u32 {
            match self {
                Launched::Unconfined(a) => a.pid(),
                Launched::Sandboxie(a) => a.root_pid(),
            }
        }
        /// The app's authoritative process set — fully resolved per provider so callers can
        /// simply delegate (no second wrapper walk in `app_pids`):
        /// - Unconfined: the Job's kernel-tracked PID list ∪ a Toolhelp descendant walk
        ///   (validated fallback) ∪ the root pid.
        /// - Sandboxie: `Start.exe /listpids` ∪ a descendant walk of the wrapper (the boxed
        ///   app pids come from `/listpids`; the wrapper itself owns no app window).
        pub(crate) fn pids(&self) -> Vec<u32> {
            self.pids_by(Deadline::UNBOUNDED)
                .unwrap_or_else(|_| match self {
                    Launched::Unconfined(a) => crate::process::descendant_pids(a.pid()),
                    Launched::Sandboxie(a) => crate::process::descendant_pids(a.root_pid()),
                })
        }

        pub(crate) fn pids_by(&self, deadline: Deadline) -> Result<Vec<u32>> {
            match self {
                Launched::Unconfined(a) => super::pid_discovery::collect_pids_by_with(
                    deadline,
                    |_| Ok(a.job_pids()),
                    |received| crate::process::descendant_pids_by(a.pid(), received),
                ),
                Launched::Sandboxie(a) => a.pids_by(deadline),
            }
        }
        /// The window-class prefix that positively identifies this launch's app windows, when the
        /// containment renames them. `None` for unconfined launches (no renaming); for Sandboxie,
        /// `Sandbox:<box>:` — so discovery skips glass's own launcher console (left unrenamed).
        pub(crate) fn adoption_class_prefix(&self) -> Option<String> {
            match self {
                Launched::Unconfined(_) => None,
                Launched::Sandboxie(a) => Some(a.adoption_class_prefix()),
            }
        }
        pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            match self {
                Launched::Unconfined(a) => a.try_wait(),
                Launched::Sandboxie(a) => a.try_wait(),
            }
        }
        pub(crate) fn kill(self) -> crate::process::Closed {
            match self {
                Launched::Unconfined(a) => a.kill(),
                Launched::Sandboxie(a) => a.kill(),
            }
        }
    }

    /// How a launched app's clipboard is served. The platform turns this into behavior; a contained
    /// app must never read/write the user's real clipboard.
    pub(crate) enum ClipboardRoute {
        /// Unconfined (`sandbox=off`): the real OS clipboard (today's behavior; the explicit escape hatch).
        RealOs,
        /// Sandboxie + hook active: glass's private store.
        Private(glass_clip_shim_windows::store::PrivateClipboard),
        /// Sandboxie, hook unavailable (Layer-1-only): clipboard is disabled — error, never the real clipboard.
        DisabledContained,
    }

    impl Launched {
        pub(crate) fn clipboard_route(&self) -> ClipboardRoute {
            match self {
                Launched::Unconfined(_) => ClipboardRoute::RealOs,
                Launched::Sandboxie(a) => match a.private_clipboard() {
                    Some(store) => ClipboardRoute::Private(store),
                    None => ClipboardRoute::DisabledContained,
                },
            }
        }
    }
}
