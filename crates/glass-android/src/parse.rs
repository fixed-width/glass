use glass_core::{GlassError, Result, WindowGeometry};

/// Extract the focused app window frame from `dumpsys window windows`.
/// Finds the first `Window{...package...}` block, then its first `[l,t][r,b]` frame.
pub fn parse_window_frame(dump: &str, package: &str) -> Result<WindowGeometry> {
    let mut in_block = false;
    for line in dump.lines() {
        let t = line.trim_start();
        if t.starts_with("Window #") || t.starts_with("Window{") {
            in_block = t.contains(package);
        }
        if in_block {
            // Matches both `mFrame=` and `frame=` (the former contains the latter's tail).
            if let Some(idx) = line.find("Frame=").or_else(|| line.find("frame=")) {
                let after = &line[idx + "Frame=".len()..];
                if let Some(geo) = parse_rect(after) {
                    return Ok(geo);
                }
            }
        }
    }
    Err(GlassError::WindowNotFound)
}

/// Parse a leading `[left,top][right,bottom]` rectangle into a geometry.
fn parse_rect(s: &str) -> Option<WindowGeometry> {
    let s = s.trim_start().strip_prefix('[')?;
    let (lt, rest) = s.split_once(']')?;
    let (l, t) = lt.split_once(',')?;
    let rest = rest.trim_start().strip_prefix('[')?;
    let (rb, _) = rest.split_once(']')?;
    let (r, b) = rb.split_once(',')?;
    let l: i32 = l.trim().parse().ok()?;
    let t: i32 = t.trim().parse().ok()?;
    let r: i32 = r.trim().parse().ok()?;
    let b: i32 = b.trim().parse().ok()?;
    Some(WindowGeometry {
        x: l,
        y: t,
        width: (r - l).max(0) as u32,
        height: (b - t).max(0) as u32,
    })
}

/// `adb install` → Ok on `Success`, else `AppNotStarted` with the failure reason.
pub fn check_install(output: &str) -> Result<()> {
    if output.lines().any(|l| l.trim() == "Success") {
        return Ok(());
    }
    let reason = output
        .lines()
        .find(|l| l.contains("INSTALL_FAILED") || l.contains("Failure"))
        .unwrap_or_else(|| output.trim());
    Err(GlassError::AppNotStarted(format!("adb install failed: {}", reason.trim())))
}

/// `am start -W` → Err on an `Error:`/`Error type` line, else Ok.
pub fn check_am_start(output: &str) -> Result<()> {
    if let Some(err) = output
        .lines()
        .find(|l| l.trim_start().starts_with("Error:") || l.contains("Error type"))
    {
        return Err(GlassError::AppNotStarted(format!("am start failed: {}", err.trim())));
    }
    Ok(())
}

/// First pid from `pidof <pkg>` output.
pub fn parse_pid(output: &str) -> Option<u32> {
    output.split_whitespace().next()?.parse().ok()
}

/// All pids from `pidof <pkg>` output.
pub fn parse_pids(output: &str) -> Vec<u32> {
    output.split_whitespace().filter_map(|t| t.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{GlassError, WindowGeometry};

    // Trimmed, representative `dumpsys window windows` excerpt.
    const DUMP: &str = "  Window #2 Window{abcd u0 com.example.app/com.example.app.MainActivity}:\n\
                        \x20   mDisplayId=0\n\
                        \x20   mFrame=[0,63][1080,2220] mLastFrame=[0,63][1080,2220]\n\
                        \x20 Window #3 Window{ef01 u0 StatusBar}:\n\
                        \x20   mFrame=[0,0][1080,63]\n";

    #[test]
    fn window_frame_for_package_is_origin_and_size() {
        let g = parse_window_frame(DUMP, "com.example.app").unwrap();
        assert_eq!(g, WindowGeometry { x: 0, y: 63, width: 1080, height: 2157 });
    }

    #[test]
    fn window_frame_missing_package_is_window_not_found() {
        assert!(matches!(parse_window_frame(DUMP, "com.other"), Err(GlassError::WindowNotFound)));
    }

    #[test]
    fn install_success_is_ok_else_error() {
        assert!(check_install("Performing Streamed Install\nSuccess\n").is_ok());
        let err = check_install("Failure [INSTALL_FAILED_INVALID_APK]\n").unwrap_err();
        assert!(err.to_string().contains("INSTALL_FAILED_INVALID_APK"));
    }

    #[test]
    fn am_start_error_is_detected() {
        assert!(check_am_start("Starting: Intent {...}\nStatus: ok\n").is_ok());
        let err = check_am_start("Starting: Intent {...}\nError type 3\nError: Activity not started\n").unwrap_err();
        assert!(matches!(err, GlassError::AppNotStarted(_)));
    }

    #[test]
    fn pid_parsing() {
        assert_eq!(parse_pid("4321\n"), Some(4321));
        assert_eq!(parse_pid("\n"), None);
        assert_eq!(parse_pids("4321 4322 4400\n"), vec![4321, 4322, 4400]);
    }
}
