# Changelog

All notable changes to glass are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and glass adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!--
Maintenance: add entries under [Unreleased] as user-facing changes merge to
master, into the ### heading that already exists for their kind — one Added,
one Changed, one Fixed per version, in that order. A branch that appends its
own heading reads fine on its own and leaves the section with three of each
once several have merged. At release time, rename [Unreleased] to the new version with its UTC
release date (the GitHub release's `published_at` date, so the changelog matches
the site's release list), add a fresh empty [Unreleased] above it, and update the compare links at the
bottom. Keep entries user-facing — what changed for someone using glass — not
internal refactors, CI, or test-only changes.
-->

## [Unreleased]

### Added
- The advertised MCP schema now documents what it did not. Every tool parameter carries a
  description, and `glass_type`, `glass_key`, `glass_drag`, `glass_stop`, `glass_baseline_save` and
  `glass_logs` describe what they do to the app rather than which sibling tool to prefer instead.
  Between them they state the coordinate space (window-relative everywhere except `glass_window`'s
  `move`, which positions the window on screen), the defaults for click count, drag/settle timing
  and log paging, and the behaviour a caller cannot see from the type and would otherwise learn by
  trial: `glass_type` does not focus a field and types into whatever already has focus, and a
  newline in its text does not press Return; `glass_drag` refuses a path with any endpoint outside
  the window; `glass_stop` discards the captured logs and element ids; `glass_baseline_save`
  replaces an existing name silently; `glass_logs` returns whatever has accumulated without waiting
  (`glass_wait_for_log` is the blocking one); and `glass_start`'s `timeout_ms` bounds waiting for
  the window, not the `build` step.
- Accessibility elements can now carry a second label. `glass_a11y_snapshot` renders it as
  `desc="…"` after the name — an icon-only button that used to reach you as a role and a
  rectangle now says what it is — and the `glass_a11y_marks` legend labels an unnamed element
  from it, spelled `desc="…"` there too. `glass_wait_for_element` and `glass_scroll_to_element`
  report it on the element they matched. It is display-only: both tools still select on `name`.
  All five backends now source the field: the Linux, Windows and macOS readers read AT-SPI
  `Description`, UI Automation `HelpText`, and AX `AXHelp` respectively; the two Android readers
  read whichever of an element's text and content-description did not become the name or the
  value; and the iOS reader reads the element's accessibility hint, falling back to the label an
  editable element's identifier displaced. On Android a description needs one node to carry two
  distinct labels and most controls carry only one, so expect `desc` to be absent on most Android
  nodes.
- `glass_start` on the iOS Simulator passes an app's launch arguments through: everything after
  the `.app` path or bundle id in `run` reaches the app as its own arguments, joined
  (`--tab=value`) and separated (`--tab value`) forms alike, so an app whose behaviour is
  selected by a flag can be driven.
- `glass_type` accepts an optional `return` observe (`"settle"` or `"snapshot"`), matching
  `glass_click_element` and `glass_set_value` — type text and confirm the UI settled (or fold a
  fresh accessibility tree) in one call. Inside a `glass_do` `type` action the field is rejected
  with guidance to use a `settle` action or the terminal `then` observe instead.
- [docs/reference/a11y-roles.md](docs/reference/a11y-roles.md) documents which accessibility roles
  each platform backend can produce, and why a role is unavailable where it is. Two example apps
  hold the controls its cells are decided from — one screen of stock `android.widget` controls in
  [`examples/android-role-fixture/`](examples/android-role-fixture/) (builds without Gradle) and the
  UIKit and SwiftUI equivalents in [`examples/ios-role-fixture/`](examples/ios-role-fixture/) — so
  anyone can read the same trees back.
- `glass_click_element` now actuates through Android's accessibility action when the optional
  on-device accessibility companion is installed, instead of always synthesizing a tap. It reaches
  controls a tap cannot — an element scrolled far below the fold actuates in place — and the
  result's `method` field reports `native-action` for those clicks. A control whose label is a
  separate element from the control itself, as in Jetpack Compose, resolves to the enclosing
  control that handles the tap. Clicking a disabled control is now an error rather than a tap that
  silently does nothing, and a checkbox or switch is only reported clicked once its state is
  observed to change (a radio button or tab already selected counts as clicked, since re-selecting
  it is a no-op). The `uiautomator` reader and iOS still use the pointer path and say so.
- `glass_click_element` now discloses which element it actuated. When the native action fires on a
  different element than the one you named — a control whose label is a separate element from the
  control itself — the result and the audit record carry `actuated_id`, so "clicked the Save label"
  and "clicked the card around it, which navigated away" are no longer indistinguishable
  afterwards.
- More elements report a real role instead of `Other`: on Windows, documents; on macOS, outlines
  and their rows, split views and their dividers, scroll areas, headings, and menu buttons; on
  Android, the AndroidX card container, the AppCompat linear layout, the view that hosts a Compose
  hierarchy and the `ViewPager` swipe-paged container; on iOS, content groups and headings.
  Windows also distinguishes a button that can be toggled — a formatting bar's Bold or Italic —
  as `ToggleButton`. `docs/reference/a11y-roles.md` lists what each platform can produce.
- `glass_doctor` now returns structured data alongside its rendered `report` text: `sections`
  (each check as `{name, status, detail, remedy?, remedy_action?}`, grouped under the section that
  diagnoses it) and `overall`, the single verdict — `"ok"`/`"warn"`/`"fail"` — to branch on instead
  of parsing prose. Purely additive: `report` is unchanged.

### Changed
- On Linux and Windows, `glass_wait_for_element` no longer re-reads the whole accessibility tree on
  a timer. Where the platform can say whether anything changed, a wait for something that has not
  happened yet now reads the tree when something changes instead of once per interval — measured at
  `interval_ms: 100`, 4 reads for a 3-second wait against the GTK test fixture where it previously
  took 22, and 4 on Windows hardware where it previously took 24. It re-reads once a second
  regardless, and once more before reporting nothing found, so a change the platform does not
  announce costs latency, not a wrong answer. That matters most on Windows for
  `condition: "enabled"`: of the two providers measured, a WinForms app never announced a control
  becoming enabled and a WPF one did. Every other backend polls exactly as before, and so do these
  two if the subscription cannot be established or stops delivering.
- Both Android readers now name an element the same way. The same control used to answer
  differently depending on which reader was running — `uiautomator` and the on-device
  accessibility-service reader disagreed on which label became the `name` — so a `name:`
  selector learned under one could silently miss under the other, with nothing to say why. A
  control with both a visible label and a content description is now named by the visible
  label, the one actually on screen: verified on device, the role fixture's button reads
  `Button "Save" desc="Save changes"` through both readers, where the two strings used to be
  swapped between them. An editable element is named by its content
  description and never by what has been typed into it; the accessibility-service reader used
  to name a filled text field by its own contents, so its name changed with the field's contents
  from one snapshot read to the next.

  An editable element with no content description — including an empty field named only by its
  hint — now falls back to the leaf of its view resource id rather than staying unnamed: plain
  text, not the package-qualified form Android reports, and shared by every other view built from
  the same layout, so treat it as a label of last resort, not a selector to rely on.
  Verified on device: Settings' search box reads `open_search_view_edit_text` identically from
  both readers. The on-device companion separately carries the field's hint into its
  `description`, so an agent sees `desc="Search settings"` there even when nothing names the field
  at all — verified on the role fixture's added `EditText`, which has a hint and neither a content
  description nor a resource id: it renders as `TextField desc="Search settings"` through the
  companion. `uiautomator` cannot supply that description at all — its dump carries no hint
  attribute — so a text field's `desc` is richer through the companion than through `uiautomator`.
  Both the id fallback and the hint on the accessibility-service reader need the updated on-device
  companion.

  One selector detail changes with it, though nothing becomes unreachable. On the
  accessibility-service reader an element that is not editable — a label, a button, a check box —
  no longer reports a `value`; that reader used to copy the element's own text there as well as
  into `name`. A `value_contains` filter aimed at such an element therefore stops matching, and
  `glass_wait_for_element` waits out its timeout. Match on `name` instead: it is a substring
  filter too, and that text was always in `name` as well, so every selector has an equivalent —
  including `role:"Button"` with `name:"Submit"`, which reaches a Jetpack Compose button surfacing
  as a clickable `Group` exactly as the `value_contains` form did. `uiautomator` never reported a
  value there, so a selector written against that reader is unaffected.


- `glass_start` on Android now fails on a `run` element it cannot use, instead of ignoring it.
  Android launches an activity rather than a command line — `am start` takes intent extras, not
  program arguments — so anything beyond the `package/.Activity` component and an optional `.apk`
  was being dropped, and the launch reported success for an app configured differently from what
  was asked. The error names the element it could not use, so the correction is to drop it. A call
  that used to succeed can now fail, which is a change rather than an addition: it ships in a
  minor because the caller is an agent that can read the error and retry within the same session.
- Every cell of [docs/reference/a11y-roles.md](docs/reference/a11y-roles.md) that is not `yes` now
  names the native token behind it as its own clause — `n/a (reports AXStaticText instead)`,
  `gap (AXPopUpButton arrives unmapped)` — instead of leaving it buried in prose. That is the fact
  an agent holding an `Other(...)` in a snapshot is looking for, and it is data rather than text:
  each backend's tests resolve it through that backend's own map, and on Windows, where UIA names
  every documented control type, an invented one fails the build.
- [docs/reference/a11y-roles.md](docs/reference/a11y-roles.md) now splits a role glass could still
  reach from one only a platform change could, and decides which by putting the control on screen
  and reading the tree back rather than by what a platform's API reference implies. Fourteen cells
  marked `gap` turned out to be unreachable and now say so with what the control actually reports:
  an iOS stepper arrives as two buttons, a progress view as a generic element, an alert and an
  action sheet as loose buttons, a table view and its rows as a group of static text; an Android
  toolbar arrives as a plain `ViewGroup` and a popup menu as a `ListView`. The cells that stay
  `gap` now name what is there and unread — Android's `TabWidget`, `TableLayout` and
  `NumberPicker`, iOS's `AXPopUpButton` for a menu-style picker, and the `CollectionItemInfo` and
  `isHeading` fields neither Android reader carries. Each column also records when it was last
  read. No role mapping changed; this is what the page claims, not what glass produces.
- Three kinds of element now report a different role than before, so a `role:` filter that used to
  match them no longer does. On Windows, a button that can be toggled (a formatting bar's Bold or
  Italic) reports `ToggleButton` instead of `Button`; on macOS, a row inside an outline view
  reports `TreeItem` instead of `ListItem`; on Android, the root element of a tree read through the
  on-device accessibility service reports `Window` instead of the role its widget class implied,
  matching what the `uiautomator` reader has always reported. `role:` matches exactly — the
  fallback that also accepts a generically-classified element only rescues ones reported as `Group`
  or `Other` — so a `glass_wait_for_element` (or any other element selector) asking for
  `role: "button"` or `role: "listitem"` on those elements needs the new role.
- The `glass_a11y_snapshot` outline now names the platform's own role token for an element glass
  has no role mapping for: the line reads `Other(AXDisclosureTriangle)` rather than a bare
  `Other`, so a custom control is still identifiable. The token is the platform's stable
  identifier — the AT-SPI role, the UIA control-type name, the AX role string, the Android widget
  class, the iOS role string — and reads the same on every machine.
- `glass_a11y_snapshot` now returns a compacted outline: chains of unnamed single-child
  container elements are collapsed. Element ids are unchanged and every element remains
  addressable with `glass_click_element` / `glass_set_value`.
- Linux accessibility snapshots are faster on large trees: the AT-SPI reader now issues each
  element's independent property reads concurrently instead of one at a time.
- `glass_a11y_snapshot` accepts an optional `max_nodes`: raise the element cap for a large app,
  or pass `0` to remove the element-count limit. The default cap is unchanged, and when a snapshot
  is truncated the notice now reports the actual limit and says how to widen it.
- `glass_click_element` now actuates via the platform's native accessibility action
  when the element exposes one (AT-SPI Action on Linux, UIA patterns on Windows,
  AXPress on macOS), falling back to the synthetic pointer click when the element —
  or the backend — exposes none. A native action that was dispatched but failed
  reports the error rather than falling back, so a click never actuates twice. The
  result's new `method` field reports which path ran (`native-action`/`pointer`),
  with `native_fallback` explaining any fallback. Native actuation works for
  occluded or scrolled-off-screen elements and, on macOS, no longer moves the
  cursor. On those three backends the click also re-checks the element against the
  live tree, so one that no longer matches the snapshot errors (`element changed;
  re-snapshot`) instead of clicking stale coordinates — the pointer-only iOS and
  Android paths have no such live check.
- `cargo build --workspace` and `cargo clippy --workspace` now succeed on macOS. The Linux-only
  backend crates (X11, Wayland, and their AT-SPI/D-Bus/process/sandbox helpers) gate themselves
  and every one of their dependencies to Linux, instead of failing to build elsewhere — they
  compile to nothing on macOS. CI's macOS job runs those two commands workspace-wide now, instead
  of a hand-maintained list of Mac-relevant crates, so it gains a workspace-wide build and lint
  gate; test coverage there is unchanged, still targeted `cargo test -p` runs and
  `./scripts/test-macos.sh` rather than `cargo test --workspace`, since several macOS integration
  tests need permissions CI doesn't grant.

### Fixed
- On macOS, `glass_start` now reports a window's settled geometry rather than a frame of its
  opening animation. It used to return whatever geometry it read the instant it found the newly
  launched window; measured across cold launches, that geometry disagreed with the window's
  settled size in 11 of 12 runs. It now waits for two consecutive readings to agree before
  returning. X11 and Wayland were measured against a fixed-size test fixture with no opening
  animation and didn't race this way there, so the fix stays macOS-only; Windows, Android and iOS
  were not measured.
- A `glass_wait_for_element` shorter than a second could report an element absent that was on
  screen. Where the platform announces changes, the wait skips reads it has been told are pointless
  and reads anyway once a second in case it was told wrong — but that second was previously counted
  in polling intervals rather than measured, so at the 200ms default it landed at two seconds, past
  the end of many waits. Such a wait answered from the single read it took before the change it was
  waiting for. It now reads once more before reporting nothing found, whatever its length, and the
  once-a-second floor no longer moves with `interval_ms`. Affects every backend that can subscribe
  to change notifications.
- Android's `set_value` now refuses a write whose target has drifted in value, not just in role,
  name or bounds. A re-walk that lands on a same-role, same-name, same-rect element holding
  *different* data — a recycled list row reusing the same view is the case this closes — is now
  rejected as changed since the snapshot rather than written to. An editable element with no
  content description and no resource id has no name at all, so role plus an 8px bounds match used
  to be the whole fingerprint a write re-walked against. The value only discriminates where there
  was one to capture: rows of *empty* fields share a role, a name, a rect and no value alike, so a
  write can still land on the wrong one of those — re-snapshot before writing into a list that has
  scrolled.

- The tool reference said a negative input coordinate addressed a point off the window's
  top-left edge. It is rejected, like any other point outside the window, before the backend
  sees it; the reference now says so and a test covers the negative case.
- A switch now reports the same role on Windows, macOS, Android and iOS. `glass_a11y_snapshot`
  called one a `CheckBox` on iOS, and a `Button` or a `CheckBox` on macOS depending on which toolkit
  drew it, so a `role:"ToggleButton"` selector that worked on one machine silently matched nothing on
  another. Both now read the platform's switch marker and report `ToggleButton`. A macOS switch drawn
  with AppKit also gains the checked state it never reported, so `condition:"checked"` works on it.

  Two things to know. A `role:"CheckBox"` selector that used to match a switch on iOS or macOS no
  longer does — match `ToggleButton`, or match by name. And Linux is unchanged and still differs: a
  GTK4 switch is published over AT-SPI as a check box, indistinguishable from a real one, so it
  arrives as `CheckBox` there.
- `glass doctor`'s macOS accessibility line now reports a real reading instead of assuming one. It
  used to say the reader was available whatever the accessibility API was doing, so the one case you
  need it for — the reader is not answering — showed green. It now reads one attribute off the
  system-wide accessibility element and reports what happened, with the error code. macOS gives the
  same code for several causes, so the line names the one that applies: not trusted, nobody logged in
  at the console, assistive access switched off, or a binary that was never granted despite the
  system reporting it as trusted — each with its own remedy. A logged-out console is a warning rather
  than a failure, since it is not a broken install.
- `glass doctor`'s iOS device line now reports which simulator glass would drive, by running the same
  resolution `glass_start` runs. It listed how many were available and nothing more. Nothing booted
  is fine and says so — glass boots one at start — and an iPad-only host is no longer reported as
  having no device, since glass drives any iOS simulator. What is now reported as a failure is what
  the start path will not fix for you: a `GLASS_IOS_UDID` that names a simulator which is not booted,
  or is not on this host, or is not an iOS simulator at all (glass attaches to a pinned device
  without booting or checking it, so every call would fail against it), and a `GLASS_IOS_DEVICE` that
  matches nothing here — each with the remedy that fits. A device listing that cannot be read says
  that, rather than reporting it as nothing booted.
- `glass_set_value` now tells you when a write did not take on Android (without the on-device
  accessibility service) and on the iOS Simulator. Those two backends tap the element, clear it and
  type — and used to report success without ever looking again, so a tap that landed slightly off, a
  field that rejected the input, or a dropped keystroke all came back as `ok`, and everything you
  asserted afterwards was against a screen that never changed. They now read the element back and
  require it to hold exactly what you asked for. A field that reformats what it is given (a phone
  number becoming `(123) 456-7890`) is reported as not applied even though the text arrived: read the
  element to see what it holds. Clearing a field is judged its own way — it has to read back empty —
  so on a platform that reports an emptied field's placeholder as its text (Android does), a clear
  that worked is also reported as not applied. Windows and macOS already read the value back, and
  Android's on-device service reader already did too; the Linux reader still trusts the toolkit's own
  answer.
- An Android or iOS Simulator command that stops answering no longer hangs the tool it was
  serving. Every one-shot call glass makes to `adb`, `emulator`, `xcrun simctl`, `plutil` or `ps`
  now has a deadline sized for what that call does — a full accessibility dump gets longer than a
  tap, an app install longer still, and waiting out a simulator boot longest of all — and a call
  that exceeds it comes back as an error naming the operation, how long it waited, anything the
  tool managed to say first, and what to try (for `adb`, `adb kill-server`). Log streaming is
  unaffected: it is meant to run until you stop it.
- A command that exits while something it started still holds its output pipe no longer returns
  the partial output as though it were complete; it reports that the output may be truncated. A
  short read of an accessibility or window dump used to parse as a smaller screen.

- macOS: the release `.zip` and `.dmg` now carry a `README.md`, as every other platform's artifact
  already did. It covers the drag-to-`/Applications` install, the two permission grants (including
  that granting Screen Recording relaunches the app), and the `http://127.0.0.1:7300/` endpoint —
  the platform with the most first-run setup was the one shipping none of it. The file sits beside
  `GlassMcp.app` rather than inside it, so the bundle's signature and notarization are untouched.
- iOS: `glass_start` now reads the display scale from the Simulator rather than from the app's
  accessibility tree, so a launch no longer depends on the app having rendered. The scale is a
  property of the device (`idb describe` reports it before any app runs), but glass derived it by
  dividing the capture's pixel width by the accessibility root's point width — a value that is
  absent for around two seconds after launch, against a retry budget of about half a second. A
  launch that lost that race failed with `could not determine the iOS display scale from the
  accessibility tree` and left no session, and the retry loop it needed is gone with the
  dependency.
- Android: `glass_a11y_snapshot` no longer fails on the first call against a device that has just
  booted, and a dump that does fail now says why. A device reaches `sys.boot_completed` — all
  `glass_start` waits for — several seconds before `uiautomator` can produce a dump, so the first
  snapshot now waits for it (up to 30s, once per session; later snapshots do not wait). When a dump
  genuinely fails, the error names the dump and quotes its own reason —
  `uiautomator dump did not write /sdcard/glass_dump.xml: ERROR: null root node returned by
  UiTestAutomationBridge.` — where it used to report the unrelated read of the file the dump had
  never written (`cat: /sdcard/glass_dump.xml: No such file or directory`). `glass_doctor --deep`
  keeps its single attempt, so it still reports whether the device can dump right now.
- Linux (`backend: "wayland"`): glass now recovers a window an X11 app opened that the compositor
  never surfaced. Under load, a window the app maps can reach the compositor's Xwayland server and
  never reach the compositor itself — sway has no view for it, so `glass_list_windows` leaves it
  out, and when it is the app's only window `glass_start` waits for a window that never arrives
  and times out on a healthy app. The window stays lost however long you wait, and redrawing does
  not bring it back (measured on glass's own two-window X11 fixture: 6 of 15 launches on a loaded
  machine). glass now cross-checks the X server's mapped toplevels against the compositor's window
  list and re-maps the ones the compositor never saw, once per window and only after it has been
  missing on two checks in a row, saying on stderr when it did. A launch that still finds no
  window now says the recovery ran and did not take, instead of reporting a bare timeout. An app
  with no X11 side (a native Wayland app) has nothing to recover and nothing is ever re-mapped
  for it.
- Linux (`backend: "x11"`): `glass_stop` no longer signals an app that should have been asked to
  close. The close request went out on a short-lived X connection that was closed immediately
  afterwards, and a request the server had not processed yet was discarded with it — so the app
  never saw it, sat out the whole close grace, and was signalled, losing whatever it would have
  flushed on exit. On a loaded machine 4 of 10 teardowns went that way, with the app's event loop
  running the whole time. glass now waits for the server to confirm the request before the
  connection can go away.
- macOS: launching a stock Apple app no longer leaves a crash report and a "quit unexpectedly"
  dialog behind. glass direct-spawns a bundle's inner executable to get piped logs and
  containment, but macOS gives a system app a launch constraint requiring it to be started by
  LaunchServices, so the kernel killed the spawned process — measured, 10 of 12 stock apps tried.
  The launch then succeeded anyway through the LaunchServices fallback, which is why this went
  unnoticed. glass now recognizes Apple platform code and hands those launches off without
  attempting the spawn. Your own app is unaffected: only Apple-signed system code takes the new
  path, and only when `sandbox` is `off` (a contained launch cannot be handed off at all, so it
  still tries — Terminal and Disk Utility, for instance, do run contained).
- An X11 display that stops answering no longer hangs `glass_stop`. Sending the close request is
  a series of blocking X round trips, so it now runs under a deadline; past it glass reports the
  display as unresponsive and goes straight to signalling the app, instead of holding teardown
  open until the process is killed.
- A Wayland session whose compositor stops responding no longer hangs glass. Every sway IPC
  request is bounded, and a connection that has timed out is not reused — a late reply arriving
  on it could otherwise be read as the *next* request's, reporting a close request as delivered
  when it never was. `glass_stop` reports the compositor as unreachable and falls back to
  signalling the app.
- `glass_stop` now asks the app to close before killing it on every desktop backend, so the app
  runs its own shutdown path. All four previously went straight to terminating the process —
  Windows by closing the job object, macOS by signalling, X11 and Wayland by signalling the
  process group — which an app that records whether it exited cleanly reports as a crash the
  *next* time it starts. A Chromium-based browser opened with a "Restore pages?" prompt instead
  of its normal first screen, so an agent driving it saw a different first screen on every run.
  The ask is each platform's own: `WM_CLOSE` on Windows, a quit request on macOS, a
  `WM_DELETE_WINDOW` client message on X11, and a close request through the compositor on
  Wayland. On the two Linux backends that was measured directly: a GTK app ran its shutdown
  handlers when asked and none of them when signalled, because the toolkit installs no `SIGTERM`
  handler. An app that installs one of its own is the exception.
  An app that ignores or refuses the request is still terminated, and glass now says so on
  stderr rather than reporting an unqualified success. Two exceptions keep the old teardown: a
  Windows launch under Sandboxie containment (a close request sent from outside the box is
  accepted by the OS but never reaches the app, so landing one needs a helper running inside the
  box), and an X11 client that never opted into the `WM_DELETE_WINDOW` protocol, which has no
  handler for the request and will not act on it. Under Wayland the compositor performs the ask,
  and it disconnects a client that has no close protocol rather than asking it — glass cannot
  tell that apart from a clean close there, so that case is reported as one.
- iOS: `glass_start` no longer reports a window for an app that died on launch. `simctl launch`
  exits successfully once the process is spawned, so an app that rejects a launch argument or
  trips an assertion at startup was reported as running, and the screenshot behind that geometry
  was the Simulator home screen — the next snapshot described SpringBoard with nothing saying why.
  The launch now fails, naming the bundle id, the pid, and what the app itself wrote to stderr on
  the way out — a Swift `fatalError` message, say. That output is captured for the first time
  here: the device's unified log carries the whole simulator, not the app.
- macOS: an app that hands off to LaunchServices — a bundle whose executable re-execs itself, so
  glass adopts the resulting process rather than the one it spawned — now receives the launch
  arguments from `run`. They were dropped on that path while the directly-spawned path passed
  them, so the same `run` launched differently configured apps depending on which arm ran.
- A contained app launched on the Wayland backend can now reach an Xwayland display through the
  ordinary X11 socket (`/tmp/.X11-unix`), which the sandbox's ephemeral `/tmp` previously hid.
  X11 clients that fall back to the abstract socket were unaffected; ones that don't failed to
  connect and the launch timed out. The X11 backend already exposed this socket.
- A transiently stalled Xvfb no longer fails `glass_start` on Linux/X11: if the private X
  server doesn't report its display within 10s, glass kills it and retries once with a fresh
  server (worst-case start latency in the still-failing case is ~24s). When startup still
  fails, the error now includes the head of Xvfb's stderr output and names the recovery
  (attach to an existing display with `GLASS_DISPLAY=:N`, or run Xvfb manually to diagnose).
  Previously the first stall failed the session outright, with Xvfb's stderr discarded.
  `glass doctor --deep`'s Xvfb probe budget now covers the retry, so it no longer reports a
  failure for a stall the real start path survives.
- The accessibility tree now reports when a snapshot was truncated. Previously a tree that hit
  an internal size limit was returned as though it were complete, so a missing element was
  indistinguishable from one that does not exist. All backends now share the same limits and
  disclose when one is reached.
- macOS: when the accessibility tree can't resolve the window glass adopted, the diagnostic
  it prints now names each candidate's `AXRole` and the raw `AXError` behind a failed read, and
  window adoption itself now records which on-screen window it took out of what else was
  available. `WindowNotFound` no longer asserts a timing cause it cannot know.

## [1.1.0] - 2026-07-23

### Added
- **Ignore regions for visual comparison.** `glass_diff`, `glass_wait_for_region`,
  `glass_wait_stable`, and `glass_do`'s `settle` action accept `ignore` — window-relative
  rectangles excluded from the comparison — so perpetually animating content (a blinking text
  caret, a clock, a spinner) no longer keeps `changed_pct` permanently non-zero or prevents a
  settle from ever completing. `glass_diff`, `glass_wait_stable`, and `glass_wait_for_region` each
  report the excluded count as `ignored_pixels` — so a mask that covers the whole compared area is
  visible rather than hidden behind a hollow `settled`/`matched` — and `changed_pct` is measured
  over the pixels that remain.
- How-to: measure the verification-loop cost (semantic vs screenshot) —
  [docs/how-to/verification-cost.md](docs/how-to/verification-cost.md).
- Reference: host compatibility — which MCP hosts are verified against glass, and what glass needs
  from any host — [docs/reference/host-compatibility.md](docs/reference/host-compatibility.md).
- How-to: drive a native iOS app in the Simulator — build the bundled
  [`examples/ios-greeter/`](examples/ios-greeter/) demo app and drive it end to end (launch, read the
  accessibility tree, act, verify from text and by diff) —
  [docs/how-to/drive-an-ios-app.md](docs/how-to/drive-an-ios-app.md).

### Changed
- **Accessibility is on by default at launch.** `glass_start` now enables the accessibility tools
  without an explicit `a11y: true` — the semantic path (address elements by `#id`, verify from text,
  no image tokens) works out of the box, matching how glass is meant to be driven. Pass `a11y: false`
  to skip spawning the private accessibility bus for canvas/pixel-only apps. On a Linux host that
  can't start an accessibility bus (e.g. `at-spi2-core` isn't installed), the default quietly falls
  back to pixel-only rather than failing the launch; an explicit `a11y: true` still fails loudly.
  Linux only in effect; other backends already read accessibility ambiently.
