//! Native identity and transport primitives for an owned Windows desktop.

mod identity;
mod pipe;
mod probe;
mod settings;

pub use identity::RuntimeApartment;
pub use identity::protect_stdio_inheritance;
pub use identity::signal_control;
pub use identity::stop_requested;
pub use identity::{
    ProcessIdentity, child_sessions_enabled, monotonic_ns, rdp_control_available, signal_stop,
};
pub use pipe::LocalPipe;
pub use probe::probe_graphical_desktop;
pub use settings::{
    SettingChange, change_setting, current_child_session, setting_action_exit_code,
};
