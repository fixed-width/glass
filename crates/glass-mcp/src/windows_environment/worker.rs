use super::files::Root;
use super::runtime::{ExecutionContext, Hello, Journal, Launch, validate_journal};
use crate::backend_worker::{self, Dispatcher, Operation, Request};
use anyhow::{Context, Result, ensure};
use glass_core::{Deadline, Platform, ProtectedHostPath};
use glass_windows::desktop::{LocalPipe, ProcessIdentity};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize)]
struct Ready {
    version: u32,
    instance: String,
    process: ProcessIdentity,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stop {
    version: u32,
    instance: String,
}

fn stop_requested(root: &Root, instance: &str) -> Result<bool> {
    if glass_windows::desktop::stop_requested(instance)? {
        return Ok(true);
    }
    if let Some(stop) = root.read::<Stop>("stop.json")? {
        ensure!(
            stop.version == 1 && stop.instance == instance,
            "desktop stop request has the wrong identity"
        );
        return Ok(true);
    }
    Ok(false)
}

fn apply_launch_context(spec: &mut glass_core::AppSpec, context: &ExecutionContext) {
    spec.cwd = Some(match spec.cwd.take() {
        Some(cwd) => context.cwd.join(cwd),
        None => context.cwd.clone(),
    });
    if !spec
        .env
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("PATH"))
        && let Some(path) = &context.path
    {
        spec.env.push(("PATH".into(), path.clone()));
    }
}

pub fn run(path: &Path, instance: &str) -> Result<()> {
    let owned_path = path.to_path_buf();
    let owned_instance = instance.to_owned();
    let result = std::thread::Builder::new()
        .name("glass-windows-worker".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let _apartment = glass_windows::desktop::RuntimeApartment::initialize()?;
            run_inner(&owned_path, &owned_instance)
        })
        .context("start Windows application worker thread")?
        .join()
        .map_err(|_| anyhow::anyhow!("Windows application worker thread panicked"))?;
    if let Err(error) = &result
        && let Ok(Some(root)) = Root::open(path, false)
        && let Ok(Some(owner)) = root.read::<Journal>("owner.json")
        && let Ok(current) = ProcessIdentity::current()
        && owner.instance == instance
        && owner.child == current.session
        && current.session != owner.parent.session
        && owner.parent.sid == current.sid
    {
        let _ = root.write(
            "worker-error.json",
            &serde_json::json!({"version":1,"instance":instance,"error":format!("{error:#}")}),
        );
    }
    result
}

