use super::*;

pub(super) enum Mutation<'a> {
    Pointer(&'a PointerEvent),
    Key,
    SemanticClick,
    SetValue,
    Type,
    ScrollToElement,
    ClipboardWrite,
    Window(&'a WindowOp),
    SelectWindow,
}

#[cfg(test)]
mod tests;

impl Glass {
    /// Read-only session support, excluding background actions in Foreground and all background text.
    pub fn session_capabilities(&mut self) -> Result<crate::SessionCapabilities> {
        let session = self.active_mut()?;
        let mut background_input = if session.input_mode == crate::InputMode::Background {
            session.platform.background_input_capabilities()?
        } else {
            crate::BackgroundInputCapabilities::default()
        };
        if background_input.profile.is_none() {
            background_input.click =
                crate::BackgroundOperation::unsupported("no qualified background click profile");
            background_input.scroll =
                crate::BackgroundOperation::unsupported("no qualified background scroll profile");
        }
        background_input.text =
            crate::BackgroundOperation::unsupported("background text input is unsupported");
        Ok(crate::SessionCapabilities {
            backend: session.backend.clone(),
            input_mode: session.input_mode,
            background_input,
        })
    }

    pub(super) fn check_mutation(&self, mutation: Mutation<'_>) -> Result<()> {
        let mode = self.require_active()?.input_mode;
        if mode == crate::InputMode::Foreground {
            return Ok(());
        }
        let operation = match mutation {
            Mutation::Pointer(PointerEvent::Click {
                button: MouseButton::Left,
                count: 1,
                modifiers,
                ..
            }) if modifiers.is_empty() => return Ok(()),
            Mutation::Pointer(PointerEvent::Scroll {
                dx: 0, modifiers, ..
            }) if modifiers.is_empty() => return Ok(()),
            Mutation::Window(WindowOp::Geometry) => return Ok(()),
            Mutation::Pointer(_) => "pointer operation",
            Mutation::Key | Mutation::Type => "keyboard or text input",
            Mutation::SemanticClick => "semantic click",
            Mutation::SetValue => "accessibility value write",
            Mutation::ScrollToElement => "scroll to element",
            Mutation::ClipboardWrite => "clipboard write",
            Mutation::Window(_) => "window mutation",
            Mutation::SelectWindow => "window selection",
        };
        Err(mode.unsupported(operation))
    }
}
