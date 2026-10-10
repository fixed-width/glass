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

    /// Reject a mode before constructing a backend or performing external work.
    /// Shipped backends use this until they have a qualified background route.
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
