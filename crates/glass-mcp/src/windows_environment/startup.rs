//! Per-user startup registration for the visible resident application.

use super::files::{Root, base_path};
use super::runtime;
use anyhow::{Context, Result, ensure};
use glass_windows::desktop::ProcessIdentity;
use serde::{Deserialize, Serialize};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    version: u32,
    sid: String,
    enabled: bool,
    executable: PathBuf,
    arguments: Vec<String>,
    helpers: Vec<PathBuf>,
    request: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    request: String,
    enabled: bool,
    task: String,
    error: Option<String>,
}

#[derive(Clone)]
pub(super) struct Settings {
    pub addr: std::net::SocketAddr,
    pub token_file: Option<PathBuf>,
    pub needs_token: bool,
}

fn base() -> Result<Root> {
    Root::open(&base_path()?, true)?.context("private Glass settings directory")
}

fn invoke(root: &Root, role: &str, request: &str) -> Result<bool> {
    glass_windows::desktop::protect_stdio_inheritance()?;
    let mut process = Command::new(runtime::helper(root)?)
        .arg(&root.path)
        .arg(role)
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("update Start with Windows")?;
    let identity = ProcessIdentity::read(process.id())?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        if let Some(status) = process.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            identity.terminate(glass_core::Deadline::from_millis(5_000))?;
            anyhow::bail!(
                "Windows startup is not responding. Its saved registration was retained for inspection."
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    let receipt = root
        .read::<Receipt>("startup-receipt.json")?
        .context("Windows startup did not return a result")?;
    ensure!(
        receipt.version == 1
            && receipt.request == request
            && receipt.task.starts_with("Glass.Startup."),
        "Windows startup returned a different request"
    );
    ensure!(
        status.success() && receipt.error.is_none(),
        "Start with Windows: {}",
        receipt
            .error
            .as_deref()
            .unwrap_or("the Windows startup operation failed")
    );
    Ok(receipt.enabled)
}

pub(super) fn enabled() -> Result<bool> {
    let root = base()?;
    let _lock = super::native::lock_named(&root, "startup.lock")?;
    let Some(mut recipe) = root.read::<Recipe>("startup.json")? else {
        return Ok(false);
    };
    let current = ProcessIdentity::current()?;
    ensure!(
        recipe.version == 1 && recipe.sid == current.sid,
        "Startup settings belong to a different Windows user"
    );
    let helper = runtime::helper(&root)?;
    if !recipe.helpers.contains(&helper) {
        recipe.helpers.push(helper);
    }
    ensure!(
        recipe.helpers.len() <= 64,
        "Retained startup versions need inspection"
    );
    recipe.request = format!("{:032x}", rand::random::<u128>());
    root.write("startup.json", &recipe)?;
    invoke(&root, "startup-status", &recipe.request)
}

pub(super) fn configure(wanted: bool, settings: &Settings) -> Result<bool> {
    let root = base()?;
    let _lock = super::native::lock_named(&root, "startup.lock")?;
    let current = ProcessIdentity::current()?;
    ensure!(
        current.session != 0,
        "Change Start with Windows from your signed-in desktop"
    );
    if wanted {
        ensure!(
            settings.addr.port() != 0,
            "Choose a fixed MCP endpoint before enabling Start with Windows"
        );
        ensure!(
            !settings.needs_token || settings.token_file.is_some(),
            "Start with Windows needs a --token-file for authenticated endpoints; an environment-only token is not saved"
        );
    }
    let helper = runtime::helper(&root)?;
    let previous = root.read::<Recipe>("startup.json")?;
    let mut helpers = match previous {
        Some(previous) => {
            ensure!(
                previous.version == 1 && previous.sid == current.sid,
                "Startup settings belong to a different Windows user"
            );
            previous.helpers
        }
        None => Vec::new(),
    };
    if !helpers.contains(&helper) {
        helpers.push(helper);
    }
    ensure!(
        helpers.len() <= 64,
        "Retained startup versions need inspection before changing startup"
    );
    let mut arguments = vec![
        "serve".into(),
        "--tray".into(),
        "--startup".into(),
        "--addr".into(),
        settings.addr.to_string(),
    ];
    if let Some(path) = &settings.token_file {
        arguments.extend(["--token-file".into(), path.to_string_lossy().into_owned()]);
    }
    let recipe = Recipe {
        version: 1,
        sid: current.sid,
        enabled: wanted,
        executable: current.image,
        arguments,
        helpers,
        request: format!("{:032x}", rand::random::<u128>()),
    };
    root.write("startup.json", &recipe)?;
    let actual = invoke(&root, "startup-configure", &recipe.request)?;
    ensure!(
        actual == wanted,
        "Windows startup did not retain the selected preference"
    );
    Ok(actual)
}
