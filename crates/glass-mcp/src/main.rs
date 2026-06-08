use clap::Parser;
use glass_mcp::cli::{Cli, Command};
use glass_mcp::{boot, run_doctor, run_env, run_stdio};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        // No subcommand: serve MCP over stdio (the default).
        None => run_stdio(boot()).await,
        Some(Command::Doctor { deep, json }) => run_doctor(deep, json),
        Some(Command::Env { json }) => run_env(json),
        Some(Command::Serve { http, addr, token_file }) => {
            #[cfg(feature = "network")]
            {
                glass_mcp::serve::run(http, addr, token_file).await
            }
            #[cfg(not(feature = "network"))]
            {
                let _ = (http, addr, token_file);
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
