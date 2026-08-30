//! Network MCP transport (Streamable HTTP). Feature-gated behind `network`.

pub mod auth;
pub mod config;
pub mod session_gate;
pub mod token;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::routing::get;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;
use tokio_util::sync::CancellationToken;

use crate::server::GlassServer;
use config::{Exposure, ServeConfig};
use session_gate::SingleSessionManager;

/// HTTP drain allowance after shutdown begins and before bounded session teardown.
const HTTP_GRACEFUL_DRAIN_BUDGET: Duration = Duration::from_secs(3);

/// The fail-closed bind gate (spec D4): refuse to bind a network-exposed address
/// with no token. Returns `Err` (with the operator-facing reason) only for
/// [`Exposure::ExposedNoToken`]; `Ok` for a loopback bind without a token and for
/// any authenticated bind. Pure (no I/O) so this security invariant is unit-
/// testable independently of actually binding a socket; `run` prints the advisory
/// notes for the allowed cases.
///
/// `pub(crate)` so menu-bar mode (`crate::menubar`) applies the identical fail-closed rule
/// before binding its own listener — a `--menubar` serve on an exposed address must honor the
/// same policy as a headless `serve --http`.
pub(crate) fn check_exposure(cfg: &ServeConfig) -> anyhow::Result<()> {
    if let Exposure::ExposedNoToken = cfg.exposure() {
        anyhow::bail!(
            "refusing to bind {} without a token: anyone on the network could drive this \
             machine. Generate one with `glass-mcp gen-token` and pass --token-file/GLASS_TOKEN, \
             or bind a loopback address.",
            cfg.addr
        );
    }
    Ok(())
}

/// CLI entry for `glass-mcp serve ...`. Parses args, applies the fail-closed rule,
/// binds, and serves until EOF/signal.
pub async fn run(
    http: bool,
    addr: Option<String>,
    token_file: Option<String>,
    sink: Option<Box<dyn glass_core::AuditSink>>,
    report: crate::audit::AuditReport,
) -> anyhow::Result<()> {
    // Delegate to the audited resolver (token precedence + exposure rules + its tests stay
    // the single source of truth); just reconstruct its flag form from clap's typed args.
    let mut argv: Vec<String> = Vec::new();
    if http {
        argv.push("--http".into());
    }
    if let Some(a) = addr {
        argv.push("--addr".into());
        argv.push(a);
    }
    if let Some(tf) = token_file {
        argv.push("--token-file".into());
        argv.push(tf);
    }
    let cfg = config::parse_args(&argv, std::env::var("GLASS_TOKEN").ok(), |p| {
        std::fs::read_to_string(p)
    })
    .map_err(|e| anyhow::anyhow!("glass serve: {e}"))?;

    // Fail-closed exposure rule (spec D4) — refuse a network-exposed bind without a token.
    check_exposure(&cfg)?;
    // Advisory notes for the allowed cases (ExposedNoToken is already refused above).
    match cfg.exposure() {
        Exposure::LoopbackOpen => eprintln!(
            "glass: serving on http://{} with NO auth — local only. \
             Bind a non-loopback address only with a token.",
            cfg.addr
        ),
        Exposure::Authenticated => eprintln!(
            "glass: serving on http://{} (bearer-token auth). \
             No TLS — use a trusted LAN or an SSH/Tailscale tunnel for confidentiality.",
            cfg.addr
        ),
        Exposure::ExposedNoToken => {}
    }

    let listener = tokio::net::TcpListener::bind(cfg.addr)
        .await
        .with_context(|| format!("binding {}", cfg.addr))?;
    run_on(listener, cfg, crate::boot(sink), report).await
}

