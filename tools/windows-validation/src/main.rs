//! glass Windows backend — pre-implementation validation probes.
//!
//! Each subcommand exercises one risky piece of the design spec so the make-or-break
//! gate (docs/superpowers/specs/2026-06-05-windows-validation-plan.md) can be checked
//! on the box with minimal manual work. Build with the MSVC toolchain:
//!   cargo run --release -- <subcommand> [args]

mod capture;
mod dpi;
mod input;
mod printwindow;
mod proc;
mod util;

use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

fn main() -> anyhow::Result<()> {
    // DPI awareness comes from the embedded manifest (build.rs) — authoritative,
    // applied before startup. This runtime call is a harmless fallback (it no-ops if
    // the manifest already set awareness, which it should).
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(1)..];

    match cmd {
        "displays" => list_displays()?,
        "windows" => list_windows(),
        "capture" => {
            let title = arg(rest, 0, "capture <title-substr> [out.png]")?;
            capture::run(title, rest.get(1).map(String::as_str).unwrap_or("capture.png"))?;
        }
        "printwindow" => {
            let title = arg(rest, 0, "printwindow <title-substr> [out.png]")?;
            printwindow::run(title, rest.get(1).map(String::as_str).unwrap_or("printwindow.png"))?;
        }
        "input" => input::run(arg(rest, 0, "input <title-substr>")?)?,
        "dpi" => dpi::run(rest.first().map(String::as_str))?,
        "discover" => discover(rest)?,
        "killtree" => {
            let cmd = after_spawn(rest)
                .ok_or_else(|| anyhow::anyhow!("usage: killtree --spawn -- <cmd> [args...]"))?;
            proc::killtree(cmd)?;
        }
        _ => usage(),
    }
    Ok(())
}

fn discover(rest: &[String]) -> anyhow::Result<()> {
    if let Some(cmd) = after_spawn(rest) {
        let pid = proc::spawn(cmd)?;
        println!("spawned pid {pid}: {}", cmd.join(" "));
        return proc::discover(pid, true);
    }
    if rest.first().map(String::as_str) == Some("--pid") {
        let pid: u32 = rest
            .get(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow::anyhow!("usage: discover --pid <N>"))?;
        return proc::discover(pid, false);
    }
    anyhow::bail!("usage: discover --pid <N>  |  discover --spawn -- <cmd> [args...]");
}

/// Everything after a literal `--` (the command to spawn).
fn after_spawn(rest: &[String]) -> Option<&[String]> {
    let dashes = rest.iter().position(|a| a == "--")?;
    let cmd = &rest[dashes + 1..];
    (!cmd.is_empty()).then_some(cmd)
}

fn arg<'a>(rest: &'a [String], i: usize, usage: &str) -> anyhow::Result<&'a str> {
    rest.get(i)
        .map(String::as_str)
        .ok_or_else(|| anyhow::anyhow!("usage: {usage}"))
}

fn list_displays() -> anyhow::Result<()> {
    println!("active displays (confirm a virtual display is present for headless capture):");
    for m in xcap::Monitor::all()? {
        println!("  {:<24} {}x{}", m.name(), m.width(), m.height());
    }
    Ok(())
}

fn list_windows() {
    println!("top-level app windows (pid / title / class):");
    for w in util::enum_top_windows() {
        if w.looks_like_app_window() && !w.title.is_empty() {
            println!("  pid {:>6}  '{}'  [{}]", w.pid, w.title, w.class);
        }
    }
}

fn usage() {
    eprintln!(
        "winval — glass Windows validation probes\n\
\n\
  displays                         list displays (confirm a virtual display)\n\
  windows                          list top-level app windows + pids\n\
  capture <title> [out.png]        item 1/2/7: WGC capture, assert non-blank\n\
  printwindow <title> [out.png]    item 8: PrintWindow (black on GPU apps)\n\
  input <title>                    item 3: focus + click + type via SendInput\n\
  dpi [title]                      item 5: confirm PMv2 + per-window DPI/bounds\n\
  discover --pid <N>               item 4: find the app window via descendant pids\n\
  discover --spawn -- <cmd...>     item 4: spawn then discover (Electron/Java)\n\
  killtree --spawn -- <cmd...>     item 6: Job-Object kill-tree teardown\n"
    );
}
