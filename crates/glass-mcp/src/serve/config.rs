//! `serve` configuration: argument parsing, token resolution, exposure rules.

use std::net::SocketAddr;

/// Resolved configuration for a `serve` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeConfig {
    pub addr: SocketAddr,
    pub token: Option<String>,
}

pub const DEFAULT_ADDR: &str = "127.0.0.1:7300";

/// The security posture implied by the bind address + token, per spec D4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// Loopback bind with no token: allowed, but warn (also the SSH-tunnel endpoint).
    LoopbackOpen,
    /// Non-loopback bind with no token: refuse to start.
    ExposedNoToken,
    /// A token is set: run regardless of bind.
    Authenticated,
}

impl ServeConfig {
    pub fn exposure(&self) -> Exposure {
        if self.token.is_some() {
            Exposure::Authenticated
        } else if self.addr.ip().is_loopback() {
            Exposure::LoopbackOpen
        } else {
            Exposure::ExposedNoToken
        }
    }
}

/// Parse `serve` args. `token_env` is the value of `GLASS_TOKEN` (so this is pure
/// and testable). `read_file` reads a `--token-file` path (injected for testing).
pub fn parse_args(
    args: &[String],
    token_env: Option<String>,
    read_file: impl Fn(&str) -> std::io::Result<String>,
) -> Result<ServeConfig, String> {
    let mut http = false;
    let mut addr_s: Option<String> = None;
    let mut token_file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--http" => http = true,
            "--addr" => {
                addr_s = Some(args.get(i + 1).ok_or("--addr needs a value")?.clone());
                i += 1;
            }
            "--token-file" => {
                token_file = Some(args.get(i + 1).ok_or("--token-file needs a value")?.clone());
                i += 1;
            }
            other => return Err(format!("unknown serve argument: {other}")),
        }
        i += 1;
    }
    if !http {
        return Err("serve requires --http (the only transport today)".into());
    }
    let addr_s = addr_s.unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let addr: SocketAddr =
        addr_s.parse().map_err(|e| format!("invalid --addr {addr_s:?}: {e}"))?;

    // --token-file wins over GLASS_TOKEN.
    let token = match token_file {
        Some(path) => {
            let raw = read_file(&path).map_err(|e| format!("reading --token-file {path:?}: {e}"))?;
            let t = raw.trim().to_string();
            if t.is_empty() {
                return Err(format!("--token-file {path:?} is empty"));
            }
            Some(t)
        }
        None => token_env.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
    };
    Ok(ServeConfig { addr, token })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_file(_: &str) -> std::io::Result<String> {
        Err(std::io::Error::other("no file expected"))
    }

    #[test]
    fn defaults_addr_and_no_token() {
        let c = parse_args(&["--http".into()], None, no_file).unwrap();
        assert_eq!(c.addr.to_string(), "127.0.0.1:7300");
        assert_eq!(c.token, None);
    }

    #[test]
    fn explicit_addr_and_env_token() {
        let c = parse_args(
            &["--http".into(), "--addr".into(), "0.0.0.0:9000".into()],
            Some("  secret  ".into()),
            no_file,
        )
        .unwrap();
        assert_eq!(c.addr.to_string(), "0.0.0.0:9000");
        assert_eq!(c.token.as_deref(), Some("secret")); // trimmed
    }

    #[test]
    fn token_file_overrides_env() {
        let c = parse_args(
            &["--http".into(), "--token-file".into(), "/x".into()],
            Some("envtok".into()),
            |_| Ok("filetok\n".into()),
        )
        .unwrap();
        assert_eq!(c.token.as_deref(), Some("filetok"));
    }

    #[test]
    fn requires_http() {
        assert!(parse_args(&[], None, no_file).is_err());
    }

    #[test]
    fn rejects_unknown_arg() {
        assert!(parse_args(&["--http".into(), "--nope".into()], None, no_file).is_err());
    }

    #[test]
    fn empty_token_file_is_error() {
        let r = parse_args(&["--http".into(), "--token-file".into(), "/x".into()], None, |_| {
            Ok("   \n".into())
        });
        assert!(r.is_err());
    }

    #[test]
    fn trailing_flag_without_value_is_error() {
        // Guards the manual arg-index loop against an off-by-one when a value-taking
        // flag is the last argument.
        assert!(parse_args(&["--http".into(), "--addr".into()], None, no_file).is_err());
        assert!(parse_args(&["--http".into(), "--token-file".into()], None, no_file).is_err());
    }

    fn cfg(addr: &str, token: Option<&str>) -> ServeConfig {
        ServeConfig { addr: addr.parse().unwrap(), token: token.map(String::from) }
    }

    #[test]
    fn exposure_loopback_no_token_warns() {
        assert_eq!(cfg("127.0.0.1:7300", None).exposure(), Exposure::LoopbackOpen);
        assert_eq!(cfg("[::1]:7300", None).exposure(), Exposure::LoopbackOpen);
    }

    #[test]
    fn exposure_exposed_no_token_refused() {
        assert_eq!(cfg("0.0.0.0:7300", None).exposure(), Exposure::ExposedNoToken);
        assert_eq!(cfg("192.168.1.5:7300", None).exposure(), Exposure::ExposedNoToken);
    }

    #[test]
    fn exposure_with_token_ok() {
        assert_eq!(cfg("0.0.0.0:7300", Some("t")).exposure(), Exposure::Authenticated);
        assert_eq!(cfg("127.0.0.1:7300", Some("t")).exposure(), Exposure::Authenticated);
    }
}
