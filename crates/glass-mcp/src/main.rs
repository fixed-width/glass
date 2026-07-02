use clap::Parser;
use glass_mcp::cli::{Cli, Command};
use glass_mcp::{boot, run_doctor, run_env, run_stdio, setup};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // One-time AppKit/WindowServer init (`NSApplication.sharedApplication`) — must run on
    // the process's real main thread ("thread 0"), exactly once, before anything spawns the
    // `glass-platform` worker thread that `GlassServer::new` (server.rs) parents every
    // `MacosPlatform` call to. This is the FIRST statement of `main()`, with no `.await`
    // above it: `#[tokio::main]`'s expansion builds the runtime and calls `Runtime::block_on`
    // on the thread that invoked `main()` (thread 0), and `block_on` polls the async body
    // itself on that same calling thread — so this line runs synchronously on thread 0
    // during that first poll, strictly before `boot()`/`run_stdio()` below ever construct a
    // `GlassServer` and spawn its worker thread. See glass-macos's `ffi::app_kit_init` doc
    // and `.superpowers/sdd/thread0-spike-report.md` for why one call here is sufficient —
    // every later `MacosPlatform` call, from any thread, is a cheap no-op after this.
    #[cfg(target_os = "macos")]
    glass_macos::init_main_thread();

    let cli = Cli::parse();
    let audit_log = cli.audit_log;
    // Resolve (and OPEN, fail-closed) the audit sink only in the serving arms below —
    // never for doctor/env/gen-token, so those never create the audit file as a side effect.
    match cli.command {
        // No subcommand: serve MCP over stdio (the default).
        None => {
            let (sink, report) =
                glass_mcp::audit::resolve(audit_log.as_deref(), |k| std::env::var(k).ok())?;
            run_stdio(boot(sink), report).await
        }
        Some(Command::Doctor { deep, json }) => run_doctor(deep, json, audit_log.as_deref()),
        Some(Command::Env { json }) => run_env(json),
        Some(Command::Serve { http, addr, token_file }) => {
            #[cfg(feature = "network")]
            {
                let (sink, report) =
                    glass_mcp::audit::resolve(audit_log.as_deref(), |k| std::env::var(k).ok())?;
                glass_mcp::serve::run(http, addr, token_file, sink, report).await
            }
            #[cfg(not(feature = "network"))]
            {
                let _ = (http, addr, token_file, &audit_log);
                anyhow::bail!(
                    "`serve` (the network transport) is not included in this build; it \
                     requires the default-on `network` feature, which a \
                     --no-default-features build omits"
                )
            }
        }
        Some(Command::GenToken { out }) => {
            #[cfg(feature = "network")]
            {
                glass_mcp::serve::gen_token(out)
            }
            #[cfg(not(feature = "network"))]
            {
                let _ = out;
                anyhow::bail!(
                    "`gen-token` (the network transport) is not included in this build; it \
                     requires the default-on `network` feature, which a \
                     --no-default-features build omits"
                )
            }
        }
        Some(Command::Setup { non_interactive, launchagent, no_launchagent, addr }) => {
            setup::run(setup::SetupArgs { non_interactive, launchagent, no_launchagent, addr })?;
            Ok(())
        }
    }
}
