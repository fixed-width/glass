use glass_core::{GlassError, Result};
use windows::Win32::Security::Authentication::Identity::SLGetWindowsInformationDWORD;
use windows::core::w;

/// Read Remote Desktop hosting entitlement without changing the listener, firewall or child setting.
pub fn rdp_host_licensed() -> Result<bool> {
    // SAFETY: the policy name is a static NUL-terminated string; the binding owns output storage.
    let value = unsafe {
        SLGetWindowsInformationDWORD(w!(
            "TerminalServices-RemoteConnectionManager-AllowRemoteConnections"
        ))
    };
    hosting_entitlement(value)
}

fn hosting_entitlement(value: windows::core::Result<u32>) -> Result<bool> {
    match value {
        Ok(0) => Ok(false),
        Ok(1) => Ok(true),
        Ok(value) => Err(GlassError::Backend(format!(
            "Windows returned an unknown Remote Desktop hosting entitlement value ({value})"
        ))),
        Err(error) => Err(GlassError::Backend(format!(
            "Windows could not check Remote Desktop hosting availability (HRESULT 0x{:08X}): {error}",
            error.code().0 as u32
        ))),
    }
}

/// Check host entitlement and the client control without creating a session or changing settings.
/// Success is a prerequisite for child creation, rather than proof that sign-in will succeed.
pub fn child_desktop_available() -> Result<()> {
    if !rdp_host_licensed()? {
        return Err(GlassError::Unsupported(
            "This edition of Windows does not include Remote Desktop hosting. Choose On your desktop."
                .into(),
        ));
    }
    super::rdp_control_available()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::{Error, HRESULT};

    #[test]
    fn unlicensed_host_is_distinct_from_a_disabled_child_setting() {
        assert!(!hosting_entitlement(Ok(0)).unwrap());
    }

    #[test]
    fn licensed_host_does_not_require_an_enabled_network_listener() {
        assert!(hosting_entitlement(Ok(1)).unwrap());
    }

    #[test]
    fn unknown_entitlement_is_not_assumed_supported() {
        assert!(hosting_entitlement(Ok(2)).is_err());
    }

    #[test]
    fn missing_policy_preserves_the_native_error_instead_of_becoming_supported() {
        let error = hosting_entitlement(Err(Error::from_hresult(HRESULT(0xC004F012u32 as i32))))
            .unwrap_err();
        assert!(error.to_string().contains("HRESULT 0xC004F012"));
    }

    #[test]
    fn denied_query_is_not_misreported_as_an_unsupported_edition() {
        let error = hosting_entitlement(Err(Error::from_hresult(HRESULT(0x80070005u32 as i32))))
            .unwrap_err();
        assert!(error.to_string().contains("HRESULT 0x80070005"));
    }
}
