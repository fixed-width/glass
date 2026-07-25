use glass_core::{GlassError, Result};

/// The launch target parsed from `AppSpec.run`.
///
/// Convention: the first element containing `/` that does not end in `.apk` is the
/// launch component `package/.Activity`; an element ending in `.apk` is installed first.
/// Those two are the whole vocabulary — see [`parse_launch`] for why nothing else is allowed.
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

    // Anything the two rules above did not consume has nowhere to go: `am start` takes intent
    // extras (`-e key value`), not a program's argument vector, so a trailing element is not
    // something this backend can honour. Say so rather than launching a differently-configured
    // app than the caller asked for and reporting success — the other backends really do pass
    // `run[1..]` to the process, which is exactly why silence here would mislead.
    let leftover: Vec<&str> = run
        .iter()
        .map(String::as_str)
        .filter(|a| Some(*a) != apk.as_deref() && *a != component)
        .collect();
    if !leftover.is_empty() {
        return Err(GlassError::AppNotStarted(format!(
            "AppSpec.run carries {} this backend cannot pass on: {}. Android launches an \
             activity rather than a command line — `am start` takes intent extras, not program \
             arguments — so put launch configuration in spec.env, which reaches the app",
            if leftover.len() == 1 {
                "an element"
            } else {
                "elements"
            },
            leftover.join(", ")
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

    #[test]
    fn an_element_that_is_neither_the_apk_nor_the_component_is_an_error() {
        // `am start` takes intent extras, not app arguments, so there is nowhere for a trailing
        // element to go. Dropping it quietly would launch a differently-configured app than the
        // caller asked for and report success.
        let run = vec![
            "com.example.app/.MainActivity".to_string(),
            "--tab=collection".to_string(),
        ];
        let err = parse_launch(&run).expect_err("a leftover element must not be ignored");
        let message = err.to_string();
        assert!(
            message.contains("--tab=collection"),
            "the error must name what it could not use, got: {message}"
        );
    }

    #[test]
    fn the_error_points_at_the_env_route_that_does_work() {
        let run = vec![
            "com.example.app/.MainActivity".to_string(),
            "extra".to_string(),
        ];
        let message = parse_launch(&run).unwrap_err().to_string();
        assert!(
            message.contains("env"),
            "the error must say where launch configuration does go, got: {message}"
        );
    }

    #[test]
    fn an_apk_and_a_component_together_are_still_accepted() {
        let run = vec![
            "/tmp/app.apk".to_string(),
            "com.example.app/.MainActivity".to_string(),
        ];
        let target = parse_launch(&run).expect("apk + component is the documented shape");
        assert_eq!(target.apk.as_deref(), Some("/tmp/app.apk"));
    }
}
