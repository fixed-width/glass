//! owned local connection to a launchd-activated macOS worker.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};

const SHIM_OVERRIDE: &str = "GLASS_CLIP_SHIM_DYLIB";
const PROTOCOL: u32 = 15;
const MAX_FRAME: usize = 256 * 1024;
const CHUNK: usize = 16 * 1024;
const HELLO: u8 = 1;
const ACCEPTED: u8 = 2;
const DATA: u8 = 3;
const CLOSE: u8 = 4;
const EXIT: u8 = 5;
const FAILED: u8 = 6;
const GREETING: u8 = 7;
const ACTIVATE: u8 = 8;
const PROBE: u8 = 9;
const READINESS: u8 = 10;
const OPEN_ONBOARDING: u8 = 11;
#[cfg(target_os = "macos")]
const INSPECT_SERVICES: u8 = 12;
#[cfg(target_os = "macos")]
const SYSTEM_SERVICES: u8 = 13;
#[cfg(target_os = "macos")]
const RECONCILE_OUTPUT: u8 = 14;
#[cfg(target_os = "macos")]
const OUTPUT_RESULT: u8 = 15;
#[cfg(target_os = "macos")]
const TERMINATE_OWNED: u8 = 16;
#[cfg(target_os = "macos")]
const OWNED_TERMINATED: u8 = 17;
const BOOTSTRAP_WAIT: Duration = Duration::from_secs(60);
const SETUP_WAIT: Duration = Duration::from_secs(10);
// App stop plus one bounded output callback drain must fit before forced child termination.
const EXIT_WAIT: Duration = Duration::from_secs(8);

#[cfg(target_os = "macos")]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputRequest {
    expires_ns: Option<u64>,
    nonce: [u8; 16],
    scope: glass_macos::workspace_access::outputs::OutputScope,
}

#[cfg(target_os = "macos")]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputReceipt {
    digest: [u8; 32],
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Bootstrap {
    version: u32,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
}

impl Bootstrap {
    pub fn new() -> anyhow::Result<Self> {
        let env = std::env::vars_os()
            .filter(|(name, _)| name == "PATH" || name == SHIM_OVERRIDE)
            .map(|(name, value)| {
                Ok((
                    name.into_string()
                        .map_err(|_| anyhow::anyhow!("non-UTF-8 configuration name"))?,
                    value
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("non-UTF-8 configuration value"))?,
                ))
            })
            .collect::<anyhow::Result<_>>()?;
        let value = Self {
            version: PROTOCOL,
            cwd: std::env::current_dir()?,
            env,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.version == PROTOCOL, "private worker protocol mismatch");
        ensure!(
            self.cwd.is_absolute(),
            "private worker requires an absolute caller cwd"
        );
        ensure!(
            self.env.len() <= 256,
            "too many private worker configuration entries"
        );
        for (name, value) in &self.env {
            ensure!(
                name == "PATH" || name == SHIM_OVERRIDE,
                "unexpected worker configuration"
            );
            ensure!(
                !name.is_empty() && !name.contains(['=', '\0']) && !value.contains('\0'),
                "invalid worker configuration"
            );
        }
        Ok(())
    }

    fn child_command(&self, binary: &std::path::Path, owner: u32) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.cwd)
            .envs(&self.env)
            .arg("native-backend")
            .arg(owner.to_string());
        command
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Admission {
    version: u32,
    private_attachment_checked: bool,
    #[cfg(target_os = "macos")]
    native_process: Option<glass_macos::private_session::work::ProcessOrigin>,
}

#[cfg(target_os = "macos")]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminationRequest {
    expires_ns: Option<u64>,
    processes: Vec<glass_macos::private_session::work::ProcessOrigin>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Activation {
    version: u32,
    endpoint: PathBuf,
}

#[cfg(target_os = "macos")]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Greeting {
    version: u32,
    worker: u32,
    session: u32,
    process: glass_macos::private_session::ProcessIdentity,
    private_session: bool,
    application_ready: std::result::Result<(), String>,
}

