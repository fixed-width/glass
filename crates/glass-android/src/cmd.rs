use glass_core::{GlassError, Result};

/// The launch target parsed from `AppSpec.run`.
///
/// Convention: the first element containing `/` that does not end in `.apk` is the
/// launch component `package/.Activity`; an element ending in `.apk` is installed first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchTarget {
    pub component: String,
    pub package: String,
    pub apk: Option<String>,
}

pub fn parse_launch(run: &[String]) -> Result<LaunchTarget> {
    let apk = run.iter().find(|a| a.ends_with(".apk")).cloned();
    let component = run
        .iter()
        .find(|a| a.contains('/') && !a.ends_with(".apk"))
        .cloned()
        .ok_or_else(|| {
            GlassError::AppNotStarted(
                "AppSpec.run must contain a launch component like \"com.example.app/.MainActivity\""
                    .into(),
            )
        })?;
    let package = component.split('/').next().unwrap_or_default().to_string();
    if package.is_empty() {
        return Err(GlassError::AppNotStarted(format!(
            "malformed component {component:?}; expected package/.Activity"
        )));
    }
    Ok(LaunchTarget {
        component,
        package,
        apk,
    })
}

pub fn install_args(apk: &str) -> Vec<String> {
    ["install", "-r", "-t", apk]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

pub fn launch_args(component: &str) -> Vec<String> {
    ["shell", "am", "start", "-W", "-n", component]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

pub fn force_stop_args(package: &str) -> Vec<String> {
    ["shell", "am", "force-stop", package]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::GlassError;

    #[test]
    fn parse_launch_extracts_component_and_package() {
        let run = vec!["com.example.app/.MainActivity".to_string()];
        let t = parse_launch(&run).unwrap();
        assert_eq!(t.component, "com.example.app/.MainActivity");
        assert_eq!(t.package, "com.example.app");
        assert_eq!(t.apk, None);
    }

    #[test]
    fn parse_launch_picks_up_apk_to_install() {
        let run = vec![
            "app/build/outputs/apk/debug/app-debug.apk".to_string(),
            "com.example.app/.MainActivity".to_string(),
        ];
        let t = parse_launch(&run).unwrap();
        assert_eq!(
            t.apk.as_deref(),
            Some("app/build/outputs/apk/debug/app-debug.apk")
        );
        assert_eq!(t.component, "com.example.app/.MainActivity");
    }

    #[test]
    fn parse_launch_requires_a_component() {
        let run = vec!["app-debug.apk".to_string()];
        assert!(matches!(
            parse_launch(&run),
            Err(GlassError::AppNotStarted(_))
        ));
    }

    #[test]
    fn arg_builders_are_exact() {
        assert_eq!(install_args("x.apk"), ["install", "-r", "-t", "x.apk"]);
        assert_eq!(
            launch_args("p/.A"),
            ["shell", "am", "start", "-W", "-n", "p/.A"]
        );
        assert_eq!(force_stop_args("p"), ["shell", "am", "force-stop", "p"]);
    }
}
