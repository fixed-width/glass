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
const PROTOCOL: u32 = 5;
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
const BOOTSTRAP_WAIT: Duration = Duration::from_secs(60);
const SETUP_WAIT: Duration = Duration::from_secs(10);
const EXIT_WAIT: Duration = Duration::from_secs(4);

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

async fn broker<S>(mut stream: S, mut command: Command) -> anyhow::Result<Receipt>
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
    send_json(
        &mut stream,
        ACCEPTED,
        &Admission {
            version: PROTOCOL,
            private_attachment_checked: true,
        },
    )
    .await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut inbound = tokio::spawn(async move {
        let mut stdin = stdin;
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
    let (mut receipt, inbound_finished, inbound_valid) = tokio::select! {
        result = &mut inbound => (finish_child(&mut child).await?, true, matches!(result, Ok(Ok(())))),
        status = child.wait() => {
            let status = status?;
            (Receipt { worker_exit_code: status.code(), forced_termination: false, clean_worker_exit: status.success(), connection_valid: true }, false, true)
        }
    };
    receipt.connection_valid = inbound_valid;
    inbound.abort();
    if !inbound_finished {
        let _ = inbound.await;
    }
    let mut outbound = outbound;
    let drain = tokio::time::timeout(Duration::from_secs(1), &mut outbound).await;
    match drain {
        Ok(Ok(Ok(mut writer))) => {
            tokio::time::timeout(
                Duration::from_secs(1),
                send_json(&mut writer, EXIT, &receipt),
            )
            .await??;
            writer.shutdown().await?;
        }
        Ok(Ok(Err(error))) => return Err(error),
        Ok(Err(error)) => return Err(error.into()),
        Err(_) => {
            outbound.abort();
            let _ = outbound.await;
            bail!("private worker output did not drain after exit");
        }
    }
    Ok(receipt)
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
        glass_macos::private_session::attach_for(owner, false)?;
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
        };
        tokio::time::timeout(SETUP_WAIT, send_json(&mut stream, GREETING, &greeting)).await??;
        let (kind, data) = tokio::time::timeout(BOOTSTRAP_WAIT, read_frame(&mut stream)).await??;
        ensure!(kind == HELLO, "private worker bootstrap missing");
        let bootstrap: Bootstrap = serde_json::from_slice(&data)?;
        bootstrap.validate()?;
        glass_macos::private_session::require_private()?;
        let command = bootstrap.child_command(&std::env::current_exe()?, owner);
        match broker(stream, command).await {
            Ok(receipt) if receipt.clean_worker_exit && receipt.connection_valid => Ok(()),
            Ok(_) => bail!("private worker cleanup is unconfirmed after forced or abnormal exit"),
            Err(error) => Err(error),
        }
    }
}

#[cfg(target_os = "macos")]
pub use native::worker;

#[cfg(target_os = "macos")]
pub(crate) mod backend_transport;

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
        handler.await.unwrap().unwrap();
    }
}
