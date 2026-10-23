//! Read-only attribution of platform services retained by the private desktop.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use glass_macos::private_session::{
    ProcessIdentity, idle_system_executable, platform_executable, work,
};
use rustix::fs::{Mode, OFlags, open, openat};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

const ROOT: &str = "/System/Library/LaunchAgents";
const LIMIT: usize = 128 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SystemService {
    pub process: ProcessIdentity,
    pub label: String,
    pub kind: ServiceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum ServiceKind {
    IdleAgent,
    IdleXpc,
    SharedUserXpc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct XpcDefinition {
    label: String,
    program: PathBuf,
    shared_user: bool,
}

impl XpcDefinition {
    fn parse(program: &Path, value: &plist::Dictionary, identifier: &str) -> Option<Self> {
        let macos = program.parent()?;
        let contents = macos.parent()?;
        let bundle = contents.parent()?;
        if !program.starts_with("/System/Library")
            || macos.file_name()? != "MacOS"
            || contents.file_name()? != "Contents"
            || bundle.extension()? != "xpc"
            || value.get("CFBundlePackageType")?.as_string()? != "XPC!"
            || value.get("CFBundleExecutable")?.as_string()? != program.file_name()?.to_str()?
        {
            return None;
        }
        let label = value.get("CFBundleIdentifier")?.as_string()?;
        if !label_valid(label) || label != identifier {
            return None;
        }
        let service = value.get("XPCService")?.as_dictionary()?;
        let shared_user = match service.get("ServiceType") {
            None => false,
            Some(value) => match value.as_string()? {
                "User" => true,
                "Application" => false,
                _ => return None,
            },
        };
        Some(Self {
            label: label.into(),
            program: program.into(),
            shared_user,
        })
    }

    fn kind(&self, idle: bool) -> Option<ServiceKind> {
        if self.shared_user {
            Some(ServiceKind::SharedUserXpc)
        } else if idle {
            Some(ServiceKind::IdleXpc)
        } else {
            None
        }
    }

    fn job_target(&self, uid: u32, process: ProcessIdentity) -> Option<String> {
        let domain = if self.shared_user {
            format!("user/{uid}")
        } else {
            format!(
                "pid/{}",
                glass_macos::private_session::service_requestor(uid, &process)?
            )
        };
        Some(format!("{domain}/{}", self.label))
    }

    fn matches_job(&self, target: &str, process: ProcessIdentity, text: &str) -> Option<u32> {
        let fields = job_fields(text, &format!("{target} = {{"))?;
        let bundle = self.program.parent()?.parent()?.parent()?.to_str()?;
        for (key, expected) in [
            ("path", bundle),
            ("type", "XPCService"),
            ("state", "running"),
            ("bundle id", self.label.as_str()),
            ("program", self.program.to_str()?),
        ] {
            if fields.get(key).copied() != Some(expected) {
                return None;
            }
        }
        if fields.get("pid")?.parse::<u32>().ok()? != process.pid()
            || fields.contains_key("arguments")
        {
            return None;
        }
        let domain = target.rsplit_once('/')?.0;
        let actual = *fields.get("domain")?;
        if if self.shared_user {
            actual != domain
        } else {
            !actual.starts_with(&format!("{domain} [")) || !actual.ends_with(']')
        } {
            return None;
        }
        let properties = fields.get("properties")?;
        if [
            "xpc bundle",
            "system service",
            "supports transactions",
            "supports pressured exit",
        ]
        .iter()
        .any(|needed| !properties.split(" | ").any(|property| property == *needed))
        {
            return None;
        }
        let session = fields.get("asid")?.parse::<u32>().ok()?;
        (session > 0 && session < i32::MAX as u32).then_some(session)
    }
}

fn xpc_candidate(uid: u32, process: ProcessIdentity) -> Option<(SystemService, XpcDefinition)> {
    let origin = work::observe(process.pid(), uid).ok()?;
    if origin.process != process || origin.parent_unique != work::observe(1, 0).ok()?.unique {
        return None;
    }
    let live = platform_executable(uid, &process)?;
    if !live.allows_idle_exit {
        return None;
    }
    let info = live.path.parent()?.parent()?.join("Info.plist");
    let mut source = ProtectedFile::open(&info)?;
    if source.file.metadata().ok()?.len() > LIMIT as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut source.file)
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > LIMIT {
        return None;
    }
    let definition = XpcDefinition::parse(
        &live.path,
        &plist::from_bytes(&bytes).ok()?,
        &live.identifier,
    )?;
    definition.kind(live.idle)?;
    let executable = ProtectedFile::open(&definition.program)?;
    let current = platform_executable(uid, &process)?;
    let kind = definition.kind(current.idle)?;
    let mut current_source = ProtectedFile::open(&info)?;
    let mut current_bytes = Vec::new();
    Read::by_ref(&mut current_source.file)
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut current_bytes)
        .ok()?;
    if current.path != definition.program
        || !current.allows_idle_exit
        || current.identifier != definition.label
        || !source.same(&current_source)
        || bytes != current_bytes
        || !ProtectedFile::open(&current.path).is_some_and(|now| executable.same(&now))
        || work::observe(process.pid(), uid).ok()? != origin
    {
        return None;
    }
    let service = SystemService {
        process,
        label: definition.label.clone(),
        kind,
    };
    Some((service, definition))
}

