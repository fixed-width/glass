# android-role-fixture

One screen of stock `android.widget` controls, for deciding what Android's accessibility
vocabulary can express. Each control answers one question: what widget class does Android report
for it? That answer is what a cell in
[docs/reference/a11y-roles.md](../../docs/reference/a11y-roles.md) records.

The table below is what these controls reported on an API 34 emulator — a reading, not a
guarantee. Re-run it rather than trusting it; the read step below prints the API level it saw, and
`build.sh` prints the platform it built against.

| Control | Reported |
|---|---|
| `Toolbar` | `android.view.ViewGroup` — no toolbar token |
| `TabHost` / `TabWidget` | themselves; a tab item is a `LinearLayout` with `selected=true` |
| `ListView`, `GridView` | themselves; items report the item view's own class |
| `TableLayout`, `TableRow` | themselves; a cell is whatever view the row holds |
| `NumberPicker` | itself |
| `ProgressBar` | itself |
| `PopupMenu` (button) | `android.widget.ListView`, entries `LinearLayout`/`RelativeLayout` |
| `AlertDialog` (button) | `FrameLayout`/`LinearLayout` panels under `android:id/parentPanel` |
| `Button` with `text` + a different `contentDescription` | `android.widget.Button`; both readers name it by its `text` and carry the `contentDescription` as `desc="…"` |
| `EditText` with a hint, no `contentDescription`, no resource id | `android.widget.EditText`; unnamed, and through the on-device companion the hint is the description — read as `#35 TextField desc="Search settings"`, which `glass_a11y_marks` labels from that description. `uiautomator` sees no hint, so there it is unnamed and undescribed |

`PopupMenu` and `AlertDialog` each need a tap. Each opens as the topmost window, so dump it
separately.

A subclass inherits the accessibility class name of the framework class it extends unless it
overrides `getAccessibilityClassName()` — which is why the toolbar arrives as `ViewGroup`, and why
`androidx.appcompat.widget.Toolbar` and `MaterialToolbar`, neither of which overrides it, arrive
that way too.

## Build and install

Needs a JDK, `zip`, and an Android SDK (`ANDROID_HOME`, `ANDROID_SDK_ROOT`, or one of
`~/Android/Sdk`, `~/Library/Android/sdk`, `~/android-sdk`). No Gradle — the build is `javac` →
`d8` → `aapt2` → `apksigner`, signed with a throwaway key generated on first build and kept
across rebuilds so `adb install -r` keeps working:

```bash
./build.sh                                   # → build/role-fixture.apk
adb install -r build/role-fixture.apk
adb shell am start -n tech.fixedwidth.glassrolefixture/.MainActivity
```

## Web view

A second screen holds a stock `WebView` on the shared page in
[../web-role-fixture](../web-role-fixture), copied into the APK's assets at build time and loaded
over `file:///android_asset/index.html`. It is a separate activity so the readings above do not
move:

```bash
adb shell am start -n tech.fixedwidth.glassrolefixture/.WebActivity
```

Its reading answers the `Document` row of
[docs/reference/a11y-roles.md](../../docs/reference/a11y-roles.md): whether the page's own
elements reach either reader, and under which widget classes they arrive.

## Read the tree

Either reader answers the same question — the widget class is the token both key off.

```bash
adb shell getprop ro.build.version.sdk      # the API level this reading is from
adb shell uiautomator dump /sdcard/roles.xml && adb shell cat /sdcard/roles.xml
```

Through glass's own probe, which prints a role histogram per app:

```bash
GLASS_A11Y_PROBE_APPS=tech.fixedwidth.glassrolefixture/.MainActivity \
  cargo test -p glass-android --test role_probe -- --ignored --nocapture --test-threads=1
```

That covers the `uiautomator` reader; add `GLASS_ANDROID_A11Y_APK=<path>` to run the same probe
through the on-device accessibility service, which otherwise skips. Run the two probes serially
with `--test-threads=1` when they share a device.

Both probes automatically require the Button pinned above the scrolling content to report
`name="Save"` and `desc="Save changes"`. Rebuild and reinstall the fixture before probing.
A missing or changed pair fails the test and names the reader's
content-description arm, even if other nodes have descriptions. The fully qualified
`tech.fixedwidth.glassrolefixture.MainActivity` activity name also enables this check. Other
apps and fixture activities remain advisory: a zero count explains that the app may expose no
descriptions or the reader may have regressed.

Semantics Android carries outside the widget class — `CollectionInfo`, `CollectionItemInfo`,
`isHeading` — reach neither reader: the `uiautomator` dump has no attribute for them, and the
service reader parses only class, text, description and bounds. Those cells stay `gap` rather than
`n/a`, because closing them is glass's own work; this fixture cannot show them either way.

## Occlusion

`OcclusionActivity` exposes Covered action, Cover action, Positive action, and a counter label
(`target=N cover=N positive=N`). Positive action removes the cover. Its default mode puts two
buttons at identical bounds. An intent extra selects `none`, `narrow`, `view-pass`, `hidden`,
`delegate`, `window`, `window-narrow`, or `window-pass`:

```bash
adb shell am start -S -n tech.fixedwidth.glassrolefixture/.OcclusionActivity --es mode window
```

`view-pass` reports a clickable button but declines touch events; `hidden` intercepts taps while
omitted from accessibility. `delegate` installs a parent touch delegate over the target's bounds.
These modes expose the limits of inferring input delivery from an accessibility tree.

The separate-window modes also have launch aliases for Glass's component-only launch interface:
`.OcclusionWindow`, `.OcclusionWindowNarrow`, and `.OcclusionWindowPass`. The last uses
`FLAG_NOT_TOUCHABLE`. Run the regression with a booted emulator and a companion built with
`pointer_window` version 1 support (API 33+):

```bash
GLASS_ANDROID_A11Y_APK=/path/to/a11y-debug.apk \
GLASS_ANDROID_ROLE_FIXTURE_APK="$PWD/examples/android-role-fixture/build/role-fixture.apk" \
  cargo test -p glass-android --test occlusion_loop -- --ignored --nocapture
```

The test checks zero dispatch and unchanged counters for both covering-window sizes, successful
taps through the untouchable window, and restored target clicks after removing the cover. It is
run separately from `scripts/test-android.sh`, which uses the pinned released companion.
