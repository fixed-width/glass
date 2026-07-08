# Set up glass for iOS

The iOS backend drives a native app on an **iOS Simulator** over `xcrun simctl`. It is
**macOS-only** — the Simulator only runs on macOS, and `xcrun`/`simctl` ship with Xcode — so
unlike the Android backend, there is no cross-host story here.

Select the backend per launch with `glass_start`'s `backend: "ios"`, or make it the default with
`GLASS_BACKEND=ios`.

> This release observes iOS apps — launch, screenshot, logs, clipboard. Tapping/typing and the
> accessibility tree are not yet available; `glass_click`/`glass_type`/`glass_key` and the a11y
> tools return an unsupported error on this backend.

## Install Xcode and a Simulator runtime

You need the **full Xcode** app (not just the Command Line Tools) — it ships `simctl` and the
Simulator app itself:

```bash
xcode-select -p   # should print .../Xcode.app/Contents/Developer, not CommandLineTools
```

If it prints a Command Line Tools path, or nothing, install Xcode from the App Store, then point
`xcode-select` at it:

```bash
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
```

Xcode doesn't bundle an iOS runtime by default. Download one (this can take a while — it's a
multi-GB image):

```bash
xcodebuild -downloadPlatform iOS
```

You also need at least one iPhone simulator device. Xcode creates a few by default; if none
exist, create one:

```bash
xcrun simctl list devices available   # see what's already there
xcrun simctl create "iPhone glass" "iPhone 17"
```

## Attach-or-boot

Like the Android backend, glass prefers to attach: if a Simulator is already booted it uses it;
otherwise it boots the newest available iPhone simulator itself and shuts it down again on
shutdown.

- `GLASS_IOS_UDID` — drive an exact device by UDID (see `xcrun simctl list devices`).
- `GLASS_IOS_DEVICE` — boot a device by name, e.g. `"iPhone 17"`, when none is running (ignored
  if `GLASS_IOS_UDID` is set).
- `GLASS_SIMULATOR_KEEP` — set to keep a glass-booted Simulator running past shutdown instead of
  shutting it down.

(All variables: [reference/environment.md](../reference/environment.md).)

## Launching an app

`glass_start`'s `run[0]` is either a path to a `.app` bundle (glass installs it on the Simulator
for you) or the bundle id of an app already installed, e.g.:

```jsonc
glass_start { "backend": "ios", "run": ["/path/to/YourApp.app"] }
// or, already installed:
glass_start { "backend": "ios", "run": ["tech.example.YourApp"] }
```

The Simulator reports one fullscreen window per app — there's no window management (resize/move
are unsupported, matching a real device).

## Check the setup

```bash
GLASS_BACKEND=ios glass-mcp doctor
```

Reports whether full Xcode is active, `simctl` works, an iOS runtime is downloaded, and an iPhone
simulator is available — each failing check comes with its own remedy (the commands above). Then
[connect your agent](connect-an-agent.md).