#[cfg(target_os = "macos")]
impl Greeting {
    fn require_application(&self) -> glass_core::Result<()> {
        if !self.private_session {
            return Err(glass_core::GlassError::Backend(
                "switch back to your normal account before starting private application work"
                    .into(),
            )
            .before_dispatch());
        }
        match &self.application_ready {
            Ok(()) => Ok(()),
            Err(reason) => Err(glass_core::GlassError::Backend(reason.clone()).before_dispatch()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    worker_exit_code: Option<i32>,
    forced_termination: bool,
    clean_worker_exit: bool,
    connection_valid: bool,
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    data: &[u8],
) -> anyhow::Result<()> {
    ensure!(
        data.len() <= MAX_FRAME,
        "private worker frame exceeds limit"
    );
    writer.write_u8(kind).await?;
    writer.write_u32(data.len() as u32).await?;
    writer.write_all(data).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<(u8, Vec<u8>)> {
    let kind = reader.read_u8().await?;
    let length = reader.read_u32().await? as usize;
    ensure!(length <= MAX_FRAME, "private worker frame exceeds limit");
    let mut data = vec![0; length];
    reader.read_exact(&mut data).await?;
    Ok((kind, data))
}

async fn send_json<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    value: &impl Serialize,
) -> anyhow::Result<()> {
    write_frame(writer, kind, &serde_json::to_vec(value)?).await
}

async fn finish_child(child: &mut Child) -> anyhow::Result<Receipt> {
    let (status, forced) = match tokio::time::timeout(EXIT_WAIT, child.wait()).await {
        Ok(status) => (status?, false),
        Err(_) => {
            child.start_kill()?;
            let status = tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .context("private worker kill/reap did not complete")??;
            (status, true)
        }
    };
    Ok(Receipt {
        worker_exit_code: status.code(),
        forced_termination: forced,
        clean_worker_exit: status.success() && !forced,
        connection_valid: true,
    })
}

struct BrokerExit<S> {
    receipt: Receipt,
    stream: Option<S>,
}

async fn broker<S>(mut stream: S, mut command: Command) -> anyhow::Result<BrokerExit<S>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            send_json(
                &mut stream,
                FAILED,
                &format!("starting the enrolled native worker: {error}"),
            )
            .await?;
            return Err(error.into());
        }
    };
    let stdin = child.stdin.take().context("worker stdin unavailable")?;
    let mut stdout = child.stdout.take().context("worker stdout unavailable")?;
    #[cfg(target_os = "macos")]
    let owned_root = if glass_macos::private_session::attached() {
        Some(glass_macos::private_session::work::observe(
            child
                .id()
                .context("worker PID unavailable before admission")?,
            rustix::process::geteuid().as_raw(),
        )?)
    } else {
        None
    };
    send_json(
        &mut stream,
        ACCEPTED,
        &Admission {
            version: PROTOCOL,
            private_attachment_checked: true,
            #[cfg(target_os = "macos")]
            native_process: owned_root,
        },
    )
    .await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut inbound = tokio::spawn(async move {
        let mut stdin = stdin;
        let result = async {
            loop {
                match read_frame(&mut reader).await {
                    Ok((DATA, data)) => stdin.write_all(&data).await?,
                    Ok((CLOSE, data)) if data.is_empty() => break,
                    Ok(_) => bail!("invalid private worker request"),
                    Err(error) => return Err(error),
                }
            }
            stdin.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        (reader, result)
    });
    let outbound = tokio::spawn(async move {
        let mut buffer = [0; CHUNK];
        loop {
            let count = stdout.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            write_frame(&mut writer, DATA, &buffer[..count]).await?;
        }
        Ok::<_, anyhow::Error>(writer)
    });
    let (mut receipt, reader, inbound_valid) = tokio::select! {
        biased;
        result = &mut inbound => {
            let (reader, result) = result?;
            (finish_child(&mut child).await?, Some(reader), result.is_ok())
        },
        status = child.wait() => {
            let status = status?;
            (Receipt { worker_exit_code: status.code(), forced_termination: false, clean_worker_exit: status.success(), connection_valid: true }, None, true)
        }
    };
    receipt.connection_valid = inbound_valid;
    inbound.abort();
    if reader.is_none() {
        let _ = inbound.await;
    }
    let mut outbound = outbound;
    let drain = tokio::time::timeout(Duration::from_secs(1), &mut outbound).await;
    let stream = match drain {
        Ok(Ok(Ok(mut writer))) => {
            tokio::time::timeout(
                Duration::from_secs(1),
                send_json(&mut writer, EXIT, &receipt),
            )
            .await??;
            if let Some(reader) = reader.filter(|_| inbound_valid) {
                Some(reader.unsplit(writer))
            } else {
                writer.shutdown().await?;
                None
            }
        }
        Ok(Ok(Err(error))) => return Err(error),
        Ok(Err(error)) => return Err(error.into()),
        Err(_) => {
            outbound.abort();
            let _ = outbound.await;
            bail!("private worker output did not drain after exit");
        }
    };
    Ok(BrokerExit { receipt, stream })
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use tokio::net::UnixStream;

    pub(super) fn protected_endpoint(owner: u32) -> anyhow::Result<()> {
        let root = glass_macos::private_session::root_for(owner);
        for parent in root.ancestors() {
            let metadata = std::fs::symlink_metadata(parent)?;
            ensure!(
                metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
                "private worker setup directory is not protected"
            );
        }
        let metadata = std::fs::symlink_metadata(root.join("worker.sock"))?;
        use std::os::unix::fs::FileTypeExt;
        ensure!(
            metadata.file_type().is_socket()
                && metadata.uid() == owner
                && metadata.mode() & 0o777 == 0o600,
            "private worker socket does not match setup"
        );
        Ok(())
    }

    pub async fn worker(owner: u32) -> anyhow::Result<()> {
        // Setup may inspect this login on the console. Backend activation still requires privacy.
        glass_macos::private_session::attach_for(owner, true)?;
        let listener = glass_macos::worker_socket::activate_listener()?;
        listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        let (stream, _) = tokio::time::timeout(SETUP_WAIT, listener.accept()).await??;
        let raw = stream.into_std()?;
        ensure!(
            glass_macos::worker_socket::peer_uid(&raw)?
                == glass_macos::private_session::owner_uid(),
            "private worker client is not its enrolled owner"
        );
        let mut activation = UnixStream::from_std(raw)?;
        let (kind, bytes) = tokio::time::timeout(SETUP_WAIT, read_frame(&mut activation)).await??;
        let request: Activation = serde_json::from_slice(&bytes)?;
        ensure!(
            kind == ACTIVATE && request.version == PROTOCOL,
            "private activation protocol mismatch"
        );
        let mut stream =
            super::callback::connect(&request.endpoint, glass_macos::private_session::owner_uid())?;
        drop(activation);
        let greeting = Greeting {
            version: PROTOCOL,
            worker: rustix::process::getuid().as_raw(),
            session: glass_macos::private_session::session_id()?,
            process: glass_macos::private_session::current_process()?,
            private_session: glass_macos::private_session::require_private().is_ok(),
            application_ready: glass_macos::private_session::require_application_desktop()
                .map_err(|error| error.to_string()),
        };
        tokio::time::timeout(SETUP_WAIT, send_json(&mut stream, GREETING, &greeting)).await??;
        let (kind, data) = loop {
            let (kind, data) =
                tokio::time::timeout(BOOTSTRAP_WAIT, read_frame(&mut stream)).await??;
            match kind {
                INSPECT_SERVICES => inspect_services(&mut stream, &data).await?,
                TERMINATE_OWNED => terminate_owned(&mut stream, &data).await?,
                RECONCILE_OUTPUT => reconcile_output(&mut stream, &data, owner).await?,
                _ => break (kind, data),
            }
        };
        if kind == PROBE || kind == OPEN_ONBOARDING {
            ensure!(data.is_empty(), "readiness probe must have no payload");
            if kind == OPEN_ONBOARDING
                && let Err(error) = crate::macos_environment::request_onboarding(owner)
            {
                send_json(&mut stream, FAILED, &format!("{error:#}")).await?;
                return Err(error);
            }
            let onboarding_running = match crate::macos_environment::onboarding_running(owner) {
                Ok(running) => running,
                Err(error) => {
                    send_json(&mut stream, FAILED, &format!("{error:#}")).await?;
                    return Err(error);
                }
            };
            let login_setup_pending = match glass_macos::private_session::login_setup_pending() {
                Ok(pending) => pending,
                Err(error) => {
                    send_json(&mut stream, FAILED, &error.to_string()).await?;
                    return Err(error.into());
                }
            };
            let readiness = crate::macos_setup::Readiness {
                private_session: glass_macos::private_session::require_private().is_ok(),
                login_setup_pending,
                accessibility: glass_macos::accessibility_granted(),
                screen_recording: glass_macos::screen_recording_granted(),
                onboarding_running,
            };
            send_json(&mut stream, READINESS, &readiness).await?;
            return Ok(());
        }
        ensure!(kind == HELLO, "private worker bootstrap missing");
        let bootstrap: Bootstrap = serde_json::from_slice(&data)?;
        bootstrap.validate()?;
        if let Err(error) = glass_macos::private_session::require_application_desktop() {
            send_json(&mut stream, FAILED, &error.to_string()).await?;
            return Err(error.into());
        }
        let command = bootstrap.child_command(&std::env::current_exe()?, owner);
        let exit = broker(stream, command).await?;
        if let Some(mut stream) = exit.stream {
            loop {
                match tokio::time::timeout(BOOTSTRAP_WAIT, read_frame(&mut stream)).await? {
                    Ok((INSPECT_SERVICES, data)) => inspect_services(&mut stream, &data).await?,
                    Ok((TERMINATE_OWNED, data)) => terminate_owned(&mut stream, &data).await?,
                    Ok((RECONCILE_OUTPUT, data)) => {
                        reconcile_output(&mut stream, &data, owner).await?
                    }
                    Err(error)
                        if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                            error.kind() == std::io::ErrorKind::UnexpectedEof
                        }) =>
                    {
                        break;
                    }
                    Err(error) => return Err(error),
                    _ => bail!(
                        "only owned-work, service and output recovery is allowed after backend exit"
                    ),
                }
            }
        }
        if exit.receipt.clean_worker_exit && exit.receipt.connection_valid {
            Ok(())
        } else {
            bail!("private worker cleanup is unconfirmed after forced or abnormal exit")
        }
    }

    async fn inspect_services(stream: &mut UnixStream, bytes: &[u8]) -> anyhow::Result<()> {
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            super::system_services::inspect(bytes),
        )
        .await;
        match result {
            Ok(Ok(services)) => send_json(stream, SYSTEM_SERVICES, &services).await,
            _ => send_json(stream, FAILED, &"macOS service attribution was unavailable").await,
        }
    }

    async fn terminate_owned(stream: &mut UnixStream, bytes: &[u8]) -> anyhow::Result<()> {
        let request: TerminationRequest = serde_json::from_slice(bytes)?;
        let deadline = crate::backend_worker::deadline(request.expires_ns)?;
        match glass_macos::private_session::work::terminate_owned(&request.processes, deadline) {
            Ok(()) => send_json(stream, OWNED_TERMINATED, &request.processes).await,
            Err(error) => {
                send_json(
                    stream,
                    FAILED,
                    &format!("stopping recorded private work: {error}"),
                )
                .await
            }
        }
    }

    async fn reconcile_output(
        stream: &mut UnixStream,
        bytes: &[u8],
        owner: u32,
    ) -> anyhow::Result<()> {
        use sha2::Digest;
        let request: OutputRequest = serde_json::from_slice(bytes)?;
        let result = (|| -> anyhow::Result<()> {
            let deadline = crate::backend_worker::deadline(request.expires_ns)?;
            let access = glass_macos::workspace_access::outputs::OutputAccess::new(owner)?;
            access.repair(&request.scope, deadline)?;
            Ok(())
        })();
        send_json(
            stream,
            OUTPUT_RESULT,
            &OutputReceipt {
                digest: sha2::Sha256::digest(bytes).into(),
                error: result
                    .err()
                    .map(|error| format!("reconciling private outputs: {error:#}")),
            },
        )
        .await
    }

    #[cfg(test)]
    mod output_tests {
        use super::*;
        use sha2::Digest;

        #[tokio::test]
        async fn setup_failure_is_digest_bound_and_malformed_requests_terminate() {
            let root = tempfile::tempdir_in("/private/tmp").unwrap();
            let scope = glass_macos::workspace_access::outputs::OutputScope::capture(
                root.path(),
                u32::MAX,
                &[],
            )
            .unwrap();
            let bytes = serde_json::to_vec(&OutputRequest {
                expires_ns: None,
                nonce: [7; 16],
                scope: scope.for_worker(),
            })
            .unwrap();
            let (mut client, mut worker) = UnixStream::pair().unwrap();
            reconcile_output(&mut worker, &bytes, rustix::process::geteuid().as_raw())
                .await
                .unwrap();
            let (kind, response) = read_frame(&mut client).await.unwrap();
            assert_eq!(kind, OUTPUT_RESULT);
            let receipt: OutputReceipt = serde_json::from_slice(&response).unwrap();
            assert_eq!(
                receipt.digest,
                <[u8; 32]>::from(sha2::Sha256::digest(&bytes))
            );
            assert!(receipt.error.is_some());
            assert!(reconcile_output(&mut worker, b"{}", 0).await.is_err());
            drop(worker);
            assert!(read_frame(&mut client).await.is_err());
        }
    }
}