/// Assemble the outer HTTP router: `/healthz` (unauthenticated, loopback-only — see
/// below) merged with the bearer-gated MCP service. `cancel` is threaded into
/// `StreamableHttpServerConfig` so that a caller cancelling the *same* token (e.g.
/// `run_on`'s graceful-shutdown hook) also stops in-flight MCP sessions. Doesn't touch
/// a socket, so this is unit-testable independently of `run_on`.
fn build_router(cfg: &ServeConfig, server: GlassServer, cancel: &CancellationToken) -> Router {
    // `StreamableHttpServerConfig` is `#[non_exhaustive]`, so build it via the builder
    // rather than a struct literal.
    //
    // rmcp's default `allowed_hosts` is loopback-only (`localhost`/`127.0.0.1`/`::1`) — a
    // DNS-rebinding defense for *token-less* browser attacks. When a token is set, that
    // allow-list would wrongly 403 a legitimate LAN client whose `Host` is the bind IP,
    // including a `0.0.0.0` bind whose reachable IP can't be enumerated here. The bearer
    // token is the real access control and a rebinding attacker can't supply it, so the
    // allow-list adds nothing once a token is required. With NO token we keep the default,
    // where DNS-rebinding protection still matters.
    let mut http_cfg =
        StreamableHttpServerConfig::default().with_cancellation_token(cancel.clone());
    if cfg.token.is_some() {
        http_cfg = http_cfg.disable_allowed_hosts();
    }
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(SingleSessionManager::default()),
        http_cfg,
    );

    // The bearer-token layer fronts only the MCP service; `/healthz` (added below,
    // outside the layer) stays reachable without a token — it exposes just the two
    // grant booleans, and setup/onboarding must be able to poll it before any token
    // is available.
    let expected = Arc::new(cfg.token.clone());
    let mut app: Router =
        Router::new()
            .fallback_service(service)
            .layer(axum::middleware::from_fn_with_state(
                expected,
                auth::require_bearer,
            ));

    // Exposed (non-loopback) binds omit the unauthenticated `/healthz` route so it
    // can't leak grant state past the bearer gate; only a loopback bind gets it.
    if cfg.addr.ip().is_loopback() {
        app = app.route(
            "/healthz",
            get(|| async { axum::Json(crate::health::current_health()) }),
        );
    }
    app
}

/// Serve on an already-bound listener (so tests can bind `127.0.0.1:0`).
pub async fn run_on(
    listener: tokio::net::TcpListener,
    cfg: ServeConfig,
    glass: glass_core::Glass,
    report: crate::audit::AuditReport,
) -> anyhow::Result<()> {
    run_on_until(
        listener,
        cfg,
        glass,
        report,
        crate::shutdown::shutdown_signal(),
    )
    .await
}

/// Serve a bound listener until caller-supplied shutdown, then run the bounded teardown that
/// `run_on` triggers from a process signal.
pub async fn run_on_until(
    listener: tokio::net::TcpListener,
    cfg: ServeConfig,
    glass: glass_core::Glass,
    report: crate::audit::AuditReport,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let server = GlassServer::new(glass, report);
    let sessions = server.sessions();
    let cancel = CancellationToken::new();
    let app = build_router(&cfg, server, &cancel);

    let server_cancel = cancel.clone();
    let serving = axum::serve(listener, app).with_graceful_shutdown(async move {
        server_cancel.cancelled().await;
    });
    match run_server_then_teardown(
        async move { serving.await },
        shutdown,
        &cancel,
        HTTP_GRACEFUL_DRAIN_BUDGET,
        crate::shutdown::run_shutdown(sessions, glass_core::TEARDOWN_BUDGET),
    )
    .await
    {
        Ok(result) => result.context("serving MCP over HTTP"),
        Err(()) => Err(anyhow::anyhow!(
            "MCP HTTP graceful drain exceeded {HTTP_GRACEFUL_DRAIN_BUDGET:?}"
        )),
    }
}

/// Complete bounded session teardown even if graceful HTTP drain times out.
async fn run_server_then_teardown<T>(
    server: impl Future<Output = T>,
    shutdown: impl Future<Output = ()>,
    cancel: &CancellationToken,
    drain_budget: Duration,
    teardown: impl Future<Output = ()>,
) -> Result<T, ()> {
    let result = wait_for_server_or_shutdown(server, shutdown, cancel, drain_budget).await;
    teardown.await;
    result
}

/// Serve until exit or shutdown, then cancel MCP transports and apply `drain_budget` to outstanding
/// HTTP work.
async fn wait_for_server_or_shutdown<T>(
    server: impl Future<Output = T>,
    shutdown: impl Future<Output = ()>,
    cancel: &CancellationToken,
    drain_budget: Duration,
) -> Result<T, ()> {
    tokio::pin!(server);
    tokio::pin!(shutdown);
    tokio::select! {
        result = &mut server => Ok(result),
        _ = &mut shutdown => {
            cancel.cancel();
            tokio::time::timeout(drain_budget, &mut server).await.map_err(|_| ())
        }
    }
}

