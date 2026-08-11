//! Whether `doctor`/`env` human output carries ANSI color, and which palette that means.
//!
//! [`resolve`] is pure in the facts it takes, so the whole decision table is unit-tested without
//! a pty and without mutating the process environment; [`palette`] is the only part that reads
//! the process.

use glass_core::Palette;
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

/// Whether a console that may not render ANSI should still get it. `Always` ignores `vt_ready`
/// — the user asked for color explicitly, so a legacy console gets escapes rather than a silent
/// downgrade — while `Auto` defers to it rather than printing escapes as visible text.
fn vt_gate_allows(vt_ready: bool, choice: ColorChoice) -> bool {
    vt_ready || choice != ColorChoice::Auto
}

/// The palette to render with, from the flag plus the process facts.
///
/// `Always` still asks [`vt_ready`] — so forcing color on a legacy Windows console produces color
/// rather than visible escape text — but ignores the answer, because the user asked explicitly.
pub(crate) fn palette(choice: ColorChoice) -> &'static Palette {
    let no_color = std::env::var("NO_COLOR").ok();
    let term = std::env::var("TERM").ok();
    let want = resolve(
        choice,
        std::io::stdout().is_terminal(),
        no_color.as_deref(),
        term.as_deref(),
    );
    if !want {
        return &Palette::PLAIN;
    }
    // `vt_ready()` is called only past this point, and deliberately so: on Windows it MUTATES
    // the console mode, so a `--color never` run must never reach it.
    if !vt_gate_allows(vt_ready(), choice) {
        return &Palette::PLAIN;
    }
    &Palette::ANSI
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
