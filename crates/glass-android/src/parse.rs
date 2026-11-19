use glass_core::{GlassError, Result, WindowGeometry};

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

/// One on-screen window owned by the app, parsed from `dumpsys window windows`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedWindow {
    pub id: u64,
    pub title: String,
    pub frame: WindowGeometry,
}

struct WinBlock {
    id: u64,
    title: String,
    package: Option<String>,
    owner_metadata_seen: bool,
    on_screen: bool,
    frame: Option<WindowGeometry>,
    visible: bool,
    display_id: Option<u32>,
    session: Option<(u64, u32)>,
    client: Option<u64>,
    transformed: bool,
}

impl WinBlock {
    /// The transient starting window, title `Splash Screen <pkg>` — a real app-package window
    /// during launch, but not one the agent should drive.
    fn is_splash(&self) -> bool {
        self.title.starts_with("Splash Screen")
    }
}

/// Parse a `Window #N Window{<hash> u0 <name>}:` header into (hash-as-u64, name).
fn parse_window_header(t: &str) -> Option<(u64, String)> {
    if !t.starts_with("Window #") {
        return None;
    }
    let brace = t.find("Window{")?;
    parse_window_ref(&t[brace..])
}

fn parse_window_ref(s: &str) -> Option<(u64, String)> {
    let inner = s.strip_prefix("Window{")?;
    let inner = inner.trim_end().strip_suffix(':').unwrap_or(inner);
    let inner = inner.trim_end().strip_suffix('}').unwrap_or(inner);
    let mut parts = inner.splitn(3, ' ');
    let hash = parts.next()?;
    let _user = parts.next()?; // "u0"
    let name = parts.next().unwrap_or("").trim().to_string();
    let id = u64::from_str_radix(hash, 16).ok()?;
    Some((id, name))
}

/// Every `Window #N` block in a `dumpsys window windows` dump, in z-order (topmost first).
///
/// Package-agnostic so [`parse_app_windows`] and [`describe_missing_window`] share one parse — a
/// description derived from a second could contradict the search.
fn parse_blocks(dump: &str) -> Vec<WinBlock> {
    let mut out = Vec::new();
    let mut cur: Option<WinBlock> = None;
    for line in dump.lines() {
        let t = line.trim_start();
        if let Some((id, title)) = parse_window_header(t) {
            out.extend(cur.take());
            cur = Some(WinBlock {
                id,
                title,
                package: None,
                owner_metadata_seen: false,
                on_screen: false,
                frame: None,
                visible: false,
                display_id: None,
                session: None,
                client: None,
                transformed: false,
            });
            continue;
        }
        // A non-blank, less-indented line ends the window list / current block, so trailing
        // dumpsys sections (e.g. the `#0 Window{…}` focus summary, which carries the foreground
        // app's `package=`) don't leak into the last real window block.
        let indent = line.len() - t.len();
        if !t.is_empty() && indent < 4 {
            out.extend(cur.take());
            continue;
        }
        if let Some(b) = cur.as_mut() {
            if let Some(owner) = t.strip_prefix("mOwnerUid=") {
                b.package = if std::mem::replace(&mut b.owner_metadata_seen, true) {
                    None
                } else {
                    owner
                        .split_whitespace()
                        .next()
                        .and_then(|uid| uid.parse::<u32>().ok())
                        .and_then(|_| field(t, "package="))
                        .filter(|package| !package.is_empty())
                        .map(str::to_owned)
                };
            } else if !b.owner_metadata_seen
                && let Some(package) = t.strip_prefix("package=")
            {
                b.package = package.split_whitespace().next().map(str::to_owned);
            }
            if line.contains("isOnScreen=true") {
                b.on_screen = true;
            }
            if t == "isVisible=true" {
                b.visible = true;
            }
            if let Some(display) = t.strip_prefix("mDisplayId=") {
                b.display_id = display
                    .split_whitespace()
                    .next()
                    .and_then(|id| id.parse().ok());
                b.session = parse_window_session(display);
                b.client = field(display, "mClient=")
                    .and_then(|client| client.strip_prefix("android.os.BinderProxy@"))
                    .and_then(|token| u64::from_str_radix(token, 16).ok());
            }
            let pending_rotation = t
                .split_once("seamlesslyRotate: pending=")
                .is_some_and(|(_, pending)| pending.split_whitespace().next() != Some("null"));
            if pending_rotation
                || [
                    "mCompatFrame=",
                    "mHScale=",
                    "mVScale=",
                    "mXOffset=",
                    "mYOffset=",
                    "mOrientationChanging=",
                    "mAnimatingExit=",
                ]
                .iter()
                .any(|prefix| field(t, prefix).is_some())
            {
                b.transformed = true;
            }
            if b.frame.is_none() {
                // The window's frame is `mFrame=[..]` on older images, or the `frame=[..]`
                // field of the modern `Frames: parent=.. display=.. frame=.. last=..` line.
                // Prefer the explicit `mFrame=`; on the `Frames:` line the bare (lowercase)
                // `frame=` is the window's own frame (parent/display/last use their own keys).
                let at = line
                    .find("mFrame=")
                    .map(|i| i + "mFrame=".len())
                    .or_else(|| line.find("frame=").map(|i| i + "frame=".len()));
                if let Some(idx) = at {
                    b.frame = parse_rect(&line[idx..]);
                }
            }
        }
    }
    out.extend(cur.take());
    out
}