- **Server instructions lead with the low-token accessibility path.** The guidance an MCP host
  shows the agent now presents semantic addressing (`glass_a11y_snapshot` → `glass_click_element` /
  `glass_set_value`, text-only, no image tokens) as the default way to see and drive the UI, with
  screenshots and pixel coordinates as the fallback for canvas/black-box apps — so an agent reaches
  for the cheap path first. No tool behavior changed.
- **Error messages point to the recovery action.** Three common failures now name what to do next
  instead of dead-ending: "no active session" tells you to `glass_start`; the accessibility-
  unsupported error points to the pixel loop (`glass_screenshot` + `glass_click`); and an element
  with no clickable geometry suggests `glass_scroll_to_element` to bring it into view (or locating it
  by screenshot). The remaining failure paths already carried a next step.
- **`glass_a11y_snapshot` says what to do when the app exposes no elements.** When a snapshot comes
  back with only the window root (no addressable elements) — common for an app that doesn't publish an
  accessibility tree — the result now appends a hint to drive the app by pixels (`glass_screenshot` +
  `glass_click`), instead of returning a bare root-only outline. Previously only the Linux "no a11y
  bus" path guided the agent; this covers the thin-tree outcome on every backend.

### Fixed
- **Linux `a11y:true` now reaches AccessKit-based apps (egui/winit and other Rust GUI
  toolkits).** The private accessibility bus glass spawns for `a11y:true` advertises a screen
  reader by setting `org.a11y.Status.ScreenReaderEnabled`, the signal AccessKit-based apps gate
  their accessibility-tree publication on. That setting previously failed to take effect (the
  bus's GSettings backend tried to persist it via a D-Bus service glass's isolated bus doesn't
  expose), so such apps stayed invisible to `glass_a11y_snapshot` and friends even though the
  bus itself was reachable — GTK/Qt apps, which don't gate on the advertisement, were unaffected.
