//! Pure provider/box configuration logic for Windows containment — no Win32, no process
//! spawning — so the policy is unit-tested on the Linux dev box. The cfg(windows) Sandboxie
//! provider consumes these.

// `decide`/`Decision`/`ProviderChoice` are consumed by the cfg(windows) containment seam,
// and the Sandboxie box-config helpers (`box_net`/`box_settings`/`parse_listpids`/`pick_path`)
// by the cfg(windows) Sandboxie provider. On non-Windows targets those consumers are gated
// out, so the helpers are dead there only.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::Path;

use glass_core::{AppSpec, GlassError, Result, SandboxLevel};

/// Which in-OS containment provider the user asked for (env/arg). `glass-core` only knows
/// the level; this is the Windows-specific provider selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderChoice {
    Auto,
    Sandboxie,
    None,
}

impl ProviderChoice {
    /// Parse `GLASS_WIN_SANDBOX_PROVIDER` / the `glass_start` field (case-insensitive).
    /// Unknown → Err with the accepted values (validate at the boundary, no silent default).
    pub(crate) fn parse(s: &str) -> std::result::Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "sandboxie" => Ok(Self::Sandboxie),
            "none" => Ok(Self::None),
            other => Err(format!(
                "unknown windows sandbox provider {other:?} (expected auto|sandboxie|none)"
            )),
        }
    }
}

/// The resolved decision for a launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// No in-OS containment (level Off, or provider none with level Off).
    Unconfined,
    /// Use the Sandboxie provider.
    Sandboxie,
    /// Refuse to launch (in-OS containment requested but unavailable). Carries a remedy.
    FailClosed(String),
}

/// Decide the provider from level + choice + whether Sandboxie is available right now.
/// `Off` is always Unconfined. For `Default`/`Strict`, an in-OS provider is required:
/// fail closed if none is available (mirrors Linux bwrap).
pub(crate) fn decide(level: SandboxLevel, choice: ProviderChoice, sandboxie_available: bool) -> Decision {
    if level == SandboxLevel::Off {
        return Decision::Unconfined;
    }
    match choice {
        ProviderChoice::None => Decision::FailClosed(
            "in-OS containment requested but provider=none; install Sandboxie Classic \
             (sandboxie-plus.com/downloads) or use sandbox=off"
                .into(),
        ),
        ProviderChoice::Sandboxie | ProviderChoice::Auto => {
            if sandboxie_available {
                Decision::Sandboxie
            } else {
                Decision::FailClosed(
                    "Sandboxie is not available (Start.exe / SbieSvc / SbieDrv); install \
                     Sandboxie Classic (sandboxie-plus.com/downloads) and ensure its service is \
                     running, or use sandbox=off"
                        .into(),
                )
            }
        }
    }
}

/// Box network policy for a level. (`Off` never reaches here.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BoxNet {
    pub allow_network_access: bool,
    pub close_afd: bool,
}
pub(crate) fn box_net(level: SandboxLevel) -> BoxNet {
    match level {
        SandboxLevel::Strict => BoxNet { allow_network_access: false, close_afd: true },
        // Default (and Off, defensively) = network on, no device close.
        _ => BoxNet { allow_network_access: true, close_afd: false },
    }
}

/// The `SbieIni.exe set <box> <key> <value>` lines to configure a glass box for `level`.
/// Returns (key, value) pairs; `close_afd` is applied as a separate `append ClosedFilePath`.
pub(crate) fn box_settings(level: SandboxLevel) -> Vec<(&'static str, &'static str)> {
    let net = box_net(level);
    vec![
        ("Enabled", "y"),
        ("KeepTokenIntegrity", "y"),
        ("NotifyInternetAccessDenied", "n"),
        ("NotifyStartRunAccessDenied", "n"),
        ("AllowNetworkAccess", if net.allow_network_access { "y" } else { "n" }),
    ]
}

