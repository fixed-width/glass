//! Environment checks for the Windows accessibility backend ("glass doctor"). The pure
//! `a11y_checks` maps gathered facts to `Check`s and is unit-tested without UIA; `checks`
//! gathers the real environment (UIA is creatable).

use glass_core::{Check, CheckStatus};

/// Probe whether UI Automation is usable.
pub fn checks(_deep: bool) -> Vec<Check> {
    let uia_ok = probe_uia();
    a11y_checks(uia_ok)
}

/// Pure: build the a11y checks from gathered facts. `uia` is the result of actually
/// trying to create a UIA instance (`Ok` = creatable, `Err(reason)` = not).
fn a11y_checks(uia: std::result::Result<(), String>) -> Vec<Check> {
    vec![match &uia {
        Ok(()) => Check::new(
            "UI Automation",
            CheckStatus::Ok,
            "available — glass_a11y_snapshot / glass_a11y_marks / glass_click_element will work",
        ),
        Err(e) => Check::new("UI Automation", CheckStatus::Warn, format!("not available: {e}"))
            .with_remedy(
                "UI Automation could not be initialized. It ships with Windows; ensure glass runs \
                 in an interactive desktop session (not Session 0). Until then the a11y tools \
                 return AccessibilityUnavailable; the pixel loop (screenshot/click/type/diff) is \
                 unaffected.",
            ),
    }]
}

#[cfg(windows)]
fn probe_uia() -> std::result::Result<(), String> {
    match uiautomation::UIAutomation::new() {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
#[cfg(not(windows))]
fn probe_uia() -> std::result::Result<(), String> {
    Err("not a Windows host".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uia_ok_is_ok() {
        assert_eq!(a11y_checks(Ok(())).len(), 1);
        assert_eq!(a11y_checks(Ok(()))[0].status, CheckStatus::Ok);
    }
    #[test]
    fn uia_err_warns_with_remedy() {
        let c = &a11y_checks(Err("boom".into()))[0];
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.remedy.is_some());
    }
}
