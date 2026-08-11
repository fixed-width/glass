//! Whether `doctor`/`env` human output carries ANSI color, and which palette that means.
//!
//! [`resolve`] is pure in the facts it takes, so the whole decision table is unit-tested without
//! a pty and without mutating the process environment; [`palette`] is the only part that reads
//! the process.

use glass_core::Palette;
use std::env::VarError;
use std::io::IsTerminal;

/// `--color` on the `doctor` and `env` subcommands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Color when stdout is a terminal that will render it.
    #[default]
    Auto,
    /// Color whatever stdout is.
    Always,
    /// Never color.
    Never,
}

/// Should ANSI be emitted?
///
/// `no_color` follows <https://no-color.org>: present **and non-empty** suppresses color, so
/// `NO_COLOR=""` still colors.
pub(crate) fn resolve(
    choice: ColorChoice,
    is_tty: bool,
    no_color: Option<&str>,
    term: Option<&str>,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            is_tty && !no_color.is_some_and(|v| !v.is_empty()) && term != Some("dumb")
        }
    }
}

/// [`resolve`], with `NO_COLOR` and `TERM` looked up **by name** through `env`.
///
/// Split out from [`resolve`] so the names are pinned by a test rather than by argument order:
/// `resolve` takes the two values positionally, so swapping them at the call site type-checks,
/// and `TERM` in the `no_color` slot is non-empty on any real terminal — `auto` would then never
/// color anything.
fn resolve_from_env(
    choice: ColorChoice,
    is_tty: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> bool {
    let no_color = env("NO_COLOR");
    let term = env("TERM");
    resolve(choice, is_tty, no_color.as_deref(), term.as_deref())
}

/// Read `name` from the process environment for the color decision.
fn env_lookup(name: &str) -> Option<String> {
    value_of(std::env::var(name))
}

/// A `VarError` mapped to what [`resolve`] means by "present".
///
/// A variable set to non-UTF-8 bytes is *set*, and by construction non-empty, so it must arrive
/// as a non-empty `Some` and suppress color. `std::env::var(..).ok()` instead collapses it into
/// `None`, indistinguishable from truly unset — the fail-open `tools::floor_from_var` avoids for
/// `GLASS_SANDBOX_FLOOR`, here enabling color for a `NO_COLOR` that asked for the opposite. The
/// lossy conversion cannot yield an empty string, since an empty `OsString` is valid UTF-8 and
/// never reaches this arm.
fn value_of(v: Result<String, VarError>) -> Option<String> {
    match v {
        Ok(s) => Some(s),
        Err(VarError::NotPresent) => None,
        Err(VarError::NotUnicode(raw)) => Some(raw.to_string_lossy().into_owned()),
    }
}

/// Whether the terminal will actually interpret the escapes. A Windows console renders them as
/// text until it is switched into VT mode, so ask it; everywhere else there is nothing to switch.
#[cfg(windows)]
fn vt_ready() -> bool {
    glass_windows::console::enable_vt_processing()
}

#[cfg(not(windows))]
fn vt_ready() -> bool {
    true
}

/// Whether a console that reports it will not interpret ANSI should be given it anyway.
///
/// `vt_ready` is `false` for a console that *refuses* the mode (legacy conhost), where escapes
/// really do show up as visible text. `Always` ignores that — the user asked for color
/// explicitly, so refusing consoles get escapes rather than a silent downgrade to plain — while
/// `Auto` defers to it.
fn vt_gate_allows(vt_ready: bool, choice: ColorChoice) -> bool {
    vt_ready || choice != ColorChoice::Auto
}

/// The palette to render with, from the flag plus this process's facts.
pub(crate) fn palette(choice: ColorChoice) -> &'static Palette {
    palette_with(
        choice,
        std::io::stdout().is_terminal(),
        &env_lookup,
        vt_ready,
    )
}

