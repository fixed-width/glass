//! What to launch on a backend, and how to read the accessibility outline it produces.

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

/// Pick the first candidate present on the host.
pub fn resolve_app(
    candidates: &'static [Candidate],
    present: &dyn Fn(&str) -> bool,
) -> Result<&'static Candidate, String> {
    candidates.iter().find(|c| present(c.bin)).ok_or_else(|| {
        let names: Vec<&str> = candidates.iter().map(|c| c.bin).collect();
        format!(
            "no target app found — install one of: {}. The smoke run drives a real app, \
             so at least one must be on PATH.",
            names.join(", ")
        )
    })
}

/// Is `bin` on PATH? The default probe for [`resolve_app`].
pub fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let p = dir.join(bin);
                p.is_file()
            })
        })
        .unwrap_or(false)
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
        let present = |b: &str| b == "zenity" || b == "xterm";
        let c = resolve_app(&X11_CANDIDATES, &present).unwrap();
        assert_eq!(c.label, "zenity");
    }

    #[test]
    fn resolve_names_what_to_install_when_nothing_is_present() {
        let err = resolve_app(&X11_CANDIDATES, &|_| false).unwrap_err();
        for expected in ["xed", "gnome-text-editor", "zenity", "xterm"] {
            assert!(err.contains(expected), "must name every candidate: {err}");
        }
        assert!(err.contains("install"), "must say what to do: {err}");
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