async fn verified_xpc(uid: u32, process: ProcessIdentity) -> anyhow::Result<Option<SystemService>> {
    let Some((service, definition)) =
        tokio::task::spawn_blocking(move || xpc_candidate(uid, process)).await?
    else {
        return Ok(None);
    };
    let Some(target) = definition.job_target(uid, process) else {
        return Ok(None);
    };
    let Some(job) = loaded_target(&target).await else {
        return Ok(None);
    };
    let Some(session) = definition.matches_job(&target, process, &job) else {
        return Ok(None);
    };
    if !glass_macos::private_session::session_exists(session, uid)? {
        return Ok(None);
    }
    let Some((current, current_definition)) =
        tokio::task::spawn_blocking(move || xpc_candidate(uid, process)).await?
    else {
        return Ok(None);
    };
    Ok((current.kind == service.kind && current_definition == definition).then_some(current))
}

fn within_deadline<T: Send + 'static>(
    deadline: glass_core::Deadline,
    verify: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let remaining = deadline
        .remaining()
        .filter(|left| !left.is_zero())
        .ok_or_else(|| std::io::Error::other("service verification deadline elapsed"))?;
    std::thread::Builder::new()
        .name("glass-service-proof".into())
        .spawn(move || {
            let _ = send.send(verify());
        })?;
    let result = receive
        .recv_timeout(remaining)
        .map_err(|_| std::io::Error::other("service verification deadline elapsed"))??;
    if deadline.has_passed() {
        return Err(std::io::Error::other(
            "service verification deadline elapsed",
        ));
    }
    Ok(result)
}

pub(super) fn validate_report(
    uid: u32,
    processes: Vec<ProcessIdentity>,
    services: Vec<SystemService>,
    deadline: glass_core::Deadline,
) -> std::io::Result<Vec<ProcessIdentity>> {
    within_deadline(deadline, move || {
        let mut retained = Vec::new();
        for service in services {
            if deadline.has_passed()
                || !processes.contains(&service.process)
                || retained.contains(&service.process)
                || !label_valid(&service.label)
            {
                return Err(std::io::Error::other(
                    "macOS system service proof is invalid or late",
                ));
            }
            let valid = match service.kind {
                ServiceKind::IdleAgent => idle_system_executable(uid, &service.process).is_some(),
                _ => xpc_candidate(uid, service.process).is_some_and(|(current, _)| {
                    current.label == service.label && current.kind == service.kind
                }),
            };
            if !valid || deadline.has_passed() {
                return Err(std::io::Error::other(
                    "macOS system service identity changed or deadline elapsed",
                ));
            }
            eprintln!(
                "glass: verified macOS system service {} (PID {}, {:?})",
                service.label,
                service.process.pid(),
                service.kind
            );
            retained.push(service.process);
        }
        if deadline.has_passed() {
            return Err(std::io::Error::other(
                "service verification deadline elapsed",
            ));
        }
        Ok(retained)
    })
}

struct Definition {
    label: String,
    path: PathBuf,
    loaded_path: PathBuf,
    program: PathBuf,
    arguments: Option<Vec<String>>,
    services: Vec<String>,
    pressured_exit: bool,
    source: Option<(ProtectedFile, Vec<u8>)>,
}

struct ProtectedFile {
    file: File,
    parents: Vec<File>,
    directory: bool,
    anchors: Vec<File>,
}

impl ProtectedFile {
    fn open(path: &Path) -> Option<Self> {
        Self::open_kind(path, false)
    }

    fn open_kind(path: &Path, directory: bool) -> Option<Self> {
        if !path.is_absolute() {
            return None;
        }
        let mut current = File::from(
            open(
                "/",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .ok()?,
        );
        let parts: Vec<_> = path.components().skip(1).collect();
        let mut parents = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            let Component::Normal(part) = part else {
                return None;
            };
            let next_directory = index + 1 < parts.len() || directory;
            let flags = OFlags::RDONLY
                | OFlags::NOFOLLOW
                | OFlags::CLOEXEC
                | if next_directory {
                    OFlags::DIRECTORY
                } else {
                    OFlags::empty()
                };
            let next = File::from(openat(&current, *part, flags, Mode::empty()).ok()?);
            if !Self::edge(&current, &next, next_directory) {
                return None;
            }
            parents.push(current);
            current = next;
        }
        if !Self::protected(&current, directory) {
            return None;
        }
        Some(Self {
            file: current,
            parents,
            directory,
            anchors: Vec::new(),
        })
    }