/// `glass-mcp gen-token [--out PATH]`: print a fresh token, or write it to PATH
/// (owner-only perms on Unix). Then exit 0.
pub fn gen_token(out: Option<String>) -> anyhow::Result<()> {
    let token = token::generate_token();
    match out {
        None => println!("{token}"),
        Some(path) => {
            write_token_file(&path, &token)?;
            eprintln!("glass: wrote token to {path}");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn write_token_file(path: &str, token: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    writeln!(f, "{token}")?;
    Ok(())
}

#[cfg(not(unix))]
fn write_token_file(path: &str, token: &str) -> anyhow::Result<()> {
    // Windows: the file inherits the user's profile ACL; document that callers keep
    // it in a per-user directory. (A future task can tighten the ACL explicitly.)
    std::fs::write(path, format!("{token}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(addr: &str, token: Option<&str>) -> ServeConfig {
        ServeConfig {
            addr: addr.parse().unwrap(),
            token: token.map(String::from),
        }
    }

    #[test]
    fn refuses_network_exposed_bind_without_token() {
        // The headline fail-closed invariant: a non-loopback bind with no token
        // must be refused before any socket is bound.
        assert!(check_exposure(&cfg("0.0.0.0:7300", None)).is_err());
        assert!(check_exposure(&cfg("192.168.1.5:7300", None)).is_err());
        assert!(check_exposure(&cfg("[::]:7300", None)).is_err());
    }

    #[test]
    fn allows_loopback_bind_without_token() {
        // Loopback without a token is the SSH-tunnel endpoint — allowed (warned).
        assert!(check_exposure(&cfg("127.0.0.1:7300", None)).is_ok());
        assert!(check_exposure(&cfg("127.0.0.5:7300", None)).is_ok());
        assert!(check_exposure(&cfg("[::1]:7300", None)).is_ok());
    }

    #[test]
    fn allows_network_exposed_bind_with_token() {
        assert!(check_exposure(&cfg("0.0.0.0:7300", Some("s3cret"))).is_ok());
    }

    fn test_server() -> GlassServer {
        GlassServer::new(
            crate::boot(None),
            crate::audit::report_from_config(None, |_| None),
        )
    }

    #[tokio::test]
    async fn healthz_mounted_on_loopback_absent_on_exposed() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let loopback = build_router(
            &cfg("127.0.0.1:7300", None),
            test_server(),
            &CancellationToken::new(),
        );
        let resp = loopback
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let exposed = build_router(
            &cfg("0.0.0.0:7300", None),
            test_server(),
            &CancellationToken::new(),
        );
        let resp = exposed
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::OK,
            "/healthz must not be reachable on a non-loopback bind"
        );
    }

    #[tokio::test]
    async fn run_on_until_cancellation_runs_the_graceful_shutdown_path() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let task = tokio::spawn(run_on_until(
            listener,
            cfg("127.0.0.1:0", Some("token")),
            crate::boot(None),
            crate::audit::report_from_config(None, |_| None),
            async move { shutdown.cancelled().await },
        ));

        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("cancellation must stop the server")
            .expect("server task must not panic");
        assert!(
            result.is_ok(),
            "graceful server shutdown failed: {result:?}"
        );
    }

    #[tokio::test]
    async fn stalled_graceful_drain_cancels_then_times_out() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let cancel = CancellationToken::new();
        let torn_down = Arc::new(AtomicBool::new(false));
        let observed = torn_down.clone();
        let result = run_server_then_teardown(
            std::future::pending::<()>(),
            async {},
            &cancel,
            std::time::Duration::ZERO,
            async move { observed.store(true, Ordering::SeqCst) },
        )
        .await;
        assert!(result.is_err(), "a stalled drain must time out");
        assert!(
            cancel.is_cancelled(),
            "shutdown must cancel active MCP sessions"
        );
        assert!(
            torn_down.load(Ordering::SeqCst),
            "teardown must follow a timed-out drain"
        );
    }
}
