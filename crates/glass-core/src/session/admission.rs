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
    /// Read-only input support and desktop effects for the active session.
    pub fn session_capabilities(&self) -> Result<crate::SessionCapabilities> {
        let session = self.require_active()?;
        let mut input = session.platform.input_capabilities();
        if session.input_route == crate::InputRoute::WindowTargeted {
            input.restrict_window_targeted();
        }
        Ok(crate::SessionCapabilities {
            backend: session.backend.clone(),
            input,
        })
    }

    pub(super) fn check_mutation(&self, mutation: Mutation<'_>) -> Result<()> {
        let route = self.require_active()?.input_route;
        if route != crate::InputRoute::WindowTargeted {
            return Ok(());
        }
        let operation = match mutation {
            Mutation::Pointer(PointerEvent::Click {
                button: MouseButton::Left,
                count: 1,
                modifiers,
                ..
            }) if modifiers.is_empty() => {
                return require_targeted_support(
                    self.require_active()?.platform.input_capabilities().click,
                    "click",
                );
            }
            Mutation::Pointer(PointerEvent::Scroll {
                dx: 0, modifiers, ..
            }) if modifiers.is_empty() => {
                return require_targeted_support(
                    self.require_active()?.platform.input_capabilities().scroll,
                    "scroll",
                );
            }
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
        Err(GlassError::UnsupportedOperation {
            operation,
            reason: "this session only admits qualified coordinate left clicks and vertical scrolls",
        }
        .before_dispatch())
    }
}

fn require_targeted_support(
    capability: crate::InputOperationCapability,
    operation: &'static str,
) -> Result<()> {
    if capability.support.status == crate::Support::Supported
        && capability.desktop_interference == crate::DesktopInterference::None
    {
        return Ok(());
    }
    Err(GlassError::UnsupportedOperation {
        operation,
        reason: capability
            .support
            .note
            .unwrap_or("no qualified input without desktop interference"),
    }
    .before_dispatch())
}