fn field<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(prefix))
}

fn parse_window_session(line: &str) -> Option<(u64, u32)> {
    let (session, _) = line.split_once(" mSession=Session{")?.1.split_once('}')?;
    let (token, owner) = session.split_once(' ')?;
    let token = u64::from_str_radix(token, 16).ok()?;
    let owner = owner
        .split_once(':')?
        .0
        .parse::<std::num::NonZeroU32>()
        .ok()?;
    Some((token, owner.get()))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ObservationWindowState {
    pub window: u64,
    pub owner_pid: u32,
    session: u64,
    client: u64,
    pub frame: WindowGeometry,
    boot: String,
    system_server: u32,
    display: (u32, u32),
    pub rotation: u8,
}

pub(crate) const OBSERVATION_DUMP: &str = "cat /proc/sys/kernel/random/boot_id && pidof system_server && dumpsys window displays && dumpsys window windows";

/// Pair the sole displayed window of the requested app with the default display's focus and rotation.
pub(crate) fn parse_observation_window(
    dump: &str,
    package: &str,
    selected: u64,
) -> Option<ObservationWindowState> {
    let mut lines = dump.splitn(3, '\n');
    let boot = lines.next()?.trim();
    let system_server = lines.next()?.trim().parse::<std::num::NonZeroU32>().ok()?;
    let (displays, windows) = lines.next()?.split_once("WINDOW MANAGER WINDOWS")?;
    let mut display_ids = displays
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("Display: mDisplayId="));
    if display_ids.next()?.split_whitespace().next()? != "0" || display_ids.next().is_some() {
        return None;
    }
    let display_line = displays
        .lines()
        .find(|line| line.trim_start().starts_with("init="))?;
    let (width, height) = field(display_line, "cur=")?.split_once('x')?;
    let display = (width.parse::<u32>().ok()?, height.parse::<u32>().ok()?);
    let rotation = displays
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("mRotation="))?
        .split_whitespace()
        .next()?
        .parse::<u8>()
        .ok()?;
    let focus = displays
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("mCurrentFocus="))?;
    let (focused, _) = parse_window_ref(focus)?;
    if !is_boot_id(boot)
        || rotation > 3
        || focused != selected
        || field(display_line, "base=").is_some()
    {
        return None;
    }
    let mut candidate = None;
    for window in parse_blocks(windows)
        .into_iter()
        .filter(|window| window.on_screen)
    {
        if !window.owner_metadata_seen {
            return None;
        }
        if window.package.as_deref()? != package {
            continue;
        }
        if candidate.replace(window).is_some() {
            return None;
        }
    }
    let window = candidate?;
    if window.id != selected
        || !window.visible
        || window.is_splash()
        || window.transformed
        || window.display_id != Some(0)
    {
        return None;
    }
    let (session, owner_pid) = window.session?;
    let frame = window.frame?;
    if frame.width == 0
        || frame.height == 0
        || u32::try_from(frame.x).ok()?.checked_add(frame.width)? > display.0
        || u32::try_from(frame.y).ok()?.checked_add(frame.height)? > display.1
    {
        return None;
    }
    Some(ObservationWindowState {
        window: selected,
        owner_pid,
        session,
        client: window.client?,
        frame,
        boot: boot.to_owned(),
        system_server: system_server.get(),
        display,
        rotation,
    })
}

