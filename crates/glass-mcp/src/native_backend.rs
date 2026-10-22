//! private GUI worker at the existing backend interfaces.

use std::io::{BufRead, Write};

use glass_core::{Deadline, GlassError, Result};

use crate::backend_worker::{Dispatcher, Properties, Request, Response, VERSION};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub response: Response,
    pub work: glass_macos::private_session::work::WorkSnapshot,
}

impl Reply {
    fn new(response: Response) -> Result<Self> {
        Ok(Self {
            response,
            work: glass_macos::private_session::work::recorded()
                .map_err(|error| GlassError::Backend(error.to_string()))?,
        })
    }
}

pub fn run(owner: u32) -> anyhow::Result<()> {
    glass_macos::private_session::attach_for(owner, false)?;
    glass_macos::private_session::work::begin()?;
    glass_macos::init_main_thread();
    let (requests, incoming) = std::sync::mpsc::channel::<Request>();
    let (outgoing, responses) = std::sync::mpsc::channel::<Reply>();
    let thread = std::thread::Builder::new()
        .name("glass-platform".into())
        .spawn(move || {
            let platform = glass_macos::MacosPlatform::new();
            let mut dispatcher = match platform {
                Ok(platform) => Dispatcher::new(platform, glass_a11y_macos::MacosA11y::new()),
                Err(error) => {
                    if let Ok(request) = incoming.recv() {
                        let properties = Properties {
                            input_route: glass_core::InputRoute::SharedDesktop,
                            input: glass_core::InputCapabilities::unknown(),
                            app_pid: None,
                            window: None,
                            scope_basis: None,
                            coverage: glass_core::AxStateCoverage::NONE,
                        };
                        let _ = outgoing.send(Reply::new(Response {
                            version: VERSION,
                            id: request.id,
                            properties,
                            result: Err(error),
                        })?);
                    }
                    return Ok::<_, GlassError>(());
                }
            };
            while let Ok(request) = incoming.recv() {
                let response = match glass_macos::private_session::require_private() {
                    Ok(()) => dispatcher.handle(request),
                    Err(error) => Response {
                        version: VERSION,
                        id: request.id,
                        properties: dispatcher.current_properties(),
                        result: Err(GlassError::Backend(error.to_string()).before_dispatch()),
                    },
                };
                if outgoing.send(Reply::new(response)?).is_err() {
                    break;
                }
            }
            dispatcher.stop(Deadline::from_millis(1500))
        })?;
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut bytes = Vec::new();
    let result = (|| {
        loop {
            bytes.clear();
            if input.read_until(b'\n', &mut bytes)? == 0 {
                break;
            }
            let request: Request = serde_json::from_slice(&bytes)?;
            requests.send(request)?;
            let response = responses.recv()?;
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        Ok::<_, anyhow::Error>(())
    })();
    drop(requests);
    let stopped: Result<()> = thread
        .join()
        .map_err(|_| anyhow::anyhow!("native backend thread panicked"))?;
    stopped?;
    result
}