- **GTK4/Qt apps render under containment on the headless Linux display instead of black.** When
  glass launches an app in its sandbox on Linux, it now sets software-render env defaults
  (`GSK_RENDERER=cairo`, `QT_X11_NO_MITSHM=1`, `QT_QUICK_BACKEND=software`) so the GPU/shared-memory
  rendering paths the sandbox blocks don't leave the window black. Pass the relevant variable in
  `glass_start`'s `env` to override.
- **`glass_a11y_marks` boxes stay inside the window.** An element whose accessibility extent
  reaches past the window edge (toolkit a11y geometry can over-report by ~10–20px) had its outline
  drawn off-frame, where it was clipped at the capture edge and read as a rendering glitch. Each
  mark box is now clamped to the window bounds, so an overrunning edge lands on the frame edge. The
  `#id` and the `glass_click_element` target (the element center) are unchanged.

## [1.0.1] - 2026-07-21

### Fixed
- **`glass-mcp` reports its real version.** `--version`, `glass-mcp doctor`, and the MCP
  `initialize` handshake previously reported `0.0.0` (the crate is versioned by release tag, not in
  `Cargo.toml`), and the handshake additionally identified the server by the transport library's
  name and version rather than glass's. The version is now derived at build time from the release
  tag, and the handshake identifies the server as `glass-mcp`.