#[cfg(target_os = "macos")]
pub use native::worker;

#[cfg(target_os = "macos")]
pub(crate) mod backend_transport;

#[cfg(target_os = "macos")]
mod system_services;

#[cfg(target_os = "macos")]
pub(crate) fn check_endpoint(owner: u32) -> anyhow::Result<()> {
    native::protected_endpoint(owner)
}

#[cfg(target_os = "macos")]
mod callback;
#[cfg(target_os = "macos")]
pub(crate) use callback::broker_requirement;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn application_admission_requires_positive_desktop_facts() {
        let value = serde_json::json!({
            "version": PROTOCOL, "worker": 505, "session": 11,
            "process": {"pid": 42, "seconds": 1, "microseconds": 0},
            "private_session": true, "application_ready": {"Err": "finish macOS welcome"},
        });
        let mut greeting: Greeting = serde_json::from_value(value.clone()).unwrap();
        let error = greeting.require_application().unwrap_err();
        assert!(error.to_string().contains("finish macOS welcome"));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
        greeting.application_ready = Ok(());
        greeting.require_application().unwrap();
        greeting.private_session = false;
        assert!(greeting.require_application().is_err());
        let mut missing = value;
        missing.as_object_mut().unwrap().remove("application_ready");
        assert!(serde_json::from_value::<Greeting>(missing).is_err());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn recovery_rpc_is_refused_while_a_native_child_is_active() {
        for request in [RECONCILE_OUTPUT, TERMINATE_OWNED] {
            let (mut client, server) = tokio::io::duplex(4096);
            let command = Command::new("/bin/cat");
            let handler = tokio::spawn(broker(server, command));
            assert_eq!(read_frame(&mut client).await.unwrap().0, ACCEPTED);
            write_frame(&mut client, request, b"{}").await.unwrap();
            let (kind, data) = read_frame(&mut client).await.unwrap();
            assert_eq!(kind, EXIT);
            let receipt: Receipt = serde_json::from_slice(&data).unwrap();
            assert!(!receipt.connection_valid);
            assert!(handler.await.unwrap().unwrap().stream.is_none());
        }
    }

    #[test]
    fn bootstrap_does_not_forward_transport_credentials_or_login_identity() {
        let mut bootstrap = Bootstrap {
            version: PROTOCOL,
            cwd: "/project".into(),
            env: BTreeMap::new(),
        };
        for key in ["HOME", "USER", "GLASS_TOKEN", "GLASS_AUDIT_LOG"] {
            bootstrap.env.insert(key.into(), "value".into());
            assert!(bootstrap.validate().is_err());
            bootstrap.env.clear();
        }
        bootstrap
            .env
            .insert("PATH".into(), "/tools:/usr/bin".into());
        assert!(bootstrap.validate().is_ok());
    }

    #[test]
    fn bootstrap_rejects_wrong_protocol_and_relative_cwd() {
        let mut bootstrap = Bootstrap {
            version: PROTOCOL + 1,
            cwd: "/project".into(),
            env: BTreeMap::new(),
        };
        assert!(bootstrap.validate().is_err());
        bootstrap.version = PROTOCOL;
        bootstrap.cwd = "relative".into();
        assert!(bootstrap.validate().is_err());
    }

    #[tokio::test]
    async fn oversized_frame_refuses_before_allocating_its_payload() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer.write_u8(DATA).await.unwrap();
        writer.write_u32(MAX_FRAME as u32 + 1).await.unwrap();
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn clean_backend_exit_retains_a_duplex_inspection_channel_until_the_owner_closes() {
        let (mut client, server) = tokio::io::duplex(4096);
        let mut command = Command::new("/bin/cat");
        command.arg("-u");
        let handler = tokio::spawn(async move {
            let exit = broker(server, command).await.unwrap();
            assert!(exit.receipt.clean_worker_exit && exit.receipt.connection_valid);
            let mut stream = exit.stream.unwrap();
            assert_eq!(
                read_frame(&mut stream).await.unwrap(),
                (INSPECT_SERVICES, b"[]".to_vec())
            );
            write_frame(&mut stream, SYSTEM_SERVICES, b"[]")
                .await
                .unwrap();
            assert!(read_frame(&mut stream).await.is_err());
        });
        assert_eq!(read_frame(&mut client).await.unwrap().0, ACCEPTED);
        write_frame(&mut client, CLOSE, b"").await.unwrap();
        assert_eq!(read_frame(&mut client).await.unwrap().0, EXIT);
        write_frame(&mut client, INSPECT_SERVICES, b"[]")
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut client).await.unwrap(),
            (SYSTEM_SERVICES, b"[]".to_vec())
        );
        drop(client);
        handler.await.unwrap();
    }

    #[tokio::test]
    async fn broker_reports_unclean_close_without_replaying_input() {
        let (mut client, server) = tokio::io::duplex(4096);
        let mut command = Command::new("/bin/cat");
        command.arg("-u");
        let handler = tokio::spawn(broker(server, command));
        assert_eq!(read_frame(&mut client).await.unwrap().0, ACCEPTED);
        write_frame(&mut client, CLOSE, b"invalid payload")
            .await
            .unwrap();
        let (kind, bytes) = read_frame(&mut client).await.unwrap();
        assert_eq!(kind, EXIT);
        let receipt: Receipt = serde_json::from_slice(&bytes).unwrap();
        assert!(receipt.clean_worker_exit);
        assert!(!receipt.connection_valid);
        let exit = handler.await.unwrap().unwrap();
        assert!(!exit.receipt.connection_valid);
        assert!(exit.stream.is_none());
    }
}
