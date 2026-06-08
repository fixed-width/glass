// The display-provisioning seam. v1 ships only ExistingDesktop; the headless
// VirtualDisplay provider is a deferred follow-on plan. `WindowsPlatform::start_app`
// consumes it (Task 5).

use glass_core::Result;

/// Where the target app renders for a session. v1 = ExistingDesktop; VirtualDisplay is a later plan.
pub(crate) trait DisplayProvider {
    /// Ensure a usable composited display exists; return its logical size in physical pixels.
    fn ensure(&mut self) -> Result<ProvisionedDisplay>;
}

pub(crate) struct ProvisionedDisplay {
    pub width: u32,
    pub height: u32,
}

mod existing;
pub(crate) use existing::ExistingDesktop;