## [1.0.0] - 2026-07-21

*First stable release. glass's agent-facing surface — tool names and parameters, result shapes, enum
values, the untrusted-content marker, and `GLASS_*` variables — is now covered by a stability
commitment (see [stability.md](docs/reference/stability.md)): changes follow SemVer, with
breaking changes signalled in the schema and error text and a deprecate-then-major path.*

### Added
- **`GLASS_SANDBOX_FLOOR`** — an operator-set minimum containment level (`off`/`default`/`strict`)
  that a launch's `sandbox` level can raise but never drop below: an omitted `sandbox` is clamped up
  to the floor, and an explicit request below it is refused. Default `off` (no floor — today's
  behavior). `glass-mcp doctor` reports the configured floor.
- **macOS and iOS accessibility now report toggle state.** A checkbox, radio button, or switch read
  via `glass_a11y_snapshot` / `glass_wait_for_element` now carries its `checkable`/`checked` state on
  the macOS and iOS backends (previously only Linux, Windows, and Android did), so
  `glass_wait_for_element {condition: "checked"}` (or `"unchecked"`) works against those controls.
  State is reported only when it can be read for certain — an indeterminate or unreadable control
  matches neither condition rather than being misreported.
- **Windows: a Slider, Spinner, or ProgressBar's numeric value is now readable** through the
  accessibility tree (via its `RangeValuePattern` position), so `glass_wait_for_element
  {value_contains}` can match a range control's number; previously these controls exposed no value.
  The value is the control's raw numeric position (a `0..1` slider shown as "50%" reads as `0.5`).