    fn root_owned(file: &File, directory: bool) -> bool {
        file.metadata().is_ok_and(|metadata| {
            metadata.uid() == 0
                && if directory {
                    metadata.is_dir()
                } else {
                    metadata.is_file() && metadata.nlink() == 1
                }
        }) && glass_macos::private_session::require_write_protected(file, 0).is_ok()
    }

    fn protected(file: &File, directory: bool) -> bool {
        Self::root_owned(file, directory)
            && file
                .metadata()
                .is_ok_and(|metadata| metadata.mode() & 0o022 == 0)
    }

    fn edge(parent: &File, child: &File, directory: bool) -> bool {
        Self::root_owned(parent, true)
            && (Self::protected(parent, true)
                || (Self::protected(child, directory)
                    && glass_macos::private_session::system_nounlink(child)))
    }

    fn valid_chain(&self) -> bool {
        Self::protected(&self.file, self.directory)
            && self.parents.iter().enumerate().all(|(index, parent)| {
                let child = self.parents.get(index + 1).unwrap_or(&self.file);
                Self::edge(
                    parent,
                    child,
                    index + 1 < self.parents.len() || self.directory,
                )
            })
            && self
                .anchors
                .iter()
                .all(|anchor| Self::protected(anchor, true))
    }

    fn same(&self, other: &Self) -> bool {
        self.directory == other.directory
            && self.valid_chain()
            && other.valid_chain()
            && self.parents.len() == other.parents.len()
            && self.anchors.len() == other.anchors.len()
            && self
                .parents
                .iter()
                .zip(&other.parents)
                .all(|(before, after)| Self::same_file(before, after))
            && Self::same_file(&self.file, &other.file)
            && self
                .anchors
                .iter()
                .zip(&other.anchors)
                .all(|(before, after)| Self::same_file(before, after))
    }

    fn same_file(before: &File, after: &File) -> bool {
        before
            .metadata()
            .ok()
            .zip(after.metadata().ok())
            .is_some_and(|(before, after)| {
                before.dev() == after.dev() && before.ino() == after.ino()
            })
    }
}

