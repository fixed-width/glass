use clap::Parser;
use glass_mcp::cli::{Cli, Command};
use glass_mcp::{boot, run_doctor, run_env, run_stdio};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    }
}
