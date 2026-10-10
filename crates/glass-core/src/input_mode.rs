use serde::Serialize;

use crate::{GlassError, Result};

/// Input routing chosen for the lifetime of an app session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Foreground,
    Background,
}

impl InputMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Background => "background",
        }
    }

    /// Reject background mode before backend construction or external work until a route is qualified.
    pub fn require_foreground(self, operation: &'static str) -> Result<()> {
        if self == Self::Foreground {
            Ok(())
        } else {
            Err(self.unsupported(operation))
        }
    }

    pub(crate) fn unsupported(self, operation: &'static str) -> GlassError {
        GlassError::UnsupportedInputMode {
            input_mode: self,
            operation,
        }
        .before_dispatch()
    }
}

impl std::fmt::Display for InputMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_mode_names_match_the_wire_format() {
        for (mode, name) in [
            (InputMode::Foreground, "foreground"),
            (InputMode::Background, "background"),
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), name);
            assert_eq!(mode.as_str(), name);
            assert_eq!(mode.to_string(), name);
        }
    }
}
