//! Selection policy and ownership of a retained Windows application desktop.

use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod files;
#[cfg(windows)]
mod native;
#[cfg(windows)]
mod runtime;
#[cfg(all(windows, feature = "network"))]
pub mod tray;
#[cfg(windows)]
mod work;
#[cfg(windows)]
mod worker;

#[cfg(windows)]
pub(crate) use native::Manager;
#[cfg(windows)]
pub use native::{command, set_requested_mode};
#[cfg(windows)]
pub use worker::run as run_worker;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum DesktopMode {
    /// Use the signed-in desktop; application input may affect its cursor and focus.
    Shared,
    /// Use a retained local child desktop after explicit Windows setup.
    Separate,
}

#[derive(Debug, clap::Subcommand)]
pub enum DesktopCommand {
    /// Open Glass in the system tray to prepare the desktop before agent work.
    Open,
    /// Enable separate-desktop development; Windows may ask for administrator consent once.
    Enable,
    /// Read configuration and live environment state without starting or changing anything.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Stop the owned environment in this Windows logon.
    Stop,
    /// Stop owned work and restore the child-session setting changed by setup.
    Disable,
    /// Show the child viewer to inspect or resolve a desktop blocker.
    Show,
    /// Hide the child viewer and continue background work.
    Hide,
    /// Finish cleanup for an exited owner before starting another environment.
    Recover,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum SettingAction {
    Enable,
    Disable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    version: u32,
    enabled: bool,
    revision: String,
    setting_before: Option<bool>,
    owns_setting: bool,
    setup_pending: bool,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            version: 1,
            enabled: false,
            revision: "unconfigured".into(),
            setting_before: None,
            owns_setting: false,
            setup_pending: false,
        }
    }
}

impl Configuration {
    fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || (self.revision != "unconfigured"
                && (self.revision.len() != 32
                    || !self.revision.bytes().all(|byte| byte.is_ascii_hexdigit())))
        {
            return Err("unsupported or invalid Windows desktop configuration".into());
        }
        if self.owns_setting && self.setting_before != Some(false) {
            return Err("Windows child-setting ownership has no disabled baseline".into());
        }
        Ok(())
    }
}

struct Selection {
    preference: Option<DesktopMode>,
    separate_revision: Option<String>,
}

impl Selection {
    fn new(preference: Option<DesktopMode>) -> Self {
        Self {
            preference,
            separate_revision: None,
        }
    }

    fn select(&mut self, configuration: &Configuration) -> Result<DesktopMode, String> {
        if self.preference == Some(DesktopMode::Shared) {
            return Ok(DesktopMode::Shared);
        }
        configuration.validate()?;
        if configuration.setup_pending {
            return Err("Windows desktop setup is still pending".into());
        }
        let chosen = self.preference.unwrap_or(if configuration.enabled {
            DesktopMode::Separate
        } else {
            DesktopMode::Shared
        });
        if let Some(revision) = &self.separate_revision
            && (revision != &configuration.revision || chosen != DesktopMode::Separate)
        {
            return Err("Windows desktop selection changed; restart this MCP server before starting another application".into());
        }
        if chosen == DesktopMode::Separate {
            self.separate_revision
                .get_or_insert_with(|| configuration.revision.clone());
        }
        Ok(chosen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enrolled(revision: char) -> Configuration {
        Configuration {
            enabled: true,
            revision: revision.to_string().repeat(32),
            ..Configuration::default()
        }
    }

    #[test]
    fn disabling_separate_selection_never_routes_the_next_app_to_the_parent() {
        let mut selection = Selection::new(None);
        let mut configuration = enrolled('a');
        assert_eq!(selection.select(&configuration), Ok(DesktopMode::Separate));
        configuration.enabled = false;
        assert!(selection.select(&configuration).is_err());
    }

    #[test]
    fn reenrollment_does_not_revive_a_stopped_server_lease() {
        let mut selection = Selection::new(None);
        selection.select(&enrolled('a')).expect("initial selection");
        assert!(selection.select(&enrolled('b')).is_err());
    }

    #[test]
    fn explicit_shared_selection_is_independent_of_an_enrolled_default() {
        assert_eq!(
            Selection::new(Some(DesktopMode::Shared)).select(&enrolled('a')),
            Ok(DesktopMode::Shared)
        );
    }

    #[test]
    fn an_explicit_separate_server_still_refuses_a_later_configuration_change() {
        let mut selection = Selection::new(Some(DesktopMode::Separate));
        assert_eq!(
            selection.select(&Configuration::default()),
            Ok(DesktopMode::Separate)
        );
        assert!(selection.select(&enrolled('a')).is_err());
    }

    #[test]
    fn a_shared_server_can_pick_up_explicit_setup_before_its_next_application() {
        let mut selection = Selection::new(None);
        assert_eq!(
            selection.select(&Configuration::default()),
            Ok(DesktopMode::Shared)
        );
        assert_eq!(selection.select(&enrolled('a')), Ok(DesktopMode::Separate));
    }

    #[test]
    fn incomplete_or_unversioned_setup_never_admits_an_application() {
        let mut configuration = enrolled('a');
        configuration.setup_pending = true;
        assert!(Selection::new(None).select(&configuration).is_err());
        configuration.setup_pending = false;
        configuration.version = 2;
        assert!(Selection::new(None).select(&configuration).is_err());
    }
}
