//! What to launch on a backend, and how to read the accessibility outline it produces.

use std::ffi::OsStr;

#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    /// Executable to look for on PATH.
    pub bin: &'static str,
    /// Its arguments. Must open a non-destructive, unsaved surface.
    pub args: &'static [&'static str],
    pub label: &'static str,
}

/// Linux is the only platform with no guaranteed stock app, so the runner probes in
/// order and records which one it selected.
pub const X11_CANDIDATES: [Candidate; 4] = [
    Candidate {
        bin: "xed",
        args: &[],
        label: "xed",
    },
    Candidate {
        bin: "gnome-text-editor",
        args: &[],
        label: "gnome-text-editor",
    },
    Candidate {
        // The prompt text must not look like anything the interaction check writes: a label
        // the runner could also have typed makes `value_contains` ambiguous evidence.
        bin: "zenity",
        args: &["--entry", "--text=type in this box"],
        label: "zenity",
    },
    Candidate {
        bin: "xterm",
        args: &[],
        label: "xterm",
    },
];

#[derive(Debug, Clone)]
pub struct Profile {
    pub backend: String,
    pub app: &'static Candidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineNode {
    pub id: u32,
    pub role: String,
    pub name: Option<String>,
    pub states: Vec<String>,
}

/// Why a run has no target app to drive. The two cases are kept apart because their remedies
/// differ: installing another candidate will never fix an unset `PATH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTargetApp {
    /// `PATH` is unset, so no bare command name resolves at all — nothing was even searched.
    NoSearchPath,
    /// `PATH` was searched and held none of the candidates.
    NoneInstalled,
}

impl NoTargetApp {
    /// Why a real run cannot proceed. A real run drives an app, so this is terminal.
    pub fn blocking_error(self, candidates: &[Candidate]) -> String {
        match self {
            Self::NoSearchPath => format!(
                "PATH is unset, so no command name can be resolved — set PATH to a directory \
                 holding one of: {}. The smoke run drives a real app, so one must be runnable.",
                bin_list(candidates)
            ),
            Self::NoneInstalled => format!(
                "no target app found — install one of: {}. The smoke run drives a real app, \
                 so at least one must be on PATH.",
                bin_list(candidates)
            ),
        }
    }

    /// What a `--dry-run` plan records instead. A dry run drives nothing, so the missing app
    /// does not stop it — it is stated as what a real run would hit, not as a present failure.
    pub fn plan_note(self, candidates: &[Candidate]) -> String {
        match self {
            Self::NoSearchPath => format!(
                "would fail: PATH is unset, so no target app can be resolved — set PATH to a \
                 directory holding one of: {}",
                bin_list(candidates)
            ),
            Self::NoneInstalled => format!(
                "would fail: no target app on PATH — install one of: {}",
                bin_list(candidates)
            ),
        }
    }
}

/// The candidates' executable names, in probe order, for a message that names what to install.
fn bin_list(candidates: &[Candidate]) -> String {
    candidates
        .iter()
        .map(|c| c.bin)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Pick the first candidate runnable on `path` — the value of `PATH`, or `None` when the
/// variable is unset. Taking the search path rather than reading the environment is what lets
/// the probe below be tested against a directory the test controls, instead of whichever apps
/// the machine running the suite happens to have.
pub fn resolve_app(
    candidates: &'static [Candidate],
    path: Option<&OsStr>,
) -> Result<&'static Candidate, NoTargetApp> {
    let path = path.ok_or(NoTargetApp::NoSearchPath)?;
    candidates
        .iter()
        .find(|c| on_path_in(c.bin, path))
        .ok_or(NoTargetApp::NoneInstalled)
}

/// Is this specific candidate runnable, without regard to probe order? What `--app` needs:
/// the caller has already chosen which candidate, and only wants to know whether the plan it
/// is about to print could actually be carried out.
pub fn runnable(candidate: &Candidate, path: Option<&OsStr>) -> bool {
    path.is_some_and(|p| on_path_in(candidate.bin, p))
}

/// Is `bin` runnable as a bare command name on `path`? Delegates to the workspace's one
/// `execvp`-semantics resolver — a regular file carrying an execute bit, first `$PATH` entry
/// wins — rather than testing `is_file()`, which would accept a mode-644 file named `xed` and
/// hand the run an "app" that cannot launch.
fn on_path_in(bin: &str, path: &OsStr) -> bool {
    glass_sandbox_core::resolve_on_path_in(OsStr::new(bin), path).is_some()
}

