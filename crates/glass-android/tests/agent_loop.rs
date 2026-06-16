//! Live agent round-trip. Ignored; run with a booted AVD + the built agent jar:
//!   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
//!   GLASS_ANDROID_AGENT_JAR=$HOME/git-sources/fw/glass-android-agent/build/glass-agent.jar \
//!     cargo test -p glass-android --test agent_loop -- --ignored --nocapture
//!
//! Pushes + launches the real agent, then asserts a clipboard round-trip + input via the
//! AgentInjector path.

use glass_android::{AgentRegistry, EmulatorRegistry};
use glass_core::{AppSpec, KeyEvent, MouseButton, Platform, PointerEvent, SandboxLevel};

fn settings_spec() -> AppSpec {
    AppSpec {
        build: None,
        run: vec!["com.android.settings/.Settings".to_string()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 15_000,
        sandbox: SandboxLevel::Off,
        a11y: false,
    }
}

#[test]
#[ignore = "requires a booted AVD + GLASS_ADB + GLASS_ANDROID_AGENT_JAR"]
fn agent_clipboard_and_input_roundtrip() {
    // Keep the registry alive for the whole test (it owns the launched agent process), and
    // tear the agent down explicitly at the end — in production `Glass`'s shutdown hook does
    // this; a test must not leak the process (the registry doesn't kill on drop).
    let agents = AgentRegistry::new();
    let mut p = glass_android::AndroidPlatform::from_env(&EmulatorRegistry::new(), &agents)
        .expect("attach + agent");
    p.start_app(&settings_spec()).expect("launch settings");

    // Clipboard round-trip via the agent.
    p.set_clipboard("glass-agent-✓").expect("set clipboard");
    assert_eq!(p.get_clipboard().expect("get clipboard"), "glass-agent-✓");

    // Input via the AgentInjector (must not error).
    p.send_pointer(&PointerEvent::Click {
        x: 100,
        y: 200,
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![],
    })
    .expect("agent tap");
    p.send_key(&KeyEvent::Text("hi".into())).expect("agent text");

    p.stop_app().expect("stop");
    drop(p); // close the platform's agent connection before stopping the agent
    agents.shutdown(); // kill the launched agent + remove the forward (no leaked process)
}