/// [`palette`] with every process fact injected, so the decision is testable without a pty, a
/// console, or a mutated process environment.
///
/// `vt_ready` is reached only after the want-color decision, and deliberately so: on Windows the
/// call MUTATES the console mode, so a `--color never` run — a flag whose whole meaning is "don't
/// do color things" — must never get that far. Past that point it is called for exactly that side
/// effect: switching a VT-capable Windows console into VT mode is what makes `--color always`
/// produce color there instead of visible escape text. Only its *return value* is ignored for
/// `Always`, which is the console that refuses the mode outright (see [`vt_gate_allows`]).
fn palette_with(
    choice: ColorChoice,
    is_tty: bool,
    env: &dyn Fn(&str) -> Option<String>,
    vt_ready: impl FnOnce() -> bool,
) -> &'static Palette {
    if !resolve_from_env(choice, is_tty, env) {
        return &Palette::PLAIN;
    }
    if !vt_gate_allows(vt_ready(), choice) {
        return &Palette::PLAIN;
    }
    &Palette::ANSI
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::ffi::OsString;

    /// An env lookup that answers `name` with `value` and reports every other name unset.
    fn only(name: &'static str, value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |k: &str| (k == name).then(|| value.to_string())
    }

    /// An env lookup that reports everything unset.
    fn nothing_set(_: &str) -> Option<String> {
        None
    }

    /// Bytes that are not valid UTF-8, in this platform's `OsString` encoding — what the OS hands
    /// back as `VarError::NotUnicode`.
    #[cfg(unix)]
    fn not_unicode() -> OsString {
        use std::os::unix::ffi::OsStringExt as _;
        OsString::from_vec(vec![0xff, 0xfe])
    }
    #[cfg(windows)]
    fn not_unicode() -> OsString {
        use std::os::windows::ffi::OsStringExt as _;
        // An unpaired high surrogate: representable in a Windows `OsString`, not in a `String`.
        OsString::from_wide(&[0xd800])
    }

    #[test]
    fn always_and_never_ignore_every_other_fact() {
        for tty in [true, false] {
            assert!(resolve(ColorChoice::Always, tty, Some("1"), Some("dumb")));
            assert!(!resolve(ColorChoice::Never, tty, None, Some("xterm")));
        }
    }

    #[test]
    fn auto_colors_a_terminal_and_not_a_pipe() {
        assert!(resolve(
            ColorChoice::Auto,
            true,
            None,
            Some("xterm-256color")
        ));
        assert!(!resolve(
            ColorChoice::Auto,
            false,
            None,
            Some("xterm-256color")
        ));
        // TERM unset is a common real case (non-interactive shells, some Windows shells).
        assert!(resolve(ColorChoice::Auto, true, None, None));
    }

    #[test]
    fn auto_is_suppressed_by_a_non_empty_no_color() {
        assert!(!resolve(ColorChoice::Auto, true, Some("1"), Some("xterm")));
        assert!(!resolve(
            ColorChoice::Auto,
            true,
            Some("anything"),
            Some("xterm")
        ));
    }

    #[test]
    fn an_empty_no_color_does_not_suppress() {
        // no-color.org: the variable suppresses when present *and non-empty*. Easy to write
        // backwards, so it gets its own test rather than riding along in the one above.
        assert!(resolve(ColorChoice::Auto, true, Some(""), Some("xterm")));
    }

    #[test]
    fn auto_is_suppressed_by_a_dumb_terminal() {
        assert!(!resolve(ColorChoice::Auto, true, None, Some("dumb")));
        // but only by exactly "dumb"
        assert!(resolve(
            ColorChoice::Auto,
            true,
            None,
            Some("dumb-something")
        ));
    }

    #[test]
    fn auto_is_the_default_choice() {
        assert_eq!(ColorChoice::default(), ColorChoice::Auto);
    }

    #[test]
    fn an_explicit_always_ignores_a_console_that_cannot_render() {
        // The legacy-conhost case: the user asked for color, so a console that refuses VT mode
        // still gets escapes rather than a silent downgrade to plain.
        assert!(vt_gate_allows(false, ColorChoice::Always));
    }

    #[test]
    fn auto_defers_to_the_console() {
        // Auto must not emit escapes a console will render as visible text.
        assert!(!vt_gate_allows(false, ColorChoice::Auto));
        assert!(vt_gate_allows(true, ColorChoice::Auto));
    }

    #[test]
    fn the_suppressing_variable_is_the_one_named_no_color() {
        // `resolve` takes the two values positionally, so swapping them at the call site
        // type-checks and every decision-table test above still passes. In production TERM is
        // non-empty on any real terminal, so a swap would suppress color forever.
        assert!(!resolve_from_env(
            ColorChoice::Auto,
            true,
            &only("NO_COLOR", "1")
        ));
    }

    #[test]
    fn the_terminal_type_is_read_from_the_variable_named_term() {
        assert!(!resolve_from_env(
            ColorChoice::Auto,
            true,
            &only("TERM", "dumb")
        ));
    }

    #[test]
    fn an_empty_no_color_still_colors_a_real_terminal() {
        assert!(resolve_from_env(
            ColorChoice::Auto,
            true,
            &only("NO_COLOR", "")
        ));
    }

    #[test]
    fn an_unset_variable_reads_as_none_and_an_empty_one_as_empty() {
        assert_eq!(value_of(Err(VarError::NotPresent)), None);
        assert_eq!(value_of(Ok(String::new())), Some(String::new()));
    }

    #[test]
    fn a_variable_set_to_non_utf8_bytes_reads_as_set_and_non_empty() {
        // `.ok()` would report this as unset, which for NO_COLOR is fail-open: the variable is
        // set, so it must suppress.
        let v = value_of(Err(VarError::NotUnicode(not_unicode())));
        assert!(
            v.as_deref().is_some_and(|s| !s.is_empty()),
            "a set-but-non-UTF-8 value must arrive non-empty, got {v:?}"
        );
    }

    #[test]
    fn a_non_utf8_no_color_suppresses_color() {
        let env = |k: &str| {
            value_of(if k == "NO_COLOR" {
                Err(VarError::NotUnicode(not_unicode()))
            } else {
                Err(VarError::NotPresent)
            })
        };
        assert!(!resolve_from_env(ColorChoice::Auto, true, &env));
    }

    #[test]
    fn never_does_not_touch_the_console() {
        // On Windows `vt_ready` mutates the console mode, so `--color never` must not reach it.
        let asked = Cell::new(false);
        let p = palette_with(ColorChoice::Never, true, &nothing_set, || {
            asked.set(true);
            true
        });
        assert!(!asked.get(), "--color never must not call vt_ready");
        assert_eq!(p, &Palette::PLAIN);
    }

    #[test]
    fn a_piped_auto_does_not_touch_the_console() {
        let asked = Cell::new(false);
        let p = palette_with(ColorChoice::Auto, false, &nothing_set, || {
            asked.set(true);
            true
        });
        assert!(!asked.get(), "a redirected auto must not call vt_ready");
        assert_eq!(p, &Palette::PLAIN);
    }

    #[test]
    fn always_asks_the_console_and_colors_whatever_it_answers() {
        let asked = Cell::new(false);
        let p = palette_with(ColorChoice::Always, false, &nothing_set, || {
            asked.set(true);
            false
        });
        assert!(
            asked.get(),
            "always must still call vt_ready, for the side effect that enables VT mode"
        );
        assert_eq!(p, &Palette::ANSI);
    }

    #[test]
    fn auto_on_a_console_that_refuses_vt_mode_stays_plain() {
        assert_eq!(
            palette_with(ColorChoice::Auto, true, &nothing_set, || false),
            &Palette::PLAIN
        );
    }

    #[test]
    fn auto_on_a_terminal_that_renders_ansi_colors() {
        assert_eq!(
            palette_with(ColorChoice::Auto, true, &nothing_set, || true),
            &Palette::ANSI
        );
    }
}