pub(super) fn label_valid(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 255
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

impl Definition {
    fn parse(path: PathBuf, value: plist::Dictionary) -> Option<Self> {
        let label = value.get("Label")?.as_string()?.to_owned();
        let transactions = match value.get("EnableTransactions") {
            Some(value) => value.as_boolean()?,
            None => false,
        };
        let pressured_exit = match value.get("EnablePressuredExit") {
            Some(value) => value.as_boolean()?,
            None => false,
        };
        if !label_valid(&label) || !(transactions || pressured_exit) {
            return None;
        }
        let arguments = match value.get("ProgramArguments") {
            Some(arguments) => {
                let arguments = arguments
                    .as_array()?
                    .iter()
                    .map(|argument| argument.as_string().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()?;
                if arguments
                    .iter()
                    .any(|argument| argument.contains(['\n', '\r', '\0']))
                {
                    return None;
                }
                (!arguments.is_empty()).then_some(arguments)
            }
            None => None,
        };
        let program = match value.get("Program") {
            Some(program) => program.as_string()?,
            None => arguments.as_ref()?.first()?.as_str(),
        };
        if !Path::new(program).is_absolute() || program.contains(['\n', '\r', '\0']) {
            return None;
        }
        let services = match value.get("MachServices") {
            Some(services) => {
                let services = services.as_dictionary()?;
                if services.iter().any(|(name, enabled)| {
                    name.is_empty()
                        || name.contains(['\n', '\r', '\0', '"'])
                        || !(enabled.as_boolean() == Some(true)
                            || enabled.as_dictionary().is_some())
                }) {
                    return None;
                }
                services.keys().cloned().collect()
            }
            None => Vec::new(),
        };
        Some(Self {
            label,
            loaded_path: path.clone(),
            path,
            program: program.into(),
            arguments,
            services,
            pressured_exit,
            source: None,
        })
    }

    fn read(path: PathBuf) -> Option<Self> {
        if path.parent()? != Path::new(ROOT) {
            return None;
        }
        let resolved = path.canonicalize().ok()?;
        let mut source = ProtectedFile::open(&resolved)?;
        if resolved != path {
            let raw = std::fs::symlink_metadata(&path).ok()?;
            let catalog = ProtectedFile::open_kind(Path::new(ROOT), true)?;
            if raw.uid() != 0
                || !raw.is_symlink()
                || !glass_macos::private_session::readonly_filesystem(&catalog.file)
            {
                return None;
            }
            source.anchors.extend(catalog.parents);
            source.anchors.push(catalog.file);
        }
        let mut file = &source.file;
        if file.metadata().ok()?.len() > LIMIT as u64 {
            return None;
        }
        let mut bytes = Vec::new();
        Read::take(&mut file, LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > LIMIT {
            return None;
        }
        let mut definition = Self::parse(path, plist::from_bytes(&bytes).ok()?)?;
        definition.loaded_path = resolved;
        definition.source = Some((source, bytes));
        Some(definition)
    }

    fn unchanged(&self) -> bool {
        let Some((source, bytes)) = &self.source else {
            return false;
        };
        Self::read(self.path.clone())
            .filter(|definition| definition.loaded_path == self.loaded_path)
            .and_then(|definition| definition.source)
            .is_some_and(|(current, current_bytes)| {
                source.same(&current) && bytes == &current_bytes
            })
    }

    fn program_matches(&self, executable: &Path) -> bool {
        self.program
            .canonicalize()
            .ok()
            .zip(executable.canonicalize().ok())
            .is_some_and(|(declared, live)| declared == live)
    }

    fn transaction_idle(&self, code: &glass_macos::private_session::PlatformExecutable) -> bool {
        code.idle && code.allows_idle_exit == self.pressured_exit
    }

    fn matches_job(&self, uid: u32, session: u32, process: ProcessIdentity, text: &str) -> bool {
        let lines: Vec<_> = text.lines().collect();
        let Some(fields) = job_fields(text, &format!("gui/{uid}/{} = {{", self.label)) else {
            return false;
        };
        let expected = [
            ("path", self.loaded_path.to_string_lossy().into_owned()),
            ("type", "LaunchAgent".to_owned()),
            ("state", "running".to_owned()),
            ("program", self.program.to_string_lossy().into_owned()),
            ("pid", process.pid().to_string()),
            ("asid", session.to_string()),
            ("domain", format!("gui/{uid} [{session}]")),
        ];
        if expected
            .iter()
            .any(|(key, value)| fields.get(key).copied() != Some(value.as_str()))
        {
            return false;
        }
        let Some(properties) = fields.get("properties") else {
            return false;
        };
        if ["system service", "supports transactions"]
            .iter()
            .any(|needed| !properties.split(" | ").any(|property| property == *needed))
        {
            return false;
        }
        if properties
            .split(" | ")
            .any(|property| property == "supports pressured exit")
            != self.pressured_exit
        {
            return false;
        }
        match (&self.arguments, block(&lines, "arguments")) {
            (None, None) if !fields.contains_key("arguments") => {}
            (Some(expected), Some(arguments))
                if fields.get("arguments").copied() == Some("{")
                    && arguments
                        .iter()
                        .map(|line| line.strip_prefix("\t\t"))
                        .collect::<Option<Vec<_>>>()
                        == Some(expected.iter().map(String::as_str).collect()) => {}
            _ => return false,
        }
        if self.services.is_empty() {
            return true;
        }
        let Some(endpoints) = block(&lines, "endpoints") else {
            return false;
        };
        self.services
            .iter()
            .all(|name| endpoints.contains(&format!("\t\t\"{name}\" = {{").as_str()))
    }
}

fn job_fields<'a>(text: &'a str, header: &str) -> Option<BTreeMap<&'a str, &'a str>> {
    let lines: Vec<_> = text.lines().collect();
    if lines.first().copied() != Some(header) || lines.last().copied() != Some("}") {
        return None;
    }
    let mut fields = BTreeMap::new();
    for line in &lines[1..lines.len() - 1] {
        if let Some(line) = line
            .strip_prefix('\t')
            .filter(|line| !line.starts_with('\t'))
            && let Some((key, value)) = line.split_once(" = ")
            && fields.insert(key, value).is_some()
        {
            return None;
        }
    }
    Some(fields)
}

fn block<'a>(lines: &'a [&str], name: &str) -> Option<&'a [&'a str]> {
    let start = lines
        .iter()
        .position(|line| *line == format!("\t{name} = {{"))?
        + 1;
    let end = start + lines[start..].iter().position(|line| *line == "\t}")?;
    Some(&lines[start..end])
}

async fn loaded_job(uid: u32, label: &str) -> Option<String> {
    loaded_target(&format!("gui/{uid}/{label}")).await
}

async fn loaded_target(target: &str) -> Option<String> {
    let mut child = tokio::process::Command::new("/bin/launchctl")
        .args(["print", target])
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let result = tokio::time::timeout(Duration::from_secs(1), async {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        if bytes.len() > LIMIT || !child.wait().await.ok()?.success() {
            return None;
        }
        String::from_utf8(bytes).ok()
    })
    .await;
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill().await;
    }
    result.ok().flatten()
}

