//! Command-line interface for `glass-mcp` (clap derive). The parser lives here, not in
//! `main.rs`, so it can be unit-tested. `main.rs` calls [`Cli::parse`] and dispatches.
//!
//! `clap` gives `--help` (global + per-subcommand), `--version`, validation, and usage
//! errors for free, and keeps the help in sync as flags change. With no subcommand,
//! `glass-mcp` serves MCP over stdio (the default).

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "glass-mcp",
    version,
    about = "glass MCP server — a build→see→interact→debug loop over native GUI apps",
    after_help = "With no command, glass-mcp serves MCP over stdio (the default)."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Append a JSONL audit log of every actuation to PATH (also GLASS_AUDIT_LOG).
    /// Opt-in; off when unset. Content redacted by default
    /// (GLASS_AUDIT_CONTENT=none|redacted|full, GLASS_AUDIT_PREFIX_LEN=N).
    #[arg(long, global = true, value_name = "PATH")]
    pub audit_log: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Check the environment glass needs and print remedies.
    Doctor {
        /// Additionally spawn + tear down the display to prove it starts.
        #[arg(long)]
        deep: bool,
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// List glass's configuration environment variables (purpose, default, current).
    Env {
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Serve MCP over the network via Streamable HTTP.
    Serve {
        /// Use the Streamable HTTP transport (required — the only network transport today).
        #[arg(long)]
        http: bool,
        /// Address to bind (default 127.0.0.1:7300).
        #[arg(long)]
        addr: Option<String>,
        /// File containing the bearer token (overrides GLASS_TOKEN).
        #[arg(long)]
        token_file: Option<String>,
        /// Also run the visible menu-bar app (macOS). Implies --http; the app serves
        /// MCP on a background thread while an NSStatusItem shows it is running.
        #[arg(long)]
        menubar: bool,
    },
    /// Generate a bearer token for the network transport.
    GenToken {
        /// Write the token to PATH (0600 on Unix) instead of stdout.
        #[arg(long)]
        out: Option<String>,
    },
    /// Guided macOS first-run: request the TCC grants, install the run integration, confirm.
    Setup {
        /// Fail instead of prompting (scripting/CI).
        #[arg(long)]
        non_interactive: bool,
        /// Install the gui/uid LaunchAgent (unattended serve --http) instead of asking.
        #[arg(long)]
        launchagent: bool,
        /// Do NOT install the LaunchAgent (attended/stdio) instead of asking.
        #[arg(long, conflicts_with = "launchagent")]
        no_launchagent: bool,
        /// LaunchAgent HTTP bind address (default 127.0.0.1:7300).
        #[arg(long)]
        addr: Option<String>,
    },
    /// Report whether a glass server is running and its endpoint (reads /healthz).
    Status {
        /// Address to check (default: 127.0.0.1:7300).
        #[arg(long)]
        addr: Option<String>,
    },
    /// Spike/diagnostic: poll the two TCC grants once a second in one long-lived process,
    /// so you can watch which flips live when granted (Accessibility) vs. stays stale until
    /// relaunch (Screen Recording). macOS-only; hidden from help.
    #[command(hide = true)]
    DebugGrants,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subcommand_is_none() {
        let cli = Cli::try_parse_from(["glass-mcp"]).unwrap();
        assert!(
            cli.command.is_none(),
            "bare invocation must mean the stdio default"
        );
    }

    #[test]
    fn doctor_flags_parse() {
        let cli = Cli::try_parse_from(["glass-mcp", "doctor", "--deep", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Doctor {
                deep: true,
                json: true
            })
        ));
        // flags default to false
        let cli = Cli::try_parse_from(["glass-mcp", "doctor"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Doctor {
                deep: false,
                json: false
            })
        ));
    }

    #[test]
    fn env_json_parses() {
        let cli = Cli::try_parse_from(["glass-mcp", "env", "--json"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Env { json: true })));
    }

    #[test]
    fn serve_flags_use_kebab_case() {
        let cli = Cli::try_parse_from([
            "glass-mcp",
            "serve",
            "--http",
            "--addr",
            "0.0.0.0:7300",
            "--token-file",
            "/t",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Serve {
                http,
                addr,
                token_file,
                menubar,
            }) => {
                assert!(http);
                assert_eq!(addr.as_deref(), Some("0.0.0.0:7300"));
                assert_eq!(token_file.as_deref(), Some("/t"));
                assert!(!menubar, "menubar defaults to false");
            }
            other => panic!("expected serve, got {other:?}"),
        }
    }

    #[test]
    fn gen_token_subcommand_is_kebab_case() {
        let cli = Cli::try_parse_from(["glass-mcp", "gen-token", "--out", "/p"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::GenToken { out: Some(_) })
        ));
    }

    #[test]
    fn audit_log_is_global_and_optional() {
        let c = Cli::try_parse_from(["glass-mcp", "--audit-log", "/p"]).unwrap();
        assert!(c.command.is_none());
        assert_eq!(c.audit_log.as_deref(), Some("/p"));
        let c = Cli::try_parse_from(["glass-mcp", "serve", "--http", "--audit-log", "/q"]).unwrap();
        assert_eq!(c.audit_log.as_deref(), Some("/q"));
        assert!(Cli::try_parse_from(["glass-mcp"])
            .unwrap()
            .audit_log
            .is_none());
    }

    #[test]
    fn unknown_subcommand_is_an_error() {
        assert!(Cli::try_parse_from(["glass-mcp", "bogus"]).is_err());
    }

    #[test]
    fn setup_flags_parse() {
        let cli = Cli::try_parse_from(["glass-mcp", "setup", "--non-interactive", "--launchagent"])
            .unwrap();
        match cli.command {
            Some(Command::Setup {
                non_interactive,
                launchagent,
                no_launchagent,
                addr,
            }) => {
                assert!(non_interactive);
                assert!(launchagent);
                assert!(!no_launchagent);
                assert!(addr.is_none());
            }
            other => panic!("expected setup, got {other:?}"),
        }
    }

    #[test]
    fn setup_flags_default_to_false() {
        let cli = Cli::try_parse_from(["glass-mcp", "setup"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Setup {
                non_interactive: false,
                launchagent: false,
                no_launchagent: false,
                addr: None
            })
        ));
    }

    #[test]
    fn setup_addr_parses() {
        let cli = Cli::try_parse_from(["glass-mcp", "setup", "--addr", "0.0.0.0:7300"]).unwrap();
        match cli.command {
            Some(Command::Setup { addr, .. }) => assert_eq!(addr.as_deref(), Some("0.0.0.0:7300")),
            other => panic!("expected setup, got {other:?}"),
        }
    }

    #[test]
    fn setup_launchagent_and_no_launchagent_conflict() {
        let err = Cli::try_parse_from(["glass-mcp", "setup", "--launchagent", "--no-launchagent"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn serve_accepts_menubar_flag() {
        let cli = Cli::try_parse_from(["glass-mcp", "serve", "--http", "--menubar"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Serve { menubar: true, .. })
        ));
    }

    #[test]
    fn status_subcommand_parses_with_optional_addr() {
        let cli = Cli::try_parse_from(["glass-mcp", "status"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Status { addr: None })));
        let cli = Cli::try_parse_from(["glass-mcp", "status", "--addr", "127.0.0.1:7300"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Status { addr: Some(_) })
        ));
    }

    #[test]
    fn help_and_version_are_handled() {
        let help = Cli::try_parse_from(["glass-mcp", "--help"]).unwrap_err();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        let version = Cli::try_parse_from(["glass-mcp", "--version"]).unwrap_err();
        assert_eq!(version.kind(), clap::error::ErrorKind::DisplayVersion);
        // per-subcommand help works too
        let dh = Cli::try_parse_from(["glass-mcp", "doctor", "--help"]).unwrap_err();
        assert_eq!(dh.kind(), clap::error::ErrorKind::DisplayHelp);
    }
}
