//! Pure SBPL (Seatbelt) profile generator. No `unsafe`, no OS calls — unit-tested on the
//! Linux dev box. The profile is deny-default and keeps the launched app drivable
//! (WindowServer + AX) while containing filesystem, process, and (at `Strict`) network.
//! `$HOME` is not broadly readable, so secrets are hidden by construction.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use glass_core::SandboxLevel;

/// Inputs [`build_profile`] needs. `level` is never `Off` (the caller skips containment).
#[derive(Debug, Clone)]
pub struct ProfileOpts {
    /// Working dir: read + write allowed (the project dir).
    pub cwd: PathBuf,
    /// The launched program's path; its directory is read-allowed (the app bundle/binary).
    pub program: PathBuf,
    /// Extra paths re-exposed read-only (currently none on macOS; kept for parity/extensibility).
    pub ro_binds: Vec<PathBuf>,
    /// Extra paths re-exposed read-write.
    pub rw_binds: Vec<PathBuf>,
}

/// System read-only roots every dynamically-linked macOS process needs (dyld + frameworks).
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr/lib",
    "/System",
    "/Library",
    "/private/var/db/dyld",
    "/dev",
];
/// Scratch/cache roots a typical app writes to.
const SCRATCH_WRITE_ROOTS: &[&str] = &["/private/var/folders", "/private/tmp", "/tmp", "/dev"];

/// Build the deny-default SBPL profile for `level`. Never called with `SandboxLevel::Off`.
pub fn build_profile(level: SandboxLevel, opts: &ProfileOpts) -> String {
    let mut p = String::new();
    p.push_str("(version 1)\n(deny default)\n");
    // Process + basic host info.
    p.push_str("(allow process-fork)\n(allow process-exec*)\n(allow sysctl-read)\n");
    // Mach: broad lookup + register. `mach-register` is REQUIRED so the app can serve its
    // accessibility tree — a sandboxed app returns an empty AX tree without it.
    p.push_str("(allow mach-lookup)\n(allow mach-register)\n(allow iokit-open)\n");
    // Filesystem reads: system dyld/frameworks + program dir + cwd + ro_binds. $HOME is NOT
    // listed → deny-default hides the user's home (Linux tmpfs-home parity).
    p.push_str("(allow file-read* file-read-metadata\n");
    for r in SYSTEM_READ_ROOTS {
        push_subpath(&mut p, r);
    }
    // A bare program name (no `/`) yields `Some("")` from `Path::parent`; filter it out rather
    // than emit `(subpath "")`, which would (at best) be a no-op and (at worst) surprise a
    // future SBPL reader.
    if let Some(dir) = opts.program.parent().filter(|d| !d.as_os_str().is_empty()) {
        push_subpath_path(&mut p, dir);
    }
    push_subpath_path(&mut p, &opts.cwd);
    for b in &opts.ro_binds {
        push_subpath_path(&mut p, b);
    }
    p.push_str(")\n");
    // Filesystem writes: scratch/caches + cwd + rw_binds.
    p.push_str("(allow file-write*\n");
    for w in SCRATCH_WRITE_ROOTS {
        push_subpath(&mut p, w);
    }
    push_subpath_path(&mut p, &opts.cwd);
    for b in &opts.rw_binds {
        push_subpath_path(&mut p, b);
    }
    p.push_str(")\n");
    // Network: Default allows outbound; Strict omits it (deny-default blocks). The ONLY
    // Default-vs-Strict difference.
    if level == SandboxLevel::Default {
        p.push_str("(allow network*)\n");
    }
    // Clipboard isolation: the contained app cannot reach the real pasteboard.
    p.push_str("(deny mach-lookup (global-name \"com.apple.pasteboard.1\"))\n");
    p
}

/// Append `  (subpath "<escaped>")\n`.
fn push_subpath(out: &mut String, path: &str) {
    out.push_str("  (subpath ");
    out.push_str(&sbpl_quote(path));
    out.push_str(")\n");
}

fn push_subpath_path(out: &mut String, path: &Path) {
    push_subpath(out, &path.to_string_lossy());
}

