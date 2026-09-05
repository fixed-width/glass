# Measure interactions across application boundaries

Use these cases to measure packaged Electron, Android native/WebView transitions, a native form,
and a value transfer between two applications. You need the Linux/X11 prerequisites from
[measure interactions](measure-interactions.md), plus the runtimes for your selected cases.

1. Build the server and native fixture:

   ```bash
   cargo build --release --locked -p glass-mcp
   cargo build --release --locked --manifest-path crates/glass-fixture-egui/Cargo.toml --bin glass-interaction-native
   ```

   Build the [Electron distribution](../../examples/electron-interaction-fixture/README.md) with
   `npm ci` and `npm run package` in that fixture directory. The native fixture is a separate binary;
   its `--source` mode generates a fresh value when **Generate transfer** is clicked.

2. For Android, install an Android SDK, command-line tools, emulator, API 34, build-tools 34.0.0,
   and `system-images;android-34;google_apis;x86_64`. Java and accessible KVM are required. Build the
   APK with explicit versions:

   ```bash
   ANDROID_HOME=/absolute/path/android-sdk BUILD_TOOLS=34.0.0 ANDROID_PLATFORM=android-34 examples/android-role-fixture/build.sh
   ```

   Build the companion JAR and accessibility APK using the
   [companion repository](https://github.com/fixed-width/glass-android-agent). Supply their absolute
   paths below. The recipe needs native focus/typing and Android accessibility publication.
   It launches the fixture's `InteractionActivity`; existing role-probe activities remain available.

3. Save a configuration, replacing each absolute path:

   ```json
   {
     "cases": ["electron-form", "android-boundary", "native-form", "cross-application"],
     "repetitions": 10,
     "warmups": 1,
     "attempt_timeout_ms": 240000,
     "action_timeout_ms": 20000,
     "applications": {
       "electron": {
         "executable": "/checkout/examples/electron-interaction-fixture/dist/interaction-fixture-linux/interaction-fixture",
         "bundle": "/checkout/examples/electron-interaction-fixture/dist/interaction-fixture-linux"
       },
       "native": {
         "executable": "/checkout/crates/glass-fixture-egui/target/release/glass-interaction-native"
       },
       "android": {
         "sdk": "/absolute/path/android-sdk",
         "image": "system-images;android-34;google_apis;x86_64",
         "apk": "/checkout/examples/android-role-fixture/build/role-fixture.apk",
         "agent_jar": "/companion/build/glass-agent.jar",
         "a11y_apk": "/companion/a11y/build/outputs/apk/debug/a11y-debug.apk"
       }
     }
   }
   ```

   Configure exactly the application kinds needed by the selected cases. A browser path is required
   only when including web cases. Application cases currently support the Glass adapter. Keep the
   packaged Electron viewport at 1000×700. The native fixture uses a 600×500 window.

4. Run a diagnostic with `repetitions:1,warmups:0,allow_dirty:true`. Once it succeeds, commit the
   source, restore ten repetitions/one warm-up, and set `allow_dirty:false` for the measured cohort:

   ```bash
   python3 tools/interaction-bench/run.py preflight --config /absolute/path/config.json
   python3 tools/interaction-bench/run.py run --config /absolute/path/config.json
   python3 tools/interaction-bench/run.py validate /absolute/path/results
   python3 tools/interaction-bench/run.py summarize /absolute/path/results
   ```

Each Android attempt creates fresh emulator configuration and data under an owned temporary directory,
starts its own ADB server, and targets its exact serial. Boot, installation and accessibility setup
precede task timing. Emulator logs, preparation commands and device/WebView versions are retained.
The runner checks runtime file hashes before and after the cohort. These cold starts do not flush
host OS caches. All application attempts reserve 30 seconds for cleanup.

The cross-application case starts two independent MCP processes and keeps both applications alive.
The source app generates a fresh ticket; Glass observes it through MCP and types that exact value
into the destination. Both inventories and every call count toward the attempt. Each app owns its
own display, so this measures explicit value transfer between applications, without clipboard sharing.

See the [runner reference](../../tools/interaction-bench/README.md) for outcome and accounting rules.