- **Android: the on-device accessibility companion now reports toggle state.** A checkbox / switch /
  toggle read via the high-fidelity `AccessibilityService` companion now carries its
  `checkable`/`checked` state (the baseline `uiautomator` reader already did), so
  `glass_wait_for_element {condition: "checked"}` works against Android toggles through the companion
  path too.

### Fixed
- **Linux: apps whose program or files live under your home directory now start under the default
  sandbox.** The default sandbox hides your home directory (and `/tmp`) from the launched app, which
  previously also hid the app's *own* launch target — so an app passed by absolute path under home
  (`python3 /home/you/app.py`), reached through a symlink (a virtualenv or asdf/pyenv shim), found on
  a `PATH` entry under home (a `cargo install`/`pipx`/`npm -g` tool), or given by a relative path
  would fail to start (it exited before its window appeared). glass now makes the launch target
  reachable inside the sandbox — and defaults the working directory to glass's — while still keeping
  your home directory hidden. A contained app that still exits before its window now reports the
  likely cause and how to fix it (`set cwd`, or run with `sandbox:"off"`) instead of a bare exit code.
- **macOS: apps whose program or files live under your home directory now start under the default
  sandbox.** The same fix as Linux, for the macOS (Seatbelt) sandbox: the default sandbox hides
  `/Users` from the launched app, which previously also hid the app's *own* launch target — a script,
  asset, or binary passed by path, reached through a symlink, found on a `PATH` entry under your home,
  or given by a relative path would fail to start. glass now makes the launch target reachable while
  keeping the rest of your home hidden.
