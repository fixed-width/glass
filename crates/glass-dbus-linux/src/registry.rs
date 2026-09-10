//! Keep toolkit accessibility active between short-lived reader connections.

use std::sync::mpsc;
use std::time::Duration;

use glass_core::{GlassError, Result};
use tokio::sync::oneshot;

const START_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct RegistryClient {
    // Dropping the sender wakes the worker and closes its registered connection.
    _shutdown: oneshot::Sender<()>,
}

impl RegistryClient {
    pub(crate) fn start(address: &str) -> Result<Self> {
        let address = address.to_owned();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("glass-atspi-registry".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(registry_error(error)));
                        return;
                    }
                };
                runtime.block_on(async {
                    let connection = match tokio::time::timeout(START_TIMEOUT, register(&address))
                        .await
                    {
                        Ok(Ok(connection)) => connection,
                        Ok(Err(error)) => {
                            let _ = ready_tx.send(Err(error));
                            return;
                        }
                        Err(_) => {
                            let _ = ready_tx.send(Err(registry_error("registration timed out")));
                            return;
                        }
                    };
                    if ready_tx.send(Ok(())).is_ok() {
                        let _ = shutdown_rx.await;
                    }
                    drop(connection);
                });
            })
            .map_err(registry_error)?;
        ready_rx
            .recv_timeout(START_TIMEOUT)
            .map_err(registry_error)??;
        Ok(Self {
            _shutdown: shutdown_tx,
        })
    }
}

fn registry_error(error: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(format!(
        "keep private AT-SPI registry client active: {error}"
    ))
}

async fn register(address: &str) -> Result<zbus::Connection> {
    let connection = zbus::connection::Builder::address(address)
        .map_err(registry_error)?
        .build()
        .await
        .map_err(registry_error)?;
    let registry = zbus::Proxy::new(
        &connection,
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/registry",
        "org.a11y.atspi.Registry",
    )
    .await
    .map_err(registry_error)?;
    registry
        .call::<_, _, ()>("RegisterEvent", &("object:",))
        .await
        .map_err(registry_error)?;
    drop(registry);
    Ok(connection)
}