/// Quote a string as an SBPL literal: wrap in double quotes, escaping `\` and `"`.
fn sbpl_quote(s: &str) -> String {
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ProfileOpts {
        ProfileOpts {
            cwd: PathBuf::from("/work/project"),
            program: PathBuf::from("/Applications/Demo.app/Contents/MacOS/Demo"),
            ro_binds: vec![],
            rw_binds: vec![],
        }
    }

    #[test]
    fn deny_default_and_mach_register_present() {
        let p = build_profile(SandboxLevel::Default, &opts());
        assert!(p.contains("(deny default)"), "{p}");
        assert!(
            p.contains("(allow mach-register)"),
            "AX needs mach-register:\n{p}"
        );
    }

    #[test]
    fn default_allows_network() {
        assert!(build_profile(SandboxLevel::Default, &opts()).contains("(allow network*)"));
    }

    #[test]
    fn strict_omits_network() {
        assert!(!build_profile(SandboxLevel::Strict, &opts()).contains("(allow network*)"));
    }

    #[test]
    fn clipboard_is_denied() {
        let p = build_profile(SandboxLevel::Default, &opts());
        assert!(
            p.contains(r#"(deny mach-lookup (global-name "com.apple.pasteboard.1"))"#),
            "{p}"
        );
    }

    #[test]
    fn cwd_is_read_and_write_allowed() {
        let p = build_profile(SandboxLevel::Default, &opts());
        // cwd appears under both the read block and the write block.
        assert_eq!(p.matches(r#"(subpath "/work/project")"#).count(), 2, "{p}");
    }

    #[test]
    fn home_is_not_broadly_readable() {
        let p = build_profile(SandboxLevel::Default, &opts());
        assert!(
            !p.contains(r#"(subpath "/Users"#),
            "home must stay hidden (deny-default):\n{p}"
        );
    }

    #[test]
    fn program_dir_is_read_allowed() {
        let p = build_profile(SandboxLevel::Default, &opts());
        assert!(
            p.contains(r#"(subpath "/Applications/Demo.app/Contents/MacOS")"#),
            "{p}"
        );
    }

    #[test]
    fn ro_and_rw_binds_appear() {
        let mut o = opts();
        o.ro_binds = vec![PathBuf::from("/opt/data")];
        o.rw_binds = vec![PathBuf::from("/opt/scratch")];
        let p = build_profile(SandboxLevel::Default, &o);
        assert!(p.contains(r#"(subpath "/opt/data")"#), "{p}");
        assert!(p.contains(r#"(subpath "/opt/scratch")"#), "{p}");
    }

    /// A `cwd` containing `"` and `\` must come out of `sbpl_quote` escaped, so the path stays
    /// an inert string literal rather than breaking out of the generated SBPL. This is the sole
    /// coverage for `sbpl_quote`, which is the only thing standing between an adversarial path
    /// and profile injection.
    #[test]
    fn cwd_with_quote_and_backslash_is_escaped_not_injected() {
        let mut o = opts();
        o.cwd = PathBuf::from(r#"/tmp/a"b\c"#);
        let p = build_profile(SandboxLevel::Default, &o);
        assert!(
            p.contains(r#"/tmp/a\"b\\c"#),
            "expected the escaped literal in the profile:\n{p}"
        );
        // If escaping were missing, the raw `"` would close the SBPL string literal early,
        // right after `/tmp/a`, letting the rest of the path (or worse) be read as SBPL.
        assert!(
            !p.contains("\"/tmp/a\"b"),
            "raw unescaped quote must not terminate the string literal early:\n{p}"
        );
    }

    /// A bare program name has no directory component (`Path::parent` returns `Some("")`);
    /// `build_profile` must not emit that as `(subpath "")`.
    #[test]
    fn bare_program_name_does_not_emit_empty_subpath() {
        let mut o = opts();
        o.program = PathBuf::from("Demo");
        let p = build_profile(SandboxLevel::Default, &o);
        assert!(!p.contains(r#"(subpath "")"#), "{p}");
    }

    /// `Strict` is defined as `Default` minus network access; assert that invariant directly
    /// so the two variants can never silently diverge elsewhere in the profile.
    #[test]
    fn strict_equals_default_minus_network_line() {
        let o = opts();
        assert_eq!(
            build_profile(SandboxLevel::Strict, &o),
            build_profile(SandboxLevel::Default, &o).replace("(allow network*)\n", "")
        );
    }
}