pub(super) async fn inspect(bytes: &[u8]) -> anyhow::Result<Vec<SystemService>> {
    let processes: Vec<ProcessIdentity> = serde_json::from_slice(bytes)?;
    anyhow::ensure!(processes.len() <= 64, "too many system-service candidates");
    let uid = rustix::process::geteuid().as_raw();
    let session = glass_macos::private_session::session_id()?;
    inspect_for_account(uid, session, processes).await
}

async fn inspect_for_account(
    uid: u32,
    session: u32,
    processes: Vec<ProcessIdentity>,
) -> anyhow::Result<Vec<SystemService>> {
    let entries = tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<PathBuf>> {
        let entries = std::fs::read_dir(ROOT)?
            .take(4097)
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        anyhow::ensure!(
            entries.len() <= 4096,
            "system service catalog exceeds bound"
        );
        Ok(entries)
    })
    .await??;
    let entries = std::sync::Arc::new(entries);
    let mut idle = Vec::new();
    for process in processes {
        if let Some(service) = verified_xpc(uid, process).await? {
            idle.push(service);
            continue;
        }
        let catalog = entries.clone();
        let Some((code, executable, definition)) = tokio::task::spawn_blocking(move || {
            let code = platform_executable(uid, &process).filter(|code| code.idle)?;
            let executable = ProtectedFile::open(&code.path.canonicalize().ok()?)?;
            let mut matching = catalog
                .iter()
                .filter_map(|path| Definition::read(path.clone()))
                .filter(|definition| {
                    definition.program_matches(&code.path) && definition.transaction_idle(&code)
                });
            let definition = matching.next()?;
            if matching.next().is_some() {
                return None;
            }
            Some((code, executable, definition))
        })
        .await?
        else {
            continue;
        };
        let Some(job) = loaded_job(uid, &definition.label).await else {
            continue;
        };
        let service = tokio::task::spawn_blocking(move || {
            (definition.matches_job(uid, session, process, &job)
                && platform_executable(uid, &process).as_ref() == Some(&code)
                && definition.unchanged()
                && definition.program_matches(&code.path)
                && code
                    .path
                    .canonicalize()
                    .ok()
                    .and_then(|path| ProtectedFile::open(&path))
                    .is_some_and(|current| executable.same(&current)))
            .then_some(SystemService {
                process,
                label: definition.label,
                kind: ServiceKind::IdleAgent,
            })
        })
        .await?;
        if let Some(service) = service {
            idle.push(service);
        }
    }
    Ok(idle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xpc_value(service_type: Option<plist::Value>) -> plist::Dictionary {
        let mut service = plist::Dictionary::new();
        if let Some(value) = service_type {
            service.insert("ServiceType".into(), value);
        }
        [
            (
                "CFBundleIdentifier".to_owned(),
                plist::Value::String("com.apple.example".into()),
            ),
            (
                "CFBundlePackageType".to_owned(),
                plist::Value::String("XPC!".into()),
            ),
            (
                "CFBundleExecutable".to_owned(),
                plist::Value::String("Example".into()),
            ),
            ("XPCService".to_owned(), plist::Value::Dictionary(service)),
        ]
        .into_iter()
        .collect()
    }

    const XPC_PROGRAM: &str = "/System/Library/Frameworks/Example.framework/XPCServices/Example.xpc/Contents/MacOS/Example";

    #[test]
    fn signed_identifier_and_live_xpc_job_must_both_match() {
        let value = xpc_value(Some("User".into()));
        assert!(XpcDefinition::parse(Path::new(XPC_PROGRAM), &value, "com.apple.other").is_none());
        let definition =
            XpcDefinition::parse(Path::new(XPC_PROGRAM), &value, "com.apple.example").unwrap();
        let target = "user/502/com.apple.example";
        let bundle = Path::new(XPC_PROGRAM)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let job = format!(
            "{target} = {{\n\tpath = {}\n\ttype = XPCService\n\tstate = running\n\tbundle id = com.apple.example\n\tprogram = {XPC_PROGRAM}\n\tpid = 123\n\tdomain = user/502\n\tasid = 456\n\tproperties = xpc bundle | system service | supports transactions | supports pressured exit\n}}\n",
            bundle.display()
        );
        assert_eq!(definition.matches_job(target, process(), &job), Some(456));
        for (before, after) in [
            ("type = XPCService", "type = LaunchAgent"),
            ("pid = 123", "pid = 124"),
            (
                "bundle id = com.apple.example",
                "bundle id = com.apple.other",
            ),
            ("domain = user/502", "domain = user/503"),
            ("supports pressured exit", "unrelated property"),
            ("\tasid = 456", "\targuments = {\n\t}\n\tasid = 456"),
        ] {
            assert!(
                definition
                    .matches_job(target, process(), &job.replace(before, after))
                    .is_none(),
                "{before}"
            );
        }
        assert!(
            definition
                .matches_job("user/502/user-submitted", process(), &job)
                .is_none()
        );
    }

    #[test]
    fn late_read_only_proof_cannot_extend_the_callers_deadline() {
        let started = std::time::Instant::now();
        let result = within_deadline(glass_core::Deadline::from_millis(10), || {
            std::thread::sleep(Duration::from_millis(200));
            Ok(true)
        });
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(150));
    }

    #[test]
    fn only_shared_user_xpc_can_remain_active_without_becoming_owned_work() {
        for service_type in [None, Some(plist::Value::String("Application".into()))] {
            let definition = XpcDefinition::parse(
                Path::new(XPC_PROGRAM),
                &xpc_value(service_type),
                "com.apple.example",
            )
            .unwrap();
            assert_eq!(definition.kind(true), Some(ServiceKind::IdleXpc));
            assert_eq!(definition.kind(false), None);
        }
        let definition = XpcDefinition::parse(
            Path::new(XPC_PROGRAM),
            &xpc_value(Some("User".into())),
            "com.apple.example",
        )
        .unwrap();
        assert_eq!(definition.kind(false), Some(ServiceKind::SharedUserXpc));
    }

    #[test]
    fn xpc_role_cannot_be_declared_by_an_app_or_unprotected_location() {
        let value = xpc_value(Some("User".into()));
        for program in [
            XPC_PROGRAM.replace("/System/Library/", "/Users/dev/Library/"),
            XPC_PROGRAM.replace("Example.xpc", "Example.app"),
            XPC_PROGRAM.replace("Contents/MacOS", "Other/MacOS"),
        ] {
            assert!(
                XpcDefinition::parse(Path::new(&program), &value, "com.apple.example").is_none()
            );
        }
        for (key, replacement) in [
            ("CFBundlePackageType", plist::Value::String("APPL".into())),
            ("CFBundleExecutable", plist::Value::String("Other".into())),
            (
                "CFBundleIdentifier",
                plist::Value::String("bad\nlabel".into()),
            ),
            ("XPCService", plist::Value::Boolean(true)),
        ] {
            let mut changed = value.clone();
            changed.insert(key.into(), replacement);
            assert!(
                XpcDefinition::parse(Path::new(XPC_PROGRAM), &changed, "com.apple.example")
                    .is_none()
            );
        }
        for service_type in [plist::Value::Boolean(true), "System".into(), "Other".into()] {
            assert!(
                XpcDefinition::parse(
                    Path::new(XPC_PROGRAM),
                    &xpc_value(Some(service_type)),
                    "com.apple.example"
                )
                .is_none()
            );
        }
    }

    #[test]
    fn protected_source_retains_the_system_inode_and_rejects_owned_or_symlink_paths() {
        let system = ProtectedFile::open(Path::new("/bin/ls")).unwrap();
        assert!(system.same(&ProtectedFile::open(Path::new("/bin/ls")).unwrap()));
        let directory = tempfile::tempdir().unwrap();
        let owned = directory.path().join("definition.plist");
        std::fs::write(&owned, b"owned").unwrap();
        assert!(ProtectedFile::open(&owned).is_none());
        let link = directory.path().join("system");
        std::os::unix::fs::symlink("/bin/ls", &link).unwrap();
        assert!(ProtectedFile::open(&link).is_none());
    }

    #[test]
    fn protected_definition_matches_its_resolved_system_executable() {
        let mut definition = definition();
        definition.program = PathBuf::from("/bin/../bin/ls");
        assert!(definition.program_matches(Path::new("/bin/ls")));
        assert!(!definition.program_matches(Path::new("/bin/cat")));
        assert!(ProtectedFile::open(&definition.program.canonicalize().unwrap()).is_some());
    }

    #[test]
    fn loaded_job_must_match_the_resolved_catalog_path() {
        let mut definition = definition();
        definition.loaded_path = PathBuf::from("/System/Volumes/Preboot/example.plist");
        assert!(!definition.matches_job(502, 456, process(), &job()));
        let loaded = job().replace(
            &format!("path = {ROOT}/com.apple.example.plist"),
            "path = /System/Volumes/Preboot/example.plist",
        );
        assert!(definition.matches_job(502, 456, process(), &loaded));
    }

    #[test]
    fn transaction_only_agents_bind_inactivity_and_loaded_properties() {
        for pressured_exit in [false, true] {
            let mut value = definition_value();
            value.insert("EnablePressuredExit".into(), pressured_exit.into());
            value.insert("EnableTransactions".into(), true.into());
            let definition =
                Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), value).unwrap();
            for idle in [false, true] {
                for allows_idle_exit in [false, true] {
                    let code = glass_macos::private_session::PlatformExecutable {
                        path: "/usr/libexec/example".into(),
                        idle,
                        allows_idle_exit,
                        identifier: "com.apple.example".into(),
                    };
                    assert_eq!(
                        definition.transaction_idle(&code),
                        idle && allows_idle_exit == pressured_exit
                    );
                }
            }
            let without_pressure = job().replace("supports pressured exit | ", "");
            assert_eq!(
                definition.matches_job(502, 456, process(), &job()),
                pressured_exit
            );
            assert_eq!(
                definition.matches_job(502, 456, process(), &without_pressure),
                !pressured_exit
            );
            let matching = if pressured_exit {
                job()
            } else {
                without_pressure
            };
            assert!(!definition.matches_job(
                502,
                456,
                process(),
                &matching.replace("supports transactions", "other property")
            ));
        }
    }

    #[test]
    fn agents_require_typed_and_enabled_transaction_tracking() {
        let values: [Option<plist::Value>; 4] = [
            None,
            Some(false.into()),
            Some(true.into()),
            Some("true".into()),
        ];
        for transactions in &values {
            for pressured_exit in &values {
                let mut value = definition_value();
                value.remove("EnablePressuredExit");
                for (key, flag) in [
                    ("EnableTransactions", transactions),
                    ("EnablePressuredExit", pressured_exit),
                ] {
                    if let Some(flag) = flag {
                        value.insert(key.into(), flag.clone());
                    }
                }
                let valid = [transactions, pressured_exit].iter().all(|flag| {
                    flag.as_ref()
                        .is_none_or(|value| value.as_boolean().is_some())
                }) && [transactions, pressured_exit]
                    .iter()
                    .any(|flag| flag.as_ref().and_then(|value| value.as_boolean()) == Some(true));
                assert_eq!(
                    Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), value)
                        .is_some(),
                    valid
                );
            }
        }
    }

    fn definition_value() -> plist::Dictionary {
        plist::from_bytes(
            br#"
            <plist version="1.0"><dict>
            <key>Label</key><string>com.apple.example</string>
            <key>ProgramArguments</key><array><string>/usr/libexec/example</string></array>
            <key>EnablePressuredExit</key><true/>
            <key>MachServices</key><dict><key>com.apple.example</key><true/></dict>
            </dict></plist>"#,
        )
        .unwrap()
    }

    fn definition() -> Definition {
        Definition::parse(
            format!("{ROOT}/com.apple.example.plist").into(),
            definition_value(),
        )
        .unwrap()
    }

    fn job() -> String {
        format!(
            "gui/502/com.apple.example = {{\n\tpath = {ROOT}/com.apple.example.plist\n\ttype = LaunchAgent\n\tstate = running\n\tprogram = /usr/libexec/example\n\targuments = {{\n\t\t/usr/libexec/example\n\t}}\n\tdomain = gui/502 [456]\n\tasid = 456\n\tpid = 123\n\tendpoints = {{\n\t\t\"com.apple.example\" = {{\n\t\t\tactive = 1\n\t\t}}\n\t}}\n\tproperties = supports transactions | supports pressured exit | system service\n}}\n"
        )
    }

    fn process() -> ProcessIdentity {
        serde_json::from_str(r#"{"pid":123,"seconds":1,"microseconds":2}"#).unwrap()
    }

    #[test]
    fn jobs_without_mach_services_still_require_loaded_system_provenance() {
        let job = job().replace(
            "\tendpoints = {\n\t\t\"com.apple.example\" = {\n\t\t\tactive = 1\n\t\t}\n\t}\n",
            "",
        );
        assert!(!definition().matches_job(502, 456, process(), &job));
        for services in [
            None,
            Some(plist::Value::Dictionary(plist::Dictionary::new())),
        ] {
            let mut value = definition_value();
            value.remove("MachServices");
            if let Some(services) = services {
                value.insert("MachServices".into(), services);
            }
            let definition =
                Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), value).unwrap();
            assert!(definition.matches_job(502, 456, process(), &job));
            for (before, after) in [
                (ROOT, "/Users/dev/Library/LaunchAgents"),
                ("pid = 123", "pid = 124"),
                ("asid = 456", "asid = 457"),
                (
                    "program = /usr/libexec/example",
                    "program = /usr/libexec/other",
                ),
                (
                    "\t\t/usr/libexec/example\n",
                    "\t\t/usr/libexec/example\n\t\t--user-work\n",
                ),
                ("supports pressured exit", "other property"),
                ("supports transactions", "other property"),
                ("system service", "user service"),
            ] {
                assert!(
                    !definition.matches_job(502, 456, process(), &job.replace(before, after)),
                    "{before}"
                );
            }
        }
    }

    #[test]
    fn malformed_mach_service_declarations_are_never_treated_as_absent() {
        for services in [
            plist::Value::Boolean(false),
            plist::Value::Array(Vec::new()),
            plist::Value::Dictionary(
                [("com.apple.example".to_owned(), plist::Value::Boolean(false))]
                    .into_iter()
                    .collect(),
            ),
            plist::Value::Dictionary(
                [("bad\nname".to_owned(), plist::Value::Boolean(true))]
                    .into_iter()
                    .collect(),
            ),
        ] {
            let mut value = definition_value();
            value.insert("MachServices".into(), services);
            assert!(
                Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), value)
                    .is_none()
            );
        }
    }

    #[test]
    fn empty_argument_arrays_require_an_explicit_program_and_absent_live_arguments() {
        let mut value = definition_value();
        value.insert("Program".into(), "/usr/libexec/example".into());
        value.insert("ProgramArguments".into(), plist::Value::Array(Vec::new()));
        let definition = Definition::parse(
            format!("{ROOT}/com.apple.example.plist").into(),
            value.clone(),
        )
        .unwrap();
        let without_arguments =
            job().replace("\targuments = {\n\t\t/usr/libexec/example\n\t}\n", "");
        assert!(definition.matches_job(502, 456, process(), &without_arguments));
        assert!(!definition.matches_job(502, 456, process(), &job()));
        for present in ["junk", "{\n\t}"] {
            let job = without_arguments.replace(
                "\tstate = running",
                &format!("\tstate = running\n\targuments = {present}"),
            );
            assert!(!definition.matches_job(502, 456, process(), &job));
        }
        value.remove("Program");
        assert!(
            Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), value).is_none()
        );
    }

    #[test]
    fn program_only_system_job_requires_the_same_absent_argument_list() {
        let mut value: plist::Dictionary = plist::from_bytes(
            br#"<plist version="1.0"><dict>
            <key>Label</key><string>com.apple.example</string>
            <key>Program</key><string>/usr/libexec/example</string>
            <key>EnablePressuredExit</key><true/>
            <key>MachServices</key><dict><key>com.apple.example</key><true/></dict>
            </dict></plist>"#,
        )
        .unwrap();
        let program_only = Definition::parse(
            format!("{ROOT}/com.apple.example.plist").into(),
            value.clone(),
        )
        .unwrap();
        let job_without_arguments =
            job().replace("\targuments = {\n\t\t/usr/libexec/example\n\t}\n", "");
        assert!(program_only.matches_job(502, 456, process(), &job_without_arguments));
        assert!(!program_only.matches_job(502, 456, process(), &job()));
        assert!(!definition().matches_job(502, 456, process(), &job_without_arguments));
        for malformed in ["junk", "{"] {
            let malformed_job = job_without_arguments.replace(
                "\tstate = running",
                &format!("\tstate = running\n\targuments = {malformed}"),
            );
            assert!(!program_only.matches_job(502, 456, process(), &malformed_job));
        }
        for malformed in [
            plist::Value::Boolean(false),
            plist::Value::Array(vec![plist::Value::Integer(1.into())]),
        ] {
            value.insert("ProgramArguments".into(), malformed);
            assert!(
                Definition::parse(
                    format!("{ROOT}/com.apple.example.plist").into(),
                    value.clone()
                )
                .is_none()
            );
        }
        value.remove("ProgramArguments");
        value.remove("Program");
        assert!(
            Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), value).is_none()
        );
    }

    #[test]
    fn loaded_system_job_binds_source_program_arguments_process_and_login() {
        assert!(definition().matches_job(502, 456, process(), &job()));
    }

    #[test]
    fn user_job_wrong_process_or_changed_session_is_not_platform_infrastructure() {
        for (before, after) in [
            (ROOT, "/Users/dev/Library/LaunchAgents"),
            ("pid = 123", "pid = 124"),
            ("asid = 456", "asid = 457"),
            (
                "\t\t/usr/libexec/example\n",
                "\t\t/usr/libexec/example\n\t\t--user-work\n",
            ),
            ("supports pressured exit", "other property"),
            ("system service", "user service"),
            ("\t\t\"com.apple.example\" = {", "\t\t\"other.service\" = {"),
        ] {
            assert!(
                !definition().matches_job(502, 456, process(), &job().replace(before, after)),
                "{before}"
            );
        }
    }

    #[test]
    fn duplicate_or_incomplete_launchd_observations_are_rejected() {
        let duplicate = job().replace("\tpid = 123", "\tpid = 123\n\tpid = 123");
        assert!(!definition().matches_job(502, 456, process(), &duplicate));
        assert!(!definition().matches_job(
            502,
            456,
            process(),
            job().trim_end_matches(['}', '\n'])
        ));
    }
}
