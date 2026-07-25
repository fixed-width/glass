# android-role-fixture

One screen of stock `android.widget` controls, for deciding what Android's accessibility
vocabulary can express. Each control answers one question: what widget class does Android report
for it? That answer is what a cell in
[docs/reference/a11y-roles.md](../../docs/reference/a11y-roles.md) records — a role is marked
`n/a` there only where a control was watched to arrive carrying no token for it.

| Control | Reports (API 34 emulator) |
|---|---|
| `Toolbar` | `android.view.ViewGroup` — no toolbar token |
| `TabHost` / `TabWidget` | themselves; a tab item is a `LinearLayout` with `selected=true` |
| `ListView`, `GridView` | themselves; items report the item view's own class |
| `TableLayout`, `TableRow` | themselves; a cell is whatever view the row holds |
| `NumberPicker` | itself |
| `ProgressBar` | itself |
| `PopupMenu` (button) | `android.widget.ListView`, entries `LinearLayout`/`RelativeLayout` |
| `AlertDialog` (button) | `FrameLayout`/`LinearLayout` panels under `android:id/parentPanel` |

The last two need a tap. Each opens as the topmost window, so dump it separately.

## Build and install

Needs a JDK and an Android SDK (`ANDROID_HOME`, or `~/android-sdk`). No Gradle — the build is
`javac` → `d8` → `aapt2` → `apksigner`, signed with a throwaway key it generates:

```bash
./build.sh                                   # → build/role-fixture.apk
adb install -r build/role-fixture.apk
adb shell am start -n tech.fixedwidth.glassrolefixture/.MainActivity
```

## Read the tree

Either reader answers the same question — the class is the token both key off.

```bash
adb shell uiautomator dump /sdcard/roles.xml && adb shell cat /sdcard/roles.xml
```

Through glass's own probe, which prints a role histogram per app:

```bash
GLASS_A11Y_PROBE_APPS=tech.fixedwidth.glassrolefixture/.MainActivity \
  cargo test -p glass-android --test role_probe -- --ignored --nocapture
```

Semantics Android carries outside the class name — `CollectionInfo`, `CollectionItemInfo`,
`isHeading` — reach neither reader: the `uiautomator` dump has no attribute for them and the
on-device service protocol does not send them. That is why those cells stay `gap` rather than
`n/a`.
