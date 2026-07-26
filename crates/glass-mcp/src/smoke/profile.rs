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
        bin: "zenity",
        args: &["--entry", "--text=glass smoke"],
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
        let role = rest
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        let name = rest
            .split_once('"')
            .and_then(|(_, after)| after.split_once('"').map(|(n, _)| n.to_string()));
        let states = rest
            .rsplit_once('[')
            .and_then(|(_, s)| s.split_once(']'))
            .map(|(s, _)| s.split(',').map(|t| t.trim().to_string()).collect())
            .unwrap_or_default();
        nodes.push(OutlineNode {
            id,
            role,
            name,
            states,
        });
    }
    nodes
}

/// The element the interaction check writes to: an explicitly editable node, else a
/// node whose normalized role is a text surface.
pub fn first_editable(nodes: &[OutlineNode]) -> Option<&OutlineNode> {
    const TEXT_ROLES: [&str; 3] = ["TextBox", "TextArea", "SearchBox"];
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
        let outline = "#1 Window \"Untitled\" (0,0 800x600)\n  #12 TextBox \"Body\" (0,24 800x576) [editable,focusable]\n  #13 Button \"Save\" (700,0 80x24)\n";
        let nodes = parse_outline(outline);
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[1].id, 12);
        assert_eq!(nodes[1].role, "TextBox");
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
}
