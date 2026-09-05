use clap::Parser;
use glass_mcp::cli::{Cli, Command, TraceCommand};
use glass_mcp::launch::NoArgLaunch;
use glass_mcp::{
    boot, launch, onboarding, run_debug_checklist, run_debug_grants, run_doctor, run_env,
    run_status, run_stdio_configured, run_uninstall, setup,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // AppKit setup precedes platform workers; offline evidence commands need none.
    #[cfg(target_os = "macos")]
    if !matches!(
        cli.command,
        Some(Command::Tools { .. } | Command::Trace { .. })
    ) {
        glass_macos::init_main_thread();
    }
    let audit_log = cli.audit_log;
    let tool_profile = cli.tool_profile;
    let trace = cli
        .trace_dir
        .map(|directory| glass_mcp::trace::TraceConfig::new(directory, cli.trace_max_bytes))
        .transpose()?;
    // Resolve (and OPEN, fail-closed) the audit sink only in the serving arms below —
    // never for doctor/env/gen-token, so those never create the audit file as a side effect.
    match cli.command {
        Some(Command::Trace { command }) => {
            let report = match command {
                TraceCommand::Inspect { directory, json } => {
                    let report = glass_mcp::trace::inspect(&directory)?;
                    glass_mcp::trace::print_inspection(&report, json)?;
                    report
                }
                TraceCommand::Export { directory, out } => {
                    let report = glass_mcp::trace::export(&directory, &out)?;
                    eprintln!(
                        "glass: exported {} trace to {out:?}",
                        if report.complete {
                            "complete"
                        } else {
                            "incomplete"
                        }
                    );
                    report
                }
            };
            if report.exit_code() != 0 {
                std::process::exit(report.exit_code());
            }
            Ok(())
        }
        Some(Command::Tools { json }) => glass_mcp::tool_profile::print_tools(tool_profile, json),
        // No subcommand: a LaunchServices double-click routes to onboarding; an MCP client's
        // stdio spawn (the default, and the only case off macOS) serves MCP over stdio.
        None => match launch::detect_no_arg_launch() {
            NoArgLaunch::Onboarding => onboarding::run(onboarding::DEFAULT_ADDR),
            NoArgLaunch::StdioServe => {
                let (sink, report) =
                    glass_mcp::audit::resolve(audit_log.as_deref(), |k| std::env::var(k).ok())?;
                run_stdio_configured(boot(sink), report, tool_profile, trace.as_ref()).await
            }
        },
        Some(Command::Doctor { deep, json, color }) => {
            run_doctor(deep, json, audit_log.as_deref(), color)
        }
        Some(Command::Env { json, color }) => run_env(json, color),
        Some(Command::Serve {
            http,
            addr,
            token_file,
            menubar,
        }) => {
            #[cfg(feature = "network")]
            {
                if menubar {
                    // The visible menu-bar app (macOS only) — `menubar::run` binds and
                    // serves (see menubar.rs). Reuse `serve::config::parse_args` (the
                    // "single source of truth" resolver `serve::run` itself delegates to
                    // below) rather than duplicate its token-precedence/exposure-parsing
                    // logic.
                    #[cfg(target_os = "macos")]
                    {
                        // `--menubar` implies serving over HTTP (the only transport today), so
                        // don't require the caller to also pass `--http`: `serve --menubar`
                        // alone must not error "serve requires --http". The plist passes both;
                        // this makes the flag redundant rather than mandatory.
                        let _ = http;
                        let mut argv: Vec<String> = vec!["--http".into()];
                        if let Some(a) = addr {
                            argv.push("--addr".into());
                            argv.push(a);
                        }
                        if let Some(tf) = token_file {
                            argv.push("--token-file".into());
                            argv.push(tf);
                        }
                        let mut cfg = glass_mcp::serve::config::parse_args(
                            &argv,
                            std::env::var("GLASS_TOKEN").ok(),
                            |p| std::fs::read_to_string(p),
                        )
                        .map_err(|e| anyhow::anyhow!("glass serve --menubar: {e}"))?;
                        cfg.tool_profile = tool_profile;
                        cfg.trace = trace;
                        glass_mcp::menubar::run(cfg)
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        let _ = (http, addr, token_file, &audit_log);
                        anyhow::bail!("--menubar is macOS-only")
                    }
                } else {
                    let (sink, report) =
                        glass_mcp::audit::resolve(audit_log.as_deref(), |k| std::env::var(k).ok())?;
                    glass_mcp::serve::run_configured(
                        http,
                        addr,
                        token_file,
                        sink,
                        report,
                        tool_profile,
                        trace,
                    )
                    .await
                }
            }
            #[cfg(not(feature = "network"))]
            {
                let _ = (http, addr, token_file, menubar, &audit_log);
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
        Some(Command::Setup {
            non_interactive,
            launchagent,
            no_launchagent,
            addr,
        }) => {
            setup::run(setup::SetupArgs {
                non_interactive,
                launchagent,
                no_launchagent,
                addr,
            })?;
            Ok(())
        }
        Some(Command::Status { addr }) => run_status(addr.as_deref()),
        #[cfg(feature = "self-update")]
        Some(Command::Update {
            check,
            yes,
            skip_attestation,
            json,
            color,
        }) => glass_mcp::run_update(check, yes, skip_attestation, json, color).await,
        Some(Command::Uninstall) => run_uninstall(),
        Some(Command::DebugGrants) => run_debug_grants(),
        Some(Command::DebugChecklist) => run_debug_checklist(),
    }
}