fn run_inner(path: &Path, instance: &str) -> Result<()> {
    let root = Root::open(path, false)?.context("private Windows worker directory is missing")?;
    let launch = root
        .read::<Launch>("launch.json")?
        .context("worker launch record is missing")?;
    let owner = root
        .read::<Journal>("owner.json")?
        .context("worker owner record is missing")?;
    let current = ProcessIdentity::current()?;
    validate_journal(&owner, &current)?;
    ensure!(
        launch.version == 1
            && launch.instance == instance
            && owner.instance == instance
            && launch.parent == owner.parent
            && launch.helper.as_ref() == Some(&owner.owner),
        "Windows worker launch identity changed"
    );
    ensure!(
        current.session != 0
            && current.session == owner.child
            && current.session != owner.parent.session
            && current.sid == owner.parent.sid
            && current.image == launch.worker_exe,
        "Windows worker is outside its assigned child session"
    );
    ensure!(
        owner.owner.is_current()? && launch.parent.is_current()?,
        "Windows worker owner or parent has exited"
    );
    glass_windows::desktop::probe_graphical_desktop(Deadline::from_millis(10_000))?;
    let work = std::sync::Arc::new(
        super::work::Work::open(&root, instance, true)?.context("container ownership journal")?,
    );
    let mut platform =
        glass_windows::WindowsPlatform::new()?.with_containment_journal(work.clone());
    let protected =
        ProtectedHostPath::directory(path.parent().context("desktop state root has no parent")?);
    platform.configure_protected_host_paths(std::slice::from_ref(&protected))?;
    let mut pipe = LocalPipe::listen(&launch.pipe)?;
    root.write(
        "worker.json",
        &Ready {
            version: 1,
            instance: instance.into(),
            process: current.clone(),
        },
    )?;
    pipe.accept(&launch.parent, Deadline::from_millis(30_000))?;
    let greeting: Hello = serde_json::from_slice(&pipe.read_frame(Deadline::from_millis(10_000))?)?;
    ensure!(
        greeting.version == backend_worker::VERSION
            && greeting.instance == instance
            && greeting.process == launch.parent,
        "Windows worker client greeting does not match its native peer"
    );
    let context = greeting
        .context
        .context("Windows worker client supplied no launch context")?;
    let provider_settings = context.providers.clone();
    let configured = platform.with_provider_settings(provider_settings.clone());
    let mut dispatcher = Dispatcher::new(configured, glass_a11y_windows::WindowsA11y::new());
    ensure!(
        context.cwd.is_absolute(),
        "Windows worker launch directory must be absolute"
    );
    pipe.write_frame(
        &serde_json::to_vec(&Hello {
            version: backend_worker::VERSION,
            instance: instance.into(),
            process: current,
            context: None,
        })?,
        Deadline::from_millis(10_000),
    )?;
    let serving = (|| -> Result<()> {
        let mut idle = true;
        loop {
            if stop_requested(&root, instance)?
                || !owner.owner.is_current()?
                || !launch.parent.is_current()?
            {
                break;
            }
            let bytes = match pipe.read_optional_frame(
                Deadline::from_millis(250),
                std::time::Duration::from_secs(10),
            ) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(error) => return Err(error.into()),
            };
            let mut request: Request = serde_json::from_slice(&bytes)?;
            let is_stop = matches!(&request.operation, Operation::Stop);
            if !is_stop {
                ensure!(
                    !stop_requested(&root, instance)?
                        && owner.owner.is_current()?
                        && launch.parent.is_current()?,
                    "Windows desktop is stopping; request was not dispatched"
                );
            }
            if let Operation::Configure(paths) = &mut request.operation {
                if idle {
                    let mut platform = glass_windows::WindowsPlatform::new()?
                        .with_containment_journal(work.clone())
                        .with_provider_settings(provider_settings.clone());
                    platform.configure_protected_host_paths(std::slice::from_ref(&protected))?;
                    dispatcher = Dispatcher::new(platform, glass_a11y_windows::WindowsA11y::new());
                }
                paths.push(protected.clone());
            }
            if let Operation::Start(spec) = &mut request.operation {
                idle = false;
                apply_launch_context(spec, &context);
            }
            let deadline = backend_worker::deadline(request.expires_ns)?;
            let mut response = dispatcher.handle(request);
            if is_stop && response.result.is_ok() && !work.complete()? {
                response.result = Err(glass_core::GlassError::Backend(
                    "container cleanup is unconfirmed; the environment journal was retained".into(),
                ));
            }
            if is_stop && response.result.is_ok() {
                idle = true;
            }
            pipe.write_frame(
                &serde_json::to_vec(&response)?,
                Deadline::at(
                    std::time::Instant::now()
                        + deadline
                            .remaining()
                            .unwrap_or(std::time::Duration::from_secs(5))
                            .min(std::time::Duration::from_secs(5)),
                ),
            )?;
        }
        Ok(())
    })();
    let cleanup = dispatcher
        .stop(Deadline::from_millis(5_000))
        .and_then(|()| match work.complete() {
            Ok(true) => Ok(()),
            Ok(false) => Err(glass_core::GlassError::Backend(
                "container cleanup remains unconfirmed".into(),
            )),
            Err(error) => Err(glass_core::GlassError::Backend(error.to_string())),
        });
    let properties = dispatcher.current_properties();
    root.write("receipt.json", &serde_json::json!({"version":1,"instance":instance,"clean":cleanup.is_ok(),"app_pid":properties.app_pid,"error":cleanup.as_ref().err().map(ToString::to_string)}))?;
    cleanup?;
    serving
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{AppSpec, SandboxLevel};

    fn spec(cwd: Option<&str>) -> AppSpec {
        AppSpec {
            build: Some("custom-tool build".into()),
            run: vec!["custom-tool".into()],
            cwd: cwd.map(Into::into),
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Default,
            a11y: false,
        }
    }

    fn context() -> ExecutionContext {
        ExecutionContext {
            cwd: r"C:\caller".into(),
            path: Some(r"C:\developer-tools;C:\Windows\System32".into()),
            providers: glass_windows::containment::ProviderSettings {
                provider: "auto".into(),
                directory: "unused".into(),
                clipboard_dll: None,
            },
        }
    }

    #[test]
    fn worker_resolves_default_relative_and_absolute_cwd_before_build_and_launch() {
        let context = context();
        for (supplied, expected) in [
            (None, r"C:\caller"),
            (Some("project"), r"C:\caller\project"),
            (Some(r"D:\other"), r"D:\other"),
        ] {
            let mut spec = spec(supplied);
            apply_launch_context(&mut spec, &context);
            assert_eq!(spec.cwd.as_deref(), Some(Path::new(expected)));
        }
    }

    #[test]
    fn worker_forwards_caller_path_but_preserves_case_insensitive_override() {
        let context = context();
        let mut forwarded = spec(None);
        apply_launch_context(&mut forwarded, &context);
        assert_eq!(
            forwarded.env,
            vec![("PATH".into(), context.path.clone().unwrap())]
        );
        let mut explicit = spec(None);
        explicit.env.push(("Path".into(), "custom".into()));
        apply_launch_context(&mut explicit, &context);
        assert_eq!(explicit.env, vec![("Path".into(), "custom".into())]);
    }
}