- **Linux: accessibility-enabled launches are no longer slow.** On X11 and Wayland, starting an app
  with `a11y: true` (needed for `glass_a11y_snapshot`, `glass_click_element`, and the other
  accessibility tools) previously added a fixed ~25-second delay before the app's window appeared.
  glass now runs its private accessibility bus with no auto-activatable services, eliminating the
  delay — these launches are now as fast as launches without accessibility.
- iOS: `glass_click_element` and `glass_set_value` now toggle a `UISwitch` by swiping its control (a tap
  does not actuate a UISwitch); `glass_set_value` verifies the switch reached the requested state instead
  of returning a premature `ok`. Other backends are unchanged.
- `glass_set_value` on a switch/checkbox now returns an actionable error naming the accepted boolean
  spellings (`true/false`, `on/off`, `1/0`, `yes/no`) when given a non-boolean value, instead of a generic
  "value did not change — use keystrokes" message that misdirected the agent.
- **macOS: a newly-launched app is brought frontmost at `glass_start`**, so `glass_a11y_snapshot`
  resolves its window immediately instead of returning `window not found` until the first `glass_click`
  activated the app. The `window not found` message also now suggests a remedy.
- **Accessibility actions now work after an app resizes its own window** (e.g. macOS Calculator
  opening its sidebar). `glass_a11y_snapshot` re-reads the current window geometry, so
  `glass_click_element` / `glass_set_value` clamp against the window's actual size instead of a stale
  cached one — elements that moved beyond the old bounds are no longer reported unclickable.
- **`return:"snapshot"` now waits for the UI to settle before folding the a11y tree.** A
  screen-changing `glass_click_element` / `glass_set_value` with `return:"snapshot"` returns the
  post-transition tree instead of a mid-transition one (best-effort — a continuously-animating UI still
  returns the freshest tree at the settle deadline).

### Changed
- **Every tool now returns a uniform result envelope.** On success, a tool's leading text block is
  `{"ok": true, "tool": "<name>", "result": { … }}`, with the tool-specific fields under `result`.
  Text the target app controls (the accessibility outline, log lines, clipboard contents, window
  titles, and a matched element or log line) continues to arrive in its own block wrapped in the
  untrusted marker — never inside `result`. Image tools return the image block first, then the
  envelope, then the image note. An agent that read a tool's bare top-level JSON should now read the
  fields under `result`.
- **`glass_start`'s `env` is now a JSON object** `{ "KEY": "VALUE" }` instead of an array of
  `[key, value]` pairs.
- **`glass_logs`'s `max_lines` is a plain integer** (a `u32`).
- The containment docs now spell out that the Windows sandbox isolates the boxed app's writes and
  (under `strict`) its network, but not its reads — it does not hide your home directory from the app
  the way the Linux and macOS profiles do. Behavior is unchanged; this documents an existing limit.

## [0.5.0] - 2026-07-13

