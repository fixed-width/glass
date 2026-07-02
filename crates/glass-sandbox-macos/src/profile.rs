//! Pure SBPL (Seatbelt) profile generator. No `unsafe`, no OS calls — unit-tested on the
//! Linux dev box. The profile is deny-default and keeps the launched app drivable
//! (WindowServer + AX) while containing filesystem, process, and (at `Strict`) network.
//!
//! Filesystem model (matches Linux's `--ro-bind / /` + `--tmpfs $HOME`): the whole
//! filesystem is readable read-only, EXCEPT the user home directories (`/Users`), which are
//! denied so secrets (`~/.ssh` etc.) stay hidden by construction; the working dir, the
//! launched program's own directory, and any caller `ro_binds` are then re-allowed even if
//! they happen to live under a home, as long as they aren't the home root itself (see
//! [`is_safe_reallow`]); individual caller `ro_files` are re-allowed as single-file literals
//! (used for the injected clip-shim dylib). Writes stay deny-default: only the working dir,
//! caller `rw_binds`, and scratch/cache roots.
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
    /// Extra directories re-exposed read-only (a `subpath` re-allow of the whole tree).
    pub ro_binds: Vec<PathBuf>,
    /// Extra single FILES re-exposed read-only via a `(literal …)` rule — used to re-allow
    /// exactly the injected clip-shim dylib without exposing its whole directory (a file
    /// literal, unlike a `subpath`, grants no access to siblings). A file under `/Users` is
    /// safe to re-allow for read this way, so these are emitted unconditionally after the
    /// `/Users` deny.
    pub ro_files: Vec<PathBuf>,
    /// Extra paths re-exposed read-write.
    pub rw_binds: Vec<PathBuf>,
    /// When true (an injectable contained target), ALLOW `com.apple.pasteboard.1` so the shim's
    /// private named pasteboard works; when false (hardened/non-injectable), DENY it (the app
    /// can't reach the real pasteboard). Isolation for injectable targets comes from the shim's
    /// redirect, not from denying pasteboardd.
    pub allow_pasteboard: bool,
}

/// Scratch/cache roots a typical app writes to (also readable via the whole-FS read allow).
const SCRATCH_WRITE_ROOTS: &[&str] = &["/private/var/folders", "/private/tmp", "/tmp", "/dev"];

/// Build the deny-default SBPL profile for `level`. Never called with `SandboxLevel::Off`.
pub fn build_profile(level: SandboxLevel, opts: &ProfileOpts) -> String {
    let mut p = String::new();
    p.push_str("(version 1)\n(deny default)\n");
    p.push_str("(allow process-fork)\n(allow process-exec*)\n(allow sysctl-read)\n");
    // `mach-register` is REQUIRED so the app can vend its accessibility port — without it an
    // AXUIElement read returns an empty tree.
    p.push_str("(allow mach-lookup)\n(allow mach-register)\n(allow iokit-open)\n");
    // Reads: the whole filesystem, read-only, so any app can launch (matches Linux `--ro-bind
    // / /`)...
    p.push_str("(allow file-read* file-read-metadata (subpath \"/\"))\n");
    // ...except the user home directories, so secrets (~/.ssh etc.) stay hidden (matches Linux
    // `--tmpfs $HOME`). `/Users` is the standard macOS home layout.
    p.push_str("(deny file-read* (subpath \"/Users\"))\n");
    // ...but re-allow reads under the working dir + program dir + caller ro_binds — each only if
    // it won't re-expose a whole home or the root (see `is_safe_reallow`). Emitted AFTER the
    // `/Users` deny so SBPL's last-match-wins restores a real project dir living under a home.
    if is_safe_reallow(&opts.cwd) {
        emit_read_allow(&mut p, &opts.cwd);
    }
    if let Some(dir) = opts.program.parent().filter(|d| !d.as_os_str().is_empty()) {
        if is_safe_reallow(dir) {
            emit_read_allow(&mut p, dir);
        }
    }
    for b in opts.ro_binds.iter().filter(|b| is_safe_reallow(b)) {
        emit_read_allow(&mut p, b);
    }
    // Re-allow reads of individual FILES (a `literal`, not a `subpath`) after the `/Users`
    // deny — used for the injected clip-shim dylib, which lives under $HOME. A file literal
    // grants no access to its siblings, so (unlike a directory subpath) it needs no
    // home-exposure guard.
    for f in &opts.ro_files {
        emit_read_allow_file(&mut p, f);
    }
    // Writes: deny-default; only scratch/caches + the working dir + caller rw_binds.
    p.push_str("(allow file-write*\n");
    for w in SCRATCH_WRITE_ROOTS {
        push_subpath(&mut p, w);
    }
    if is_safe_reallow(&opts.cwd) {
        push_subpath_path(&mut p, &opts.cwd);
    }
    for b in opts.rw_binds.iter().filter(|b| is_safe_reallow(b)) {
        push_subpath_path(&mut p, b);
    }
    p.push_str(")\n");
    // Network: Default allows outbound; Strict omits it (deny-default blocks). The ONLY
    // Default-vs-Strict difference.
    if level == SandboxLevel::Default {
        p.push_str("(allow network*)\n");
    }
    // Clipboard: deny the real pasteboard unless this is an injectable target (whose shim
    // redirects `generalPasteboard` to a private named pasteboard — see glass-clip-shim-macos).
    if !opts.allow_pasteboard {
        p.push_str("(deny mach-lookup (global-name \"com.apple.pasteboard.1\"))\n");
    }
    p
}