/// Test-only: a directory of real files to search — `bins` executable, `data` left mode-644 —
/// paired with the `$PATH` value naming it. Tests probe this rather than a stand-in predicate,
/// so what they exercise is the resolution a run actually performs; and they never read the
/// host's own `PATH`, so no test depends on which apps the machine happens to have.
#[cfg(test)]
pub(crate) fn path_fixture(
    bins: &[&str],
    data: &[&str],
) -> (tempfile::TempDir, std::ffi::OsString) {
    let dir = tempfile::tempdir().expect("tempdir");
    for name in bins {
        let p = dir.path().join(name);
        std::fs::write(&p, b"#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }
    for name in data {
        std::fs::write(dir.path().join(name), b"not a program\n").expect("write");
    }
    let path = std::env::join_paths([dir.path()]).expect("join_paths");
    (dir, path)
}

/// Parse the compact accessibility outline: `#id Role "name" (x,y wxh) [states]`.
/// Correctly handles names with escaped quotes and brackets in the name.
pub fn parse_outline(outline: &str) -> Vec<OutlineNode> {
    let mut nodes = Vec::new();
    for line in outline.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('#') else {
            continue;
        };
        let (id_str, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        let Ok(id) = id_str.parse::<u32>() else {
            continue;
        };
        let rest = rest.trim_start();

        // Extract role (first word)
        let role = rest
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();

        // Find what comes after the role
        let after_role = rest
            .split_whitespace()
            .nth(1)
            .and_then(|_| {
                rest.find(char::is_whitespace)
                    .map(|pos| rest[pos..].trim_start())
            })
            .unwrap_or("");

        // Parse name if present (starts with ")
        let (name, after_name) = if let Some(quoted) = after_role.strip_prefix('"') {
            match parse_escape_quoted_string(quoted) {
                Some((name_str, remainder)) => (Some(name_str), remainder.trim_start()),
                None => continue,
            }
        } else {
            (None, after_role)
        };

        // Parse states (last [...] group in after_name)
        let states = parse_states(after_name);

        nodes.push(OutlineNode {
            id,
            role,
            name,
            states,
        });
    }
    nodes
}

/// Parse an escape-quoted string starting after the opening quote.
/// Handles escape sequences: `\"` (escaped quote) and `\\` (escaped backslash).
/// Returns the unescaped content and the remainder after the closing quote.
fn parse_escape_quoted_string(s: &str) -> Option<(String, &str)> {
    let mut result = String::new();
    let mut chars = s.chars();
    let mut consumed_len = 0;

    while let Some(ch) = chars.next() {
        consumed_len += ch.len_utf8();

        match ch {
            '"' => {
                // Found closing quote
                return Some((result, &s[consumed_len..]));
            }
            '\\' => {
                // Escape sequence
                let next_ch = chars.next()?;
                consumed_len += next_ch.len_utf8();
                match next_ch {
                    '"' => result.push('"'),
                    '\\' => result.push('\\'),
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    '0' => result.push('\0'),
                    _ => {
                        result.push('\\');
                        result.push(next_ch);
                    }
                }
            }
            ch => result.push(ch),
        }
    }

    // No closing quote found
    None
}

/// Extract states from the last `[...]` group in the text.
fn parse_states(s: &str) -> Vec<String> {
    if let Some(bracket_start) = s.rfind('[') {
        if let Some(bracket_end) = s[bracket_start..].find(']') {
            let states_str = &s[bracket_start + 1..bracket_start + bracket_end];
            states_str
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        } else {
            vec![]
        }
    } else {
        vec![]
    }
}

/// The element the interaction check writes to: an explicitly editable node, else a
/// node whose normalized role is a text surface.
pub fn first_editable(nodes: &[OutlineNode]) -> Option<&OutlineNode> {
    const TEXT_ROLES: [&str; 3] = ["TextField", "TextArea", "ComboBox"];
    nodes
        .iter()
        .find(|n| n.states.iter().any(|s| s == "editable"))
        .or_else(|| nodes.iter().find(|n| TEXT_ROLES.contains(&n.role.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_picks_the_first_present_candidate() {
        let (_dir, path) = path_fixture(&["zenity", "xterm"], &[]);
        let c = resolve_app(&X11_CANDIDATES, Some(&path)).unwrap();
        assert_eq!(c.label, "zenity");
    }

    #[test]
    fn resolve_reports_none_installed_when_the_path_holds_no_candidate() {
        let (_dir, path) = path_fixture(&[], &[]);
        let err = resolve_app(&X11_CANDIDATES, Some(&path)).unwrap_err();
        assert_eq!(err, NoTargetApp::NoneInstalled);
    }

    /// The bug this pins: `xed` present but not executable is not an app. Reading it as one
    /// selects a candidate that cannot launch, so the run fails at check 1 with a launch error
    /// instead of falling through to a candidate that would have worked.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_is_not_a_candidate() {
        let (_dir, path) = path_fixture(&["xterm"], &["xed"]);
        let c = resolve_app(&X11_CANDIDATES, Some(&path)).unwrap();
        assert_eq!(
            c.label, "xterm",
            "a mode-644 file named xed must be skipped, not selected"
        );
    }

    /// `--app`'s probe: it asks about one chosen candidate, not the first that resolves.
    #[test]
    fn runnable_answers_about_the_named_candidate_only() {
        let (_dir, path) = path_fixture(&["zenity"], &[]);
        let zenity = X11_CANDIDATES.iter().find(|c| c.bin == "zenity").unwrap();
        let xterm = X11_CANDIDATES.iter().find(|c| c.bin == "xterm").unwrap();
        assert!(runnable(zenity, Some(&path)));
        assert!(!runnable(xterm, Some(&path)));
        assert!(!runnable(zenity, None), "an unset PATH resolves nothing");
    }

    /// An unset `PATH` is its own case: no amount of installing fixes it, so it must not be
    /// reported as "nothing installed".
    #[test]
    fn an_unset_path_is_not_reported_as_nothing_installed() {
        let err = resolve_app(&X11_CANDIDATES, None).unwrap_err();
        assert_eq!(err, NoTargetApp::NoSearchPath);
    }

    #[test]
    fn nothing_installed_names_every_candidate_and_what_to_do() {
        for msg in [
            NoTargetApp::NoneInstalled.blocking_error(&X11_CANDIDATES),
            NoTargetApp::NoneInstalled.plan_note(&X11_CANDIDATES),
        ] {
            for expected in ["xed", "gnome-text-editor", "zenity", "xterm"] {
                assert!(msg.contains(expected), "must name every candidate: {msg}");
            }
            assert!(msg.contains("install"), "must say what to do: {msg}");
        }
    }

    /// A dry run drives nothing, so its wording must not claim an app is required *now* — the
    /// real-run sentence ("the smoke run drives a real app") is false in the mode that prints
    /// the plan.
    #[test]
    fn the_plan_note_does_not_repeat_the_real_runs_claim() {
        let note = NoTargetApp::NoneInstalled.plan_note(&X11_CANDIDATES);
        assert!(
            note.starts_with("would fail"),
            "a plan states what a real run would hit: {note}"
        );
        assert!(
            !note.contains("drives a real app"),
            "the real-run claim is false under --dry-run: {note}"
        );
    }

    #[test]
    fn an_unset_path_says_to_set_it_rather_than_to_install_more() {
        for msg in [
            NoTargetApp::NoSearchPath.blocking_error(&X11_CANDIDATES),
            NoTargetApp::NoSearchPath.plan_note(&X11_CANDIDATES),
        ] {
            assert!(msg.contains("PATH is unset"), "must name the cause: {msg}");
            assert!(msg.contains("set PATH"), "must name the remedy: {msg}");
            assert!(
                !msg.contains("install"),
                "installing an app cannot fix an unset PATH, so it must not be the advice: {msg}"
            );
        }
    }

    #[test]
    fn parses_id_role_name_and_states() {
        let outline = "#1 Window \"Untitled\" (0,0 800x600)\n  #12 TextField \"Body\" (0,24 800x576) [editable,focusable]\n  #13 Button \"Save\" (700,0 80x24)\n";
        let nodes = parse_outline(outline);
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[1].id, 12);
        assert_eq!(nodes[1].role, "TextField");
        assert_eq!(nodes[1].name.as_deref(), Some("Body"));
        assert!(nodes[1].states.iter().any(|s| s == "editable"));
        assert_eq!(nodes[2].name.as_deref(), Some("Save"));
        assert!(nodes[2].states.is_empty());
    }

    #[test]
    fn first_editable_prefers_the_editable_state_then_a_text_role() {
        let by_state = vec![OutlineNode {
            id: 5,
            role: "Other(entry)".into(),
            name: None,
            states: vec!["editable".into()],
        }];
        assert_eq!(first_editable(&by_state).unwrap().id, 5);

        let by_role = vec![OutlineNode {
            id: 7,
            role: "TextArea".into(),
            name: None,
            states: vec![],
        }];
        assert_eq!(first_editable(&by_role).unwrap().id, 7);

        let neither = vec![OutlineNode {
            id: 9,
            role: "Button".into(),
            name: None,
            states: vec![],
        }];
        assert!(first_editable(&neither).is_none());
    }

    #[test]
    fn parses_name_with_escaped_quote() {
        let outline = "#3 Button \"Say \\\"hi\\\"\" (0,0 10x10)\n";
        let nodes = parse_outline(outline);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, 3);
        assert_eq!(nodes[0].name.as_deref(), Some("Say \"hi\""));
    }

    #[test]
    fn parses_bracket_in_name_with_no_states() {
        let outline = "#5 Label \"Item [1]\" (0,0 10x10)\n";
        let nodes = parse_outline(outline);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, 5);
        assert_eq!(nodes[0].name.as_deref(), Some("Item [1]"));
        assert!(nodes[0].states.is_empty());
    }

    #[test]
    fn parses_bracket_in_name_with_real_states() {
        let outline = "#7 TextField \"Item [1]\" (0,0 10x10) [editable,focusable]\n";
        let nodes = parse_outline(outline);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, 7);
        assert_eq!(nodes[0].name.as_deref(), Some("Item [1]"));
        assert!(nodes[0].states.iter().any(|s| s == "editable"));
        assert!(nodes[0].states.iter().any(|s| s == "focusable"));
        assert_eq!(nodes[0].states.len(), 2);
    }

    #[test]
    fn first_editable_uses_role_fallback_for_textfield() {
        let nodes = vec![OutlineNode {
            id: 10,
            role: "TextField".into(),
            name: None,
            states: vec![],
        }];
        assert_eq!(first_editable(&nodes).unwrap().id, 10);
    }
}