/// Parse `Start.exe /box /listpids` stdout: first line is the count, then one PID per line.
/// Tolerant of blank lines / CRLF; ignores non-numeric lines after the header.
pub(crate) fn parse_listpids(stdout: &str) -> Vec<u32> {
    stdout
        .lines()
        .skip(1)
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

/// First-match path precedence: explicit arg > env > registry-probe > default.
pub(crate) fn pick_path(
    explicit: Option<&str>,
    env: Option<&str>,
    registry: Option<&str>,
    default: &str,
) -> String {
    explicit
        .or(env)
        .or(registry)
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

/// Characters in a `spec.run` token or `spec.cwd` that cannot be safely emitted into the
/// generated `launch.cmd`, so a token containing any of them is rejected (fail-closed) rather
/// than producing a script cmd.exe would mis-parse or that could inject a second command:
/// - `"` (0x22) — CMD has no escape for a double-quote inside a `"..."` token, so the token is
///   unrepresentable.
/// - `%` (0x25) — CMD expands `%VAR%` **inside** double quotes, so a token could read/inject
///   host environment variables (`%COMSPEC%`, `%PATH%`, …) into the launched argv.
/// - `\r` / `\n` — the tokens are written into a `.cmd` **file**; an embedded newline ends the
///   line and turns the remainder into a fresh CMD statement (command injection).
const FORBIDDEN_LAUNCH_CHARS: [char; 4] = ['"', '%', '\r', '\n'];

fn reject_unsafe_launch_token(s: &str) -> Result<()> {
    if let Some(c) = s.chars().find(|c| FORBIDDEN_LAUNCH_CHARS.contains(c)) {
        return Err(GlassError::AppNotStarted(format!(
            "an argument or cwd contains a character ({c:?}) that cannot be safely passed \
             through the Sandboxie launch wrapper (double-quote, percent, and newline are \
             rejected to prevent command injection)"
        )));
    }
    Ok(())
}

/// Build the `launch.cmd` body: `@echo off`, an optional `cd /d "<cwd>"`, then the quoted
/// exe + each quoted arg with stdout/stderr redirected to the log files.
///
/// Every token is wrapped in `"..."`. CMD treats most of its metacharacters (`& | < > ^`)
/// literally inside a quoted token, so quoting handles those — but `"`, `%`, and embedded
/// newlines cannot be made safe in this context (see [`FORBIDDEN_LAUNCH_CHARS`]), so any
/// `spec.run` element or `spec.cwd` containing one is rejected with an honest error.
pub(crate) fn build_launch_cmd(spec: &AppSpec, out_log: &Path, err_log: &Path) -> Result<String> {
    if let Some(cwd) = &spec.cwd {
        reject_unsafe_launch_token(&cwd.to_string_lossy())?;
    }
    for part in &spec.run {
        reject_unsafe_launch_token(part)?;
    }
    let mut s = String::from("@echo off\r\n");
    if let Some(cwd) = &spec.cwd {
        s.push_str(&format!("cd /d \"{}\"\r\n", cwd.display()));
    }
    let mut line = String::new();
    for (i, part) in spec.run.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        line.push('"');
        line.push_str(part);
        line.push('"');
    }
    s.push_str(&format!(
        "{line} 1>\"{}\" 2>\"{}\"\r\n",
        out_log.display(),
        err_log.display()
    ));
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn launch_spec(run: Vec<String>, cwd: Option<&str>) -> AppSpec {
        AppSpec {
            build: None,
            run,
            cwd: cwd.map(PathBuf::from),
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
        }
    }

    #[test]
    fn launch_cmd_quotes_tokens_and_redirects() {
        let spec = launch_spec(vec!["C:\\Program Files\\app.exe".into(), "--flag".into()], None);
        let script = build_launch_cmd(&spec, Path::new("out.log"), Path::new("err.log")).unwrap();
        assert!(script.starts_with("@echo off\r\n"), "script: {script:?}");
        assert!(script.contains("\"C:\\Program Files\\app.exe\" \"--flag\""), "script: {script:?}");
        assert!(script.contains("1>\"out.log\" 2>\"err.log\""), "script: {script:?}");
    }

    #[test]
    fn launch_cmd_rejects_injection_chars_in_run() {
        // Double-quote (unrepresentable), percent (cmd expands %VAR% inside quotes),
        // and CR/LF (tokens go into a .cmd file — a newline starts a new command).
        for bad in ["a\"b", "a%PATH%b", "a\rb", "a\nb"] {
            let spec = launch_spec(vec!["app.exe".into(), bad.into()], None);
            assert!(
                build_launch_cmd(&spec, Path::new("o"), Path::new("e")).is_err(),
                "run token {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn launch_cmd_rejects_injection_chars_in_cwd() {
        for bad in ["C:\\a\"b", "C:\\%TEMP%", "C:\\a\rb", "C:\\a\nb"] {
            let spec = launch_spec(vec!["app.exe".into()], Some(bad));
            assert!(
                build_launch_cmd(&spec, Path::new("o"), Path::new("e")).is_err(),
                "cwd {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn off_is_always_unconfined() {
        assert_eq!(decide(SandboxLevel::Off, ProviderChoice::None, false), Decision::Unconfined);
        assert_eq!(decide(SandboxLevel::Off, ProviderChoice::Auto, false), Decision::Unconfined);
    }

    #[test]
    fn default_auto_uses_sandboxie_when_available_else_fails_closed() {
        assert_eq!(decide(SandboxLevel::Default, ProviderChoice::Auto, true), Decision::Sandboxie);
        assert!(matches!(
            decide(SandboxLevel::Default, ProviderChoice::Auto, false),
            Decision::FailClosed(_)
        ));
    }

    #[test]
    fn strict_none_fails_closed() {
        assert!(matches!(
            decide(SandboxLevel::Strict, ProviderChoice::None, true),
            Decision::FailClosed(_)
        ));
    }

    #[test]
    fn box_settings_strict_blocks_network() {
        let s = box_settings(SandboxLevel::Strict);
        assert!(s.contains(&("AllowNetworkAccess", "n")));
        assert!(s.contains(&("KeepTokenIntegrity", "y")));
        assert!(box_net(SandboxLevel::Strict).close_afd);
    }

    #[test]
    fn box_settings_default_allows_network() {
        let s = box_settings(SandboxLevel::Default);
        assert!(s.contains(&("AllowNetworkAccess", "y")));
        assert!(!box_net(SandboxLevel::Default).close_afd);
    }

    #[test]
    fn parse_listpids_reads_count_then_pids() {
        assert_eq!(parse_listpids("3\r\n100\r\n200\r\n300\r\n"), vec![100, 200, 300]);
        assert_eq!(parse_listpids("0\r\n"), Vec::<u32>::new());
        assert_eq!(parse_listpids(""), Vec::<u32>::new());
    }

    #[test]
    fn provider_choice_parse() {
        assert_eq!(ProviderChoice::parse("AUTO").unwrap(), ProviderChoice::Auto);
        assert_eq!(ProviderChoice::parse("sandboxie").unwrap(), ProviderChoice::Sandboxie);
        assert!(ProviderChoice::parse("bogus").is_err());
    }

    #[test]
    fn pick_path_precedence() {
        assert_eq!(pick_path(Some("X"), Some("Y"), Some("Z"), "D"), "X");
        assert_eq!(pick_path(None, Some("Y"), Some("Z"), "D"), "Y");
        assert_eq!(pick_path(None, None, Some("Z"), "D"), "Z");
        assert_eq!(pick_path(None, None, None, "D"), "D");
    }
}