fn is_boot_id(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(at, byte)| match at {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// All on-screen windows owned by `package`, in dumpsys z-order (topmost first).
pub fn parse_app_windows(dump: &str, package: &str) -> Vec<ParsedWindow> {
    let mut out = Vec::new();
    for b in parse_blocks(dump) {
        if b.package.as_deref() != Some(package) || !b.on_screen || b.is_splash() {
            continue;
        }
        if let Some(frame) = b.frame {
            out.push(ParsedWindow {
                id: b.id,
                title: b.title,
                frame,
            });
        }
    }
    out
}

/// Packages owning an on-screen window other than `exclude`, deduped, in z-order.
fn on_screen_packages(blocks: &[WinBlock], exclude: &str) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for b in blocks.iter().filter(|b| b.on_screen) {
        if let Some(p) = b.package.as_deref()
            && p != exclude
            && !seen.contains(&p)
        {
            seen.push(p);
        }
    }
    if seen.is_empty() {
        return "nothing else is on screen".to_string();
    }
    format!("on screen: {}", seen.join(", "))
}

/// Why [`parse_app_windows`] found nothing for `package`, read off the dump it just searched.
///
/// It rejects a block for one of four reasons an agent would act on differently, and an empty
/// result tells none of them apart: no window opened, the window is behind another app's, it is
/// still a splash screen, or the dump carried no frame (glass#338).
pub fn describe_missing_window(dump: &str, package: &str) -> String {
    let blocks = parse_blocks(dump);
    let mine: Vec<&WinBlock> = blocks
        .iter()
        .filter(|b| b.package.as_deref() == Some(package))
        .collect();

    if mine.is_empty() {
        return format!(
            "no window belongs to it yet; {}",
            on_screen_packages(&blocks, package)
        );
    }
    if !mine.iter().any(|b| b.on_screen) {
        return format!(
            "its window is not on screen; {}",
            on_screen_packages(&blocks, package)
        );
    }
    if !mine.iter().any(|b| b.on_screen && !b.is_splash()) {
        return "only its splash screen is on screen".to_string();
    }
    // The other three reasons are eliminated, so the frame is the one left.
    "its window is on screen but the dump carried no frame for it".to_string()
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
    Err(GlassError::AppNotStarted(format!(
        "adb install failed: {}",
        reason.trim()
    )))
}

/// `am start -W` → Err on an `Error:`/`Error type` line, else Ok.
pub fn check_am_start(output: &str) -> Result<()> {
    if let Some(err) = output
        .lines()
        .find(|l| l.trim_start().starts_with("Error:") || l.contains("Error type"))
    {
        return Err(GlassError::AppNotStarted(format!(
            "am start failed: {}",
            err.trim()
        )));
    }
    Ok(())
}

/// First pid from `pidof <pkg>` output.
pub fn parse_pid(output: &str) -> Option<u32> {
    output.split_whitespace().next()?.parse().ok()
}

/// All pids from `pidof <pkg>` output.
pub fn parse_pids(output: &str) -> Vec<u32> {
    output
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect()
}

#[cfg(test)]
pub(crate) const OBSERVATION_FIXTURE: &str = "\
53e9ea51-9288-45ec-889e-258e3299b6a7
561
WINDOW MANAGER DISPLAY CONTENTS (dumpsys window displays)
  Display: mDisplayId=0
    init=1080x2400 420dpi cur=1080x2400 app=1080x2322
  mCurrentFocus=Window{abc u0 com.example.app/com.example.app.Main}
    mRotation=0 mDeferredRotationPauseCount=0
WINDOW MANAGER WINDOWS (dumpsys window windows)
  Window #0 Window{abc u0 com.example.app/com.example.app.Main}:
    mDisplayId=0 rootTaskId=5 mSession=Session{def 4321:u0a155} mClient=android.os.BinderProxy@bbb
    mOwnerUid=10155 showForAllUsers=false package=com.example.app appop=NONE
    Frames: parent=[0,0][1080,2400] display=[0,0][1080,2400] frame=[0,0][1080,2400]
    isOnScreen=true
    isVisible=true
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_reads_the_focused_window_owner_and_default_display() {
        let scope =
            parse_observation_window(OBSERVATION_FIXTURE, "com.example.app", 0xabc).unwrap();
        assert_eq!(scope.owner_pid, 4321);
        assert_eq!(scope.system_server, 561);
        assert_eq!(scope.session, 0xdef);
        assert_eq!(scope.rotation, 0);
        assert_eq!((scope.frame.width, scope.frame.height), (1080, 2400));

        let rotated = OBSERVATION_FIXTURE
            .replace("mRotation=0", "mRotation=3")
            .replace(
                "    isOnScreen=true",
                "    mForceSeamlesslyRotate=false seamlesslyRotate: pending=null    isOnScreen=true",
            );
        assert_eq!(
            parse_observation_window(&rotated, "com.example.app", 0xabc)
                .unwrap()
                .rotation,
            3
        );
    }

    #[test]
    fn observation_refuses_ambiguous_or_incompatible_window_metadata() {
        for (old, new) in [
            ("53e9ea51-9288-45ec-889e-258e3299b6a7", "unknown"),
            (
                "53e9ea51-9288-45ec-889e-258e3299b6a7",
                "z3e9ea51-9288-45ec-889e-258e3299b6a7",
            ),
            ("561\nWINDOW", "0\nWINDOW"),
            ("mCurrentFocus=Window{abc", "mCurrentFocus=Window{333"),
            ("Display: mDisplayId=0", "Display: mDisplayId=1"),
            (
                "Display: mDisplayId=0",
                "Display: mDisplayId=0\n  Display: mDisplayId=1",
            ),
            ("mRotation=0", "mRotation=4"),
            ("Window #0 Window{abc", "Window #0 Window{baa"),
            (
                "Window #0 Window{abc u0 com.example.app/com.example.app.Main}",
                "Window #0 Window{abc u0 Splash Screen com.example.app}",
            ),
            (" cur=", " base=640x640 cur="),
            ("isVisible=true", "isVisible=false"),
            ("mDisplayId=0 rootTaskId", "mDisplayId=1 rootTaskId"),
            ("    isVisible=true", "    mHScale=2\n    isVisible=true"),
            (
                "    isVisible=true",
                "    mForceSeamlesslyRotate=false seamlesslyRotate: pending=SeamlessRotator{}\n    isVisible=true",
            ),
            ("Session{def 4321:u0a155}", "Session{def 0:u0a155}"),
            ("mClient=android.os.BinderProxy@bbb", "mClient=null"),
            ("frame=[0,0][1080,2400]", "frame=[0,0][1081,2400]"),
            ("frame=[0,0][1080,2400]", "frame=[0,0][1080,2401]"),
            ("frame=[0,0][1080,2400]", "frame=[0,0][0,2400]"),
            ("frame=[0,0][1080,2400]", "frame=[0,0][1080,0]"),
        ] {
            let dump = OBSERVATION_FIXTURE.replace(old, new);
            assert_eq!(
                parse_observation_window(&dump, "com.example.app", 0xabc),
                None,
                "{new}"
            );
        }
        let (_, window) = OBSERVATION_FIXTURE.split_once("  Window #0").unwrap();
        let popup = format!(
            "{OBSERVATION_FIXTURE}  Window #1{}",
            window.replace("abc", "baa")
        );
        assert!(parse_observation_window(&popup, "com.example.app", 0xabc).is_none());
        assert!(parse_observation_window(OBSERVATION_FIXTURE, "com.other", 0xabc).is_none());
    }

    #[test]
    fn observation_refuses_inline_window_scale_fields() {
        for scale in ["mHScale=0.75", "mVScale=0.75"] {
            let dump = OBSERVATION_FIXTURE.replace(
                "    isOnScreen=true",
                &format!(
                    "    mForceSeamlesslyRotate=false seamlesslyRotate: pending=null    {scale} isOnScreen=true"
                ),
            );
            assert!(
                parse_observation_window(&dump, "com.example.app", 0xabc).is_none(),
                "{scale}"
            );
        }
    }

    #[test]
    fn window_titles_cannot_override_owner_metadata_or_hide_an_app_popup() {
        let popup = "\
  Window #1 Window{baa u0 Popup}:
    mOwnerUid=10155 showForAllUsers=false package=com.example.app appop=NONE
    mFrame=[0,0][1080,2400]
    mWinAnimator=WindowStateAnimator{788 Popup package=com.other}
    isOnScreen=true
    isVisible=true
";
        let dump = format!("{OBSERVATION_FIXTURE}{popup}");
        assert_eq!(parse_app_windows(&dump, "com.example.app").len(), 2);
        assert!(parse_observation_window(&dump, "com.example.app", 0xabc).is_none());

        let other = popup
            .replace(
                "package=com.example.app appop=NONE",
                "package=com.other appop=NONE",
            )
            .replace("Popup package=com.other", "Popup package=com.example.app");
        let dump = format!("{OBSERVATION_FIXTURE}{other}");
        assert_eq!(parse_app_windows(&dump, "com.example.app").len(), 1);
        assert!(parse_observation_window(&dump, "com.example.app", 0xabc).is_some());
    }

    #[test]
    fn observation_refuses_on_screen_windows_with_unknown_ownership() {
        let (_, window) = OBSERVATION_FIXTURE.split_once("  Window #0").unwrap();
        for owner in [
            "package=com.other",
            "mOwnerUid=unknown package=com.other",
            "mOwnerUid=10155",
            "mOwnerUid=10155 package=com.other\n    mOwnerUid=10155 package=com.other",
        ] {
            let other = window.replace("abc", "baa").replace(
                "mOwnerUid=10155 showForAllUsers=false package=com.example.app appop=NONE",
                owner,
            );
            let dump = format!("{OBSERVATION_FIXTURE}  Window #1{other}");
            assert!(
                parse_observation_window(&dump, "com.example.app", 0xabc).is_none(),
                "{owner}"
            );
        }
    }
    use glass_core::GlassError;

    #[test]
    fn install_success_is_ok_else_error() {
        assert!(check_install("Performing Streamed Install\nSuccess\n").is_ok());
        let err = check_install("Failure [INSTALL_FAILED_INVALID_APK]\n").unwrap_err();
        assert!(err.to_string().contains("INSTALL_FAILED_INVALID_APK"));
    }

    #[test]
    fn either_marker_alone_still_picks_the_reason_out_of_the_progress_lines() {
        // Each line here carries one marker and not the other; requiring both would find none
        // and fall back to the whole output, burying the reason in the progress lines.
        for (output, reason) in [
            (
                "Performing Streamed Install\nFailure [INSTALL_PARSE_FAILED_NO_CERTIFICATES]\n",
                "Failure [INSTALL_PARSE_FAILED_NO_CERTIFICATES]",
            ),
            (
                "Performing Streamed Install\nError [INSTALL_FAILED_INSUFFICIENT_STORAGE]\n",
                "Error [INSTALL_FAILED_INSUFFICIENT_STORAGE]",
            ),
        ] {
            let message = check_install(output).unwrap_err().to_string();
            assert!(message.contains(reason), "got: {message}");
            assert!(
                !message.contains("Performing Streamed Install"),
                "the reason must be the failing line, not the whole output: {message}"
            );
        }
    }

    #[test]
    fn am_start_error_is_detected() {
        assert!(check_am_start("Starting: Intent {...}\nStatus: ok\n").is_ok());
        let err =
            check_am_start("Starting: Intent {...}\nError type 3\nError: Activity not started\n")
                .unwrap_err();
        assert!(matches!(err, GlassError::AppNotStarted(_)));
    }

    #[test]
    fn pid_parsing() {
        assert_eq!(parse_pid("4321\n"), Some(4321));
        assert_eq!(parse_pid("\n"), None);
        assert_eq!(parse_pids("4321 4322 4400\n"), vec![4321, 4322, 4400]);
    }

    /// The state behind glass#338: the app is up, with a window of a *different* package in
    /// front of it.
    #[test]
    fn a_window_hidden_behind_another_package_is_reported_as_hidden_and_names_it() {
        let dump = concat!(
            "  Window #0 Window{aaa111 u0 com.google.android.settings.intelligence/.SearchActivity}:\n",
            "    package=com.google.android.settings.intelligence appop=NONE\n",
            "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
            "  Window #1 Window{bbb222 u0 com.android.settings/.Settings}:\n",
            "    package=com.android.settings appop=NONE\n",
            "    mFrame=[0,0][1080,2400] isOnScreen=false\n",
        );
        assert!(parse_app_windows(dump, "com.android.settings").is_empty());

        let why = describe_missing_window(dump, "com.android.settings");
        assert!(why.contains("not on screen"), "{why}");
        assert!(
            why.contains("com.google.android.settings.intelligence"),
            "{why}"
        );
    }

    #[test]
    fn an_app_with_no_window_at_all_is_told_apart_from_one_that_is_covered() {
        let dump = concat!(
            "  Window #0 Window{aaa111 u0 NexusLauncherActivity}:\n",
            "    package=com.google.android.apps.nexuslauncher appop=NONE\n",
            "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
        );
        let why = describe_missing_window(dump, "com.android.settings");
        assert!(why.contains("no window"), "{why}");
        assert!(
            why.contains("com.google.android.apps.nexuslauncher"),
            "{why}"
        );
    }

    #[test]
    fn an_app_still_on_its_splash_screen_says_so_rather_than_reporting_no_window() {
        let dump = concat!(
            "  Window #0 Window{ccc333 u0 Splash Screen com.example.app}:\n",
            "    package=com.example.app appop=NONE\n",
            "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
        );
        assert!(parse_app_windows(dump, "com.example.app").is_empty());
        let why = describe_missing_window(dump, "com.example.app");
        assert!(why.contains("splash"), "{why}");
    }

    /// The fourth rejection reason, so no device state maps to a message naming the wrong one.
    #[test]
    fn an_on_screen_window_whose_frame_did_not_parse_says_that() {
        let dump = concat!(
            "  Window #0 Window{ddd444 u0 com.example.app/.MainActivity}:\n",
            "    package=com.example.app appop=NONE\n",
            "    isOnScreen=true\n",
        );
        assert!(parse_app_windows(dump, "com.example.app").is_empty());
        let why = describe_missing_window(dump, "com.example.app");
        assert!(why.contains("frame"), "{why}");
    }

    const WINDOWS: &str = concat!(
        "  Window #0 Window{aaa111 u0 StatusBar}:\n",
        "    mOwnerUid=10168 showForAllUsers=true package=com.android.systemui appop=NONE\n",
        "    mFrame=[0,0][1080,80] isOnScreen=true\n",
        "  Window #1 Window{bbb222 u0 com.example.app/com.example.app.MyDialog}:\n",
        "    mOwnerUid=1000 showForAllUsers=false package=com.example.app appop=NONE\n",
        "    mActivityRecord=ActivityRecord{x u0 com.example.app/.MyDialog t1}\n",
        "    mFrame=[140,800][940,1600] mLastFrame=[140,800][940,1600] isOnScreen=true\n",
        "  Window #2 Window{ccc333 u0 InputMethod}:\n",
        "    mOwnerUid=10145 showForAllUsers=false package=com.google.android.inputmethod.latin appop=NONE\n",
        "    mFrame=[0,1700][1080,2400] isOnScreen=false\n",
        "  Window #3 Window{ddd444 u0 com.example.app/com.example.app.MainActivity}:\n",
        "    mOwnerUid=1000 showForAllUsers=false package=com.example.app appop=NONE\n",
        "    mActivityRecord=ActivityRecord{y u0 com.example.app/.MainActivity t1}\n",
        "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
        "  Window #4 Window{eee555 u0 com.other.app/com.other.app.Foo}:\n",
        "    mOwnerUid=1001 showForAllUsers=false package=com.other.app appop=NONE\n",
        "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
        "  Window #5 Window{fff666 u0 com.example.app/com.example.app.Background}:\n",
        "    mOwnerUid=1000 showForAllUsers=false package=com.example.app appop=NONE\n",
        "    mFrame=[0,0][1080,2400] isOnScreen=false\n",
    );

    #[test]
    fn app_windows_keeps_only_on_screen_package_owned_topmost_first() {
        let ws = parse_app_windows(WINDOWS, "com.example.app");
        assert_eq!(ws.len(), 2, "dialog + activity only");
        assert_eq!(ws[0].id, 0xbbb222);
        assert!(ws[0].title.contains("MyDialog"));
        assert_eq!(
            ws[0].frame,
            glass_core::WindowGeometry {
                x: 140,
                y: 800,
                width: 800,
                height: 800
            }
        );
        assert_eq!(ws[1].id, 0xddd444);
        assert_eq!(
            ws[1].frame,
            glass_core::WindowGeometry {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400
            }
        );
    }

    #[test]
    fn app_windows_empty_when_package_absent() {
        assert!(parse_app_windows(WINDOWS, "com.nope").is_empty());
    }

    #[test]
    fn app_windows_excludes_splash_screen() {
        let dump = concat!(
            "  Window #0 Window{ccc333 u0 Splash Screen com.example.app}:\n",
            "    mOwnerUid=10168 package=com.example.app appop=NONE\n",
            "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
            "  Window #1 Window{ddd444 u0 com.example.app/com.example.app.MainActivity}:\n",
            "    mOwnerUid=1000 package=com.example.app appop=NONE\n",
            "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
        );
        let ws = parse_app_windows(dump, "com.example.app");
        assert_eq!(ws.len(), 1, "splash excluded");
        assert_eq!(ws[0].id, 0xddd444);
    }

    #[test]
    fn app_windows_ignores_trailing_focus_summary() {
        // After the real windows, dumpsys prints a `#0 Window{...}` focus summary carrying the
        // foreground app's `package=` — it must not leak into the last (non-app) window block.
        let dump = concat!(
            "  Window #0 Window{aaa111 u0 com.example.app/com.example.app.MainActivity}:\n",
            "    mOwnerUid=1000 package=com.example.app appop=NONE\n",
            "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
            "  Window #1 Window{bbb222 u0 com.android.systemui.wallpapers.ImageWallpaper}:\n",
            "    mOwnerUid=10168 package=com.android.systemui appop=NONE\n",
            "    mFrame=[0,0][1080,2400] isOnScreen=true\n",
            "  #0 Window{aaa111 u0 com.example.app/com.example.app.MainActivity}:\n",
            "    mOwnerUid=1000 package=com.example.app appop=NONE\n",
            "    isOnScreen=true\n",
        );
        let ws = parse_app_windows(dump, "com.example.app");
        assert_eq!(
            ws.len(),
            1,
            "wallpaper must not absorb the trailing settings package"
        );
        assert_eq!(ws[0].id, 0xaaa111);
    }

    #[test]
    fn app_windows_frame_and_onscreen_on_separate_lines() {
        let dump = concat!(
            "  Window #0 Window{abc123 u0 com.example.app/com.example.app.MainActivity}:\n",
            "    mDisplayId=0 rootTaskId=1 mSession=Session{x}\n",
            "    mOwnerUid=1000 showForAllUsers=false package=com.example.app appop=NONE\n",
            "    mActivityRecord=ActivityRecord{y u0 com.example.app/.MainActivity t1}\n",
            "    mViewVisibility=0x0 mHaveFrame=true\n",
            "    mFrame=[0,63][1080,2220] mLastFrame=[0,63][1080,2220]\n",
            "    Frames: containing=[0,0][1080,2400] parent frame=[0,0][1080,2400]\n",
            "    mForceSeamlesslyRotate=false seamlesslyRotate: pending=null    isOnScreen=true\n",
        );
        let ws = parse_app_windows(dump, "com.example.app");
        assert_eq!(ws.len(), 1);
        // the window's own mFrame, NOT the containing/parent frame on the Frames: line
        assert_eq!(
            ws[0].frame,
            glass_core::WindowGeometry {
                x: 0,
                y: 63,
                width: 1080,
                height: 2157
            }
        );
    }

    #[test]
    fn app_windows_reads_frame_from_modern_frames_line() {
        // Real android-34 format: no `mFrame=`; the window frame is the `frame=` field of the
        // `Frames:` line (alongside parent=/display=/last=, which use their own keys).
        let dump = concat!(
            "  Window #0 Window{f72bd69 u0 com.example.app/com.example.app.MainActivity}:\n",
            "    mViewVisibility=0x0 mHaveFrame=true mObscured=false\n",
            "    mOwnerUid=1000 showForAllUsers=false package=com.example.app appop=NONE\n",
            "    Frames: parent=[0,0][1080,2400] display=[0,0][1080,2400] frame=[0,63][1080,2337] last=[0,63][1080,2337]\n",
            "    mForceSeamlesslyRotate=false    isOnScreen=true\n",
        );
        let ws = parse_app_windows(dump, "com.example.app");
        assert_eq!(ws.len(), 1);
        assert_eq!(
            ws[0].frame,
            glass_core::WindowGeometry {
                x: 0,
                y: 63,
                width: 1080,
                height: 2274
            }
        );
    }
}