/// Emit a standalone read-allow statement for `path` (kept separate from the write block so an
/// empty set never produces invalid SBPL).
fn emit_read_allow(out: &mut String, path: &Path) {
    out.push_str("(allow file-read* file-read-metadata ");
    out.push_str(&format!("(subpath {})", sbpl_quote(&path.to_string_lossy())));
    out.push_str(")\n");
}

/// Emit a standalone read-allow for a single FILE (`literal`, not `subpath`), so exactly that
/// file is re-allowed for read without exposing its directory. Used for the injected clip-shim
/// dylib (see [`ProfileOpts::ro_files`]).
fn emit_read_allow_file(out: &mut String, path: &Path) {
    out.push_str("(allow file-read* file-read-metadata ");
    out.push_str(&format!("(literal {})", sbpl_quote(&path.to_string_lossy())));
    out.push_str(")\n");
}

/// Whether re-allowing `path` is safe — i.e. it won't re-expose a whole user home or the
/// filesystem root through the `/Users` read-deny. Rejects non-absolute paths, `/`, `/Users`,
/// and a bare home root `/Users/<user>`; a deeper path (a real project dir) or any path outside
/// `/Users` is safe. Fail-safe: an unsafe path is simply omitted (the app is over-contained,
/// never over-exposed).
fn is_safe_reallow(path: &Path) -> bool {
    use std::path::Component;
    if !path.is_absolute() {
        return false;
    }
    if path == Path::new("/") || path == Path::new("/Users") {
        return false;
    }
    let normals: Vec<&std::ffi::OsStr> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    !(normals.len() == 2 && normals[0] == "Users")
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
            ro_files: vec![],
            rw_binds: vec![],
            // Default-safe: deny the real pasteboard unless a test opts in.
            allow_pasteboard: false,
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
    fn pasteboard_denied_when_not_injectable() {
        let mut o = opts();
        o.allow_pasteboard = false;
        let p = build_profile(SandboxLevel::Default, &o);
        assert!(
            p.contains(r#"(deny mach-lookup (global-name "com.apple.pasteboard.1"))"#),
            "{p}"
        );
    }

    #[test]
    fn pasteboard_allowed_when_injectable() {
        let mut o = opts();
        o.allow_pasteboard = true;
        let p = build_profile(SandboxLevel::Default, &o);
        assert!(
            !p.contains("com.apple.pasteboard.1"),
            "an injectable target must not deny the pasteboard (the shim redirects it):\n{p}"
        );
    }

    #[test]
    fn cwd_is_read_and_write_allowed() {
        let p = build_profile(SandboxLevel::Default, &opts());
        // cwd appears under both the read-reallow statement and the write block.
        assert_eq!(p.matches(r#"(subpath "/work/project")"#).count(), 2, "{p}");
    }

    /// Replaces the old `home_is_not_broadly_readable`: reads are now whole-filesystem, with
    /// `/Users` carved out by an explicit deny — assert both halves of that model are present.
    #[test]
    fn reads_all_but_denies_home() {
        let p = build_profile(SandboxLevel::Default, &opts());
        assert!(
            p.contains(r#"(allow file-read* file-read-metadata (subpath "/"))"#),
            "the whole filesystem must be read-allowed:\n{p}"
        );
        assert!(
            p.contains(r#"(deny file-read* (subpath "/Users"))"#),
            "home must be denied:\n{p}"
        );
    }

    /// A project dir living under `$HOME` (the common case) must still be usable: its
    /// read-reallow has to come AFTER the `/Users` deny so SBPL's last-match-wins semantics
    /// restore it, and it must remain write-allowed too.
    #[test]
    fn cwd_under_home_is_reallowed_after_home_deny() {
        let mut o = opts();
        o.cwd = PathBuf::from("/Users/dev/project");
        let p = build_profile(SandboxLevel::Default, &o);

        let deny_idx = p
            .find(r#"(deny file-read* (subpath "/Users"))"#)
            .expect("home deny must be present");
        let reallow_idx = p
            .find(r#"(allow file-read* file-read-metadata (subpath "/Users/dev/project"))"#)
            .expect("cwd read-reallow must be present");
        assert!(
            reallow_idx > deny_idx,
            "cwd reallow must be emitted after the home deny so SBPL's last-match-wins restores it:\n{p}"
        );

        let write_block_start = p.find("(allow file-write*").expect("write block present");
        assert!(
            p[write_block_start..].contains(r#"(subpath "/Users/dev/project")"#),
            "cwd must still be write-allowed:\n{p}"
        );
    }

    /// The bare home root (`/Users/<user>`) must never be reallowed — that would re-expose the
    /// whole home the `/Users` deny exists to hide. Root (`/`) must never be write-allowed
    /// either, even though it's the expected read-all target.
    #[test]
    fn home_root_cwd_is_not_reallowed() {
        let mut o = opts();
        o.cwd = PathBuf::from("/Users/dev");
        let p = build_profile(SandboxLevel::Default, &o);
        assert!(
            !p.contains(r#"(subpath "/Users/dev")"#),
            "a bare home root must not be reallowed for read or write:\n{p}"
        );

        let mut o_root = opts();
        o_root.cwd = PathBuf::from("/");
        let p_root = build_profile(SandboxLevel::Default, &o_root);
        let write_block_start = p_root.find("(allow file-write*").expect("write block present");
        assert!(
            !p_root[write_block_start..].contains(r#"(subpath "/")"#),
            "root must never be write-allowed:\n{p_root}"
        );
    }

    #[test]
    fn is_safe_reallow_rejects_root_home_and_relative_paths() {
        assert!(!is_safe_reallow(Path::new("/")), "root");
        assert!(!is_safe_reallow(Path::new("/Users")), "Users root");
        assert!(!is_safe_reallow(Path::new("/Users/dev")), "a bare home root");
        assert!(!is_safe_reallow(Path::new("rel/path")), "relative path");
        assert!(!is_safe_reallow(Path::new(".")), "relative cwd shorthand");
    }

    #[test]
    fn is_safe_reallow_accepts_real_project_dirs() {
        assert!(is_safe_reallow(Path::new("/Users/dev/project")), "a project dir under home");
        assert!(is_safe_reallow(Path::new("/work/project")), "outside home entirely");
        assert!(is_safe_reallow(Path::new("/tmp/x")), "scratch dir");
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

    /// A `ro_files` entry (the injected clip-shim dylib) must be re-allowed for read as a
    /// single-file `(literal …)` — never a `(subpath …)` that would expose its whole
    /// directory — and emitted AFTER the `/Users` deny so SBPL's last-match-wins restores read
    /// for a file living under $HOME (the shim dylib's normal location in glass's target dir).
    #[test]
    fn ro_files_emit_a_file_literal_after_the_home_deny() {
        let mut o = opts();
        let dylib = "/Users/dev/proj/target/release/libglass_clip_shim_macos.dylib";
        o.ro_files = vec![PathBuf::from(dylib)];
        let p = build_profile(SandboxLevel::Default, &o);

        let literal = format!(r#"(allow file-read* file-read-metadata (literal "{dylib}"))"#);
        let literal_idx = p.find(&literal).unwrap_or_else(|| {
            panic!("a ro_files entry must emit a (literal ...) read-allow:\n{p}")
        });
        assert!(
            !p.contains(&format!(r#"(subpath "{dylib}")"#)),
            "a ro_files entry must NOT widen to a subpath:\n{p}"
        );
        let deny_idx = p
            .find(r#"(deny file-read* (subpath "/Users"))"#)
            .expect("home deny must be present");
        assert!(
            literal_idx > deny_idx,
            "the file literal must come after the /Users deny so last-match-wins restores it:\n{p}"
        );
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