### Added
- `glass_capabilities` — a new tool reporting which operations (input, multi-touch, clipboard,
  accessibility, window move/resize) can be performed right now on a backend, and any setup a
  blocked one needs, so an agent can check before acting instead of hitting an Unsupported error.
  Each operation reports a live status — `supported`, `degraded` (works now at reduced fidelity,
  e.g. Android's adb-only input without its on-device agent), `requires_setup`, or `unsupported`
  — plus the specific tools that operation gates (e.g. `accessibility` names
  `glass_a11y_snapshot`, `glass_click_element`, and friends), so a degraded or blocked entry
  points straight at the tool calls it affects. `accessibility` is reported live on every
  backend — the desktop backends read `requires_setup` when their a11y stack isn't ready (the
  Linux AT-SPI runtime isn't installed, the macOS Accessibility permission isn't granted, or
  Windows UI Automation can't initialize) rather than a blanket `supported`. Takes an optional
  `backend` (defaults to the active one); a backend not built into the running binary reports
  `available: false`.
- `glass_scroll_to_element` now drives **horizontal** containers, not just vertical: `direction`
  accepts `left`/`right` as well as `up`/`down`, and when omitted it **infers** the direction from
  the target's off-screen position. It anchors the scroll on the target's own row/column, so a
  container that isn't centered in the window (e.g. a top toolbar) is driven correctly.

### Changed
- `glass_scroll_to_element` returns an element only once it is actually **on-screen** — previously
  it could return one that was present in the accessibility tree but off-screen (which
  `glass_click_element` then refused). The result gains a `direction` field.
- The MCP tool descriptions and `get_info` are now backend-neutral (no per-platform text or
  duplicated backend list); the accepted backends are documented once on the `glass_start`
  `backend` param, and the per-OS clipboard/gesture/doctor specifics are collected in the
  [tools reference](docs/reference/tools.md)'s per-tool **Platform notes**.
- Operation-unsupported errors now name the active backend and point at `glass_capabilities`
  (which lists what the backend can do), instead of a terse or sometimes misleading message.
  This covers gesture/multi-touch (every backend) and window resize/move (the mobile backends)
  — for example the desktop backends no longer claim gestures are "only supported on the
  android backend".

### Fixed
- Wayland: text entry (`glass_type` and a typed `KeyEvent::Text`) is reliable under heavy machine
  load. The backend now uploads one keymap per string — each character at its own keycode — instead
  of swapping a single-keycode keymap for every character; previously, under load, a target that
  resolves keysyms lazily (an X11 app under Xwayland) could read a keystroke as the adjacent
  character.
- iOS: a log line an app emits at launch — before its first frame (e.g. an `applicationDidFinishLaunching`
  / `App.init` `os_log`) — is now captured, so you can gate readiness on it with `glass_wait_for_log`.
  Previously the unified-log stream attached only after the app had already launched, so launch-time lines
  were lost to the live tail; the stream now starts before launch and the launch waits until it is
  delivering.
- iOS: a Homebrew-installed `idb_companion` is now found automatically even when glass is launched by
  launchd (the `.app` / LaunchAgent), whose minimal `PATH` omits Homebrew's bindir — so input and the
  accessibility tree work without setting `GLASS_IDB_COMPANION` by hand.
- Visual baselines (`glass_baseline_save` / `glass_diff`) are written to an absolute, always-writable
  location instead of a working-directory-relative one that failed under launchd's read-only `/` cwd.
- iOS: `glass doctor` now always shows the `idb_companion` status in the `[ios]` section, even when
  iOS isn't the selected backend (e.g. a `.app` / LaunchAgent server defaulting to `GLASS_BACKEND=macos`
  while iOS is driven per-call). Previously the line was omitted unless `GLASS_BACKEND=ios`, so its
  absence read like "not found" for the input/accessibility precondition. When iOS isn't the active
  backend an absent companion is reported as an advisory warning rather than a hard failure.
- A `GLASS_BACKEND` set to an unrecognized value (a typo like `andriod`, or a name from a newer
  glass) is no longer silently ignored: `boot` logs a one-line stderr warning naming the value and
  the recognized backends, `glass doctor`'s "default backend" check reports a non-fatal `Warn` with
  a remedy, and `glass_capabilities` attaches a `warning` field. The fallback also now resolves to
  the host default (macos/windows/x11) rather than a hardcoded x11, so a typo on a mac/Windows host
  no longer drops to an x11 that host can't drive.
- `serve --http`: reconnecting an agent no longer fails with "another session is active". A
  Streamable-HTTP session is decoupled from its TCP connection and lingers until an explicit
  shutdown or a 5-minute idle timeout, so a client that dropped without cleanly disconnecting used
  to be locked out of its own server until that timeout expired. A new client now takes over the
  single live slot (last-client-wins), evicting the stale session, so reconnects succeed
  immediately.

## [0.4.0] - 2026-07-11

