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

/// Whether `s` is shaped like a launch component — `package/.Activity` or `package/a.b.Class`.
///
/// Checked rather than assumed: the old rule was "contains a slash", which claimed anything with
/// one — a stray `--tab=a/b` became the component, and the real component was then reported as
/// the element that could not be used. A package is a Java-style dotted identifier, so the
/// charset rules it out.
fn is_component(s: &str) -> bool {
    let Some((package, activity)) = s.split_once('/') else {
        return false;
    };
    if package.is_empty() || activity.is_empty() || activity.contains('/') {
        return false;
    }
    let package_ok = package.starts_with(|c: char| c.is_ascii_alphabetic())
        && package
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
    // An activity is `.Relative`, `absolute.Class`, or an inner class with `$`.
    let activity_ok = activity
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '$');
    package_ok && activity_ok
}

/// Parse `AppSpec.run` into the launch target.
///
/// One classifying pass, so every element is accounted for: an `.apk` to install, the
/// `package/.Activity` component, and nothing else. Anything unclassifiable — or a second apk or
/// component — is an error naming the element itself, because Android launches an activity rather
/// than a command line. `am start` takes intent extras, not a program's argument vector, so there
/// is nowhere for a trailing element to go — and glass does not map extras today, so there is no
/// route to offer instead: `spec.env` configures the build command on the host, not the launched
/// app, which is forked from zygote and never sees the shell's environment. Dropping the element
/// quietly would launch a differently configured app than the caller asked for and report success.
pub fn parse_launch(run: &[String]) -> Result<LaunchTarget> {
    let mut apk: Option<&str> = None;
    let mut component: Option<&str> = None;

    for element in run {
        let element = element.as_str();
        if element.ends_with(".apk") {
            if let Some(first) = apk {
                return Err(GlassError::AppNotStarted(format!(
                    "AppSpec.run names two APKs to install, {first:?} and {element:?}; it takes \
                     at most one"
                )));
            }
            apk = Some(element);
        } else if is_component(element) {
            if let Some(first) = component {
                return Err(GlassError::AppNotStarted(format!(
                    "AppSpec.run names two launch components, {first:?} and {element:?}; it takes \
                     exactly one"
                )));
            }
            component = Some(element);
        } else {
            return Err(GlassError::AppNotStarted(format!(
                "AppSpec.run carries {element:?}, which is neither an APK to install nor a \
                 package/.Activity component. Android launches an activity rather than a command \
                 line, so there is no argument vector to put it in: `am start` would need it as \
                 an intent extra, which this backend does not map yet. Drop it, or have the app \
                 read the setting from somewhere it can reach"
            )));
        }
    }

    let component = component.ok_or_else(|| {
        GlassError::AppNotStarted(
            "AppSpec.run must contain a launch component like \"com.example.app/.MainActivity\""
                .into(),
        )
    })?;
    let package = component
        .split('/')
        .next()
        .expect("is_component proved a non-empty package before the slash")
        .to_string();

    Ok(LaunchTarget {
        component: component.to_string(),
        package,
        apk: apk.map(str::to_string),
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
    fn the_error_says_why_rather_than_naming_a_route_that_does_not_work() {
        // An earlier draft sent the caller to `spec.env`. That is wrong on Android: env
        // configures the build command on the host, and an app forked from zygote never sees it,
        // so the advice would have been one more thing that silently does nothing.
        let run = vec![
            "com.example.app/.MainActivity".to_string(),
            "extra".to_string(),
        ];
        let message = parse_launch(&run).unwrap_err().to_string();
        assert!(
            message.contains("intent extra"),
            "the error must name what Android would need, got: {message}"
        );
        assert!(
            !message.contains("spec.env"),
            "the error must not route to env, which does not reach the app: {message}"
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

    #[test]
    fn a_stray_element_is_named_rather_than_the_real_component() {
        // The old rule claimed anything containing a slash as the component, so this reported the
        // real component as the element it could not use — a diagnostic pointing at the one thing
        // that was right.
        let run = vec![
            "--tab=a/b".to_string(),
            "com.example.app/.MainActivity".to_string(),
        ];
        let message = parse_launch(&run).unwrap_err().to_string();
        assert!(
            message.contains("--tab=a/b"),
            "the error must name the stray element, got: {message}"
        );
    }

    #[test]
    fn a_repeated_component_is_an_error_rather_than_silently_one() {
        let run = vec![
            "com.example.app/.MainActivity".to_string(),
            "com.example.app/.MainActivity".to_string(),
        ];
        let message = parse_launch(&run).unwrap_err().to_string();
        assert!(
            message.contains("two launch components"),
            "a duplicate must not be absorbed, got: {message}"
        );
    }

    #[test]
    fn two_apks_are_an_error() {
        let run = vec![
            "/tmp/one.apk".to_string(),
            "/tmp/two.apk".to_string(),
            "com.example.app/.MainActivity".to_string(),
        ];
        let message = parse_launch(&run).unwrap_err().to_string();
        assert!(message.contains("two APKs"), "got: {message}");
    }

    #[test]
    fn a_component_with_a_second_slash_is_not_a_component() {
        let run = vec!["com.example.app/.Main/extra".to_string()];
        assert!(parse_launch(&run).is_err());
    }

    #[test]
    fn a_slash_with_nothing_after_it_is_not_a_component() {
        // The charset check passes vacuously on an empty activity, so this is the one part of
        // the shape that only the emptiness guard rules out. Admitting it would run
        // `am start -n com.example/`, which names no activity at all.
        let run = vec!["com.example/".to_string()];
        let message = parse_launch(&run).unwrap_err().to_string();
        assert!(message.contains("com.example/"), "got: {message}");
    }

    #[test]
    fn a_package_that_does_not_begin_with_a_letter_is_not_a_component() {
        // The charset alone would admit this: `1abc` is entirely alphanumeric. A Java package
        // cannot start with a digit, and only the leading-letter check says so.
        let run = vec!["1abc/.Main".to_string()];
        let message = parse_launch(&run).unwrap_err().to_string();
        assert!(message.contains("1abc/.Main"), "got: {message}");
    }

    #[test]
    fn underscores_belong_to_both_halves_of_a_component() {
        let run = vec!["com.my_app/.My_Activity".to_string()];
        let target = parse_launch(&run).expect("underscores are legal Java identifiers");
        assert_eq!(target.package, "com.my_app");
        assert_eq!(target.component, "com.my_app/.My_Activity");
    }

    #[test]
    fn an_inner_class_activity_keeps_its_dollar_sign() {
        // `pkg/.Outer$Inner` is how an activity declared as an inner class is named.
        let run = vec!["com.example/.Outer$Inner".to_string()];
        let target = parse_launch(&run).expect("an inner class is a legal activity");
        assert_eq!(target.component, "com.example/.Outer$Inner");
    }

    #[test]
    fn a_hyphen_keeps_an_element_out_of_both_halves() {
        // A hyphen is legal in neither, and it is what the flags a caller might pass carry.
        for stray in ["com.my-app/.Main", "com.example/.My-Activity"] {
            let message = parse_launch(&[stray.to_string()]).unwrap_err().to_string();
            assert!(message.contains(stray), "got: {message}");
        }
    }
}