### Added
- An [iOS Simulator backend](docs/how-to/setup-ios.md) (`GLASS_BACKEND=ios`, macOS only): launch, capture,
  log streaming, and clipboard for native iOS apps in the Simulator, driven through `xcrun simctl`, plus
  input (tap/click, type, swipe, scroll) and the accessibility tree (snapshot, click-element, set-value)
  over [`idb_companion`](docs/how-to/setup-ios.md#input--accessibility) when it is installed. Includes a
  `glass doctor` preflight for Xcode, an installed iOS runtime, an available simulator, and `idb_companion`;
  with `--deep`, the preflight spawns `idb_companion` for real against an already-booted simulator (or runs a
  bounded `idb_companion --version` self-test when none is booted) and fails if the companion is broken or
  missing, rather than trusting that it is merely resolvable on `PATH`.
  Multi-touch gestures (`glass_gesture`) are not supported on the Simulator yet.
- A [Windows access model](docs/explanation/windows-permissions.md) explanation: Windows needs no
  per-app permission grants (unlike macOS), what actually gates access (interactive session, UAC/UIPI
  integrity levels, SmartScreen on unsigned downloads), and how to get past the first-run SmartScreen
  prompt.

### Changed
- Installing the optional Android companions is simpler and better documented: the setup guide,
  `glass doctor`, and `glass-mcp env` now lead with the easiest path — download `glass-agent.jar`
  and `glass-a11y.apk` from the [glass-android-agent](https://github.com/fixed-width/glass-android-agent)
  releases and drop them next to the `glass-mcp` binary, where glass discovers them automatically
  (no environment variables, no build step). `GLASS_ANDROID_AGENT_JAR` / `GLASS_ANDROID_A11Y_APK`
  are documented as overrides of that auto-discovery.
- Installing glass now starts from the Releases page rather than a source build: `README.md`,
  `docs/how-to/setup-linux.md`, and `docs/how-to/setup-windows.md` lead with the prebuilt binary.
- `docs/reference/platforms.md` documents the assets each release attaches.

### Fixed
- On the iOS Simulator backend, a `glass_drag` (or any `idb` HID gesture) longer than 30s no
  longer aborts mid-swipe with a timeout error. The per-gesture RPC deadline now scales with the
  gesture's own duration plus a margin, instead of a flat 30s, so a long drag runs to completion
  while a wedged companion is still bounded.
- `doctor --deep` no longer tells you to "run with --deep" for the Android `screencap` and
  `uiautomator` probes when you already passed `--deep`. Those deep probes only run when
  Android is the selected backend, so on another host backend the skip reason now points at
  the real gate: "set `GLASS_BACKEND=android`".
- The `glass_diff` tool reference now documents its `region` parameter — a window-relative scoped
  diff that also makes the reported `bbox` region-relative — which had been usable but undocumented.

## [0.3.1] - 2026-07-08

### Changed
- Documentation reorganized into a [Diátaxis](https://diataxis.fr) structure under
  [`docs/`](docs/README.md): a getting-started [tutorial](docs/tutorial/first-drive.md)
  that has an agent build and drive the interactive egui fixture end to end, task-focused
  how-to guides, complete reference (every tool, environment variable, and CLI command),
  and explanations of how glass works. The `README` is now a concise landing page. The old
  `docs/running-on-{linux,macos,windows}.md` guides moved to `docs/how-to/setup-*.md`
  (redirects left at the old paths).

### Fixed
- a11y: `a11y: true` now exposes the accessibility tree for **accesskit-based apps**
  (egui/winit/Slint/Iced) on Linux — glass advertises a screen reader on its private
  AT-SPI bus, which accesskit's adapter requires to activate. GTK/Qt were unaffected.
- The default backend on a macOS host is documented correctly as `macos` — the `glass_start`
  tool description, the `backend` parameter docs, and `glass-mcp env` previously named only
  "windows on Windows, else x11".

## [0.3.0] - 2026-07-07

### Added
- `glass_scroll_to_element`: blind-scroll an accessibility element into view.
- `window_id` parameter on `glass_screenshot`, `glass_wait_stable`, and
  `glass_wait_for_region` to target a specific window.
- `glass_diff` can be scoped to a window-relative sub-region.
- `glass_set_value` support for switches and dropdowns.
- macOS: `glass_start` launches `.app` bundles directly (LaunchServices /
  NSWorkspace), adopting or terminating the running app.
- macOS: `cmd`/`command` is accepted as an alias for the Super modifier.
- **macOS drag-install + double-click setup.** Tagged releases attach a notarized,
  Gatekeeper-clean universal `.dmg`; drag `GlassMcp.app` to `/Applications` and
  double-click it. A permission checklist guides granting Accessibility and Screen
  Recording (one at a time; granting Screen Recording relaunches glass so it takes
  effect), then glass installs a login item and runs as a visible **`glass ●`
  menu-bar app** showing the MCP endpoint, with Copy endpoint, Restart, Quit, and
  Uninstall.
- macOS: `glass-mcp uninstall` (and the menu-bar "Uninstall glass…") stop glass from
  starting at login; `glass-mcp status` reports whether glass is running and its endpoint.
- macOS: an app icon, so `GlassMcp.app` is no longer a blank bundle in Finder and the Dock.

### Fixed
- x11: off-screen captures are clipped to the display instead of failing with
  `BadMatch`.
- x11: window captures include popovers and menus drawn outside the window.
- `glass_click_element` auto-routes into an owning popover window.
- wayland: capture works on software renderers that advertise only 24-bpp shm.
- a11y: `glass_set_value` on a spin button writes through the Value interface;
  a role-only query no longer matches a bare focusable container.
- a11y: click the visible part of a clipped element, not the window edge.
- macOS: don't orphan a bundle launch whose window never appears; absorb the
  accessibility-snapshot startup race.

## [0.2.0] - 2026-07-04

### Added
- **macOS backend.** Drive native macOS apps: screen capture (ScreenCaptureKit),
  mouse/keyboard input (CGEvent), window management, accessibility reading
  (AXUIElement), and clipboard access — behind the same platform-agnostic core
  as the Linux and Windows backends.
- macOS containment: a Seatbelt sandbox for the launched app and a clipboard
  shim that isolates the app's pasteboard from the host.
- An immutable, provenance-attested release pipeline and macOS packaging.

### Changed
- Adopted default `rustfmt`, enforced in CI.

### Fixed
- x11: oversized capture requests report a clear error instead of failing
  opaquely.
- `glass_set_value` reports honestly when the written value cannot be read back.
- Windows: HGLOBAL handles are released via RAII guards.

## [0.1.2] - 2026-06-18

### Changed
- Share one SIMD pixel-swizzle kernel across the X11, Windows, and Wayland
  capture paths for faster frame conversion.

## [0.1.1] - 2026-06-18

### Added
- **Linux accessibility (opt-in).** An `a11y` flag starts a private AT-SPI
  session bus for the launched app and reads its accessibility tree, so an agent
  can address elements semantically instead of by pixel.
- **Audit log.** `--audit-log` (and `GLASS_AUDIT_*`) records every actuation to
  JSONL with content redaction; `glass_doctor` reports audit posture.
- `glass_drag` gains a `duration_ms` and paces synthetic drags across frames on
  X11 and Wayland.

### Fixed
- Input fidelity: hold the modifier across the whole frame for synthetic chords
  and scroll wheels; self-commit each keystroke when typing on X11 and Wayland;
  pace synthetic typing on Windows to avoid an OS injection race.
- Windows: adopt the boxed app window (not glass's launcher console) under
  Sandboxie; honest `set_value` and more robust window-finding.
- x11: focus the launched and selected window so synthetic keys land; translate
  stale-window X errors to `WindowNotFound`.
- Launched apps run in their own process group with graceful teardown, so
  helper processes don't orphan.

## [0.1.0] - 2026-06-08

First public release — open core, Apache-2.0.

### Added
- An MCP server giving an agent a **build → see → interact → debug** loop over
  external native GUI apps, driven as a black box regardless of toolkit or
  language.
- Linux **X11** and **Wayland** (wlroots) backends and a **Windows** backend
  (Windows.Graphics.Capture / SendInput / UI Automation) behind a
  platform-agnostic core.
- Core tools: `glass_start`, `glass_stop`, `glass_screenshot`, `glass_click`,
  `glass_list_windows`, `glass_select_window`, and `glass_doctor`.

[Unreleased]: https://github.com/fixed-width/glass/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/fixed-width/glass/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/fixed-width/glass/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/fixed-width/glass/compare/v0.5.0...v1.0.0
[0.5.0]: https://github.com/fixed-width/glass/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/fixed-width/glass/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/fixed-width/glass/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/fixed-width/glass/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/fixed-width/glass/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/fixed-width/glass/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/fixed-width/glass/compare/c1d0d5f...v0.1.1
[0.1.0]: https://github.com/fixed-width/glass/commit/c1d0d5f
