//! Read-only attribution of platform services retained by the private desktop.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use glass_macos::private_session::{
    ProcessIdentity, idle_system_executable, platform_executable, work,
};
use rustix::fs::{Mode, OFlags, open, openat};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

const ROOT: &str = "/System/Library/LaunchAgents";
const SYSTEM_LIBRARY: &str = "/System/Library";
const CRYPTEX_LIBRARY: &str = "/System/Cryptexes/OS/System/Library";
const LIMIT: usize = 128 * 1024;
const AGENT_CANDIDATE_LIMIT: usize = 16;

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
    SharedUserAgent,
    IdleXpc,
    SharedUserXpc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct XpcDefinition {
    label: String,
    program: PathBuf,
    shared_user: bool,
    multiple_instances: bool,
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
        {
            return None;
        }
        let executable = match value.get("CFBundleExecutable") {
            Some(value) => value.as_string()?,
            None => bundle.file_stem()?.to_str()?,
        };
        if executable != program.file_name()?.to_str()? {
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
        let multiple_instances = match service.get("_MultipleInstances") {
            None => false,
            Some(value) => value.as_boolean()?,
        };
        if shared_user && multiple_instances {
            return None;
        }
        Some(Self {
            label: label.into(),
            program: program.into(),
            shared_user,
            multiple_instances,
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

    fn matches_job(
        &self,
        target: &str,
        process: ProcessIdentity,
        expected_session: u32,
        text: &str,
    ) -> Option<u32> {
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
        let session = fields.get("asid")?.parse::<u32>().ok()?;
        if session == 0 || session >= i32::MAX as u32 {
            return None;
        }
        if if self.shared_user {
            actual != domain
        } else {
            session != expected_session || !pid_domain_matches(actual, domain)
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
        Some(session)
    }
}

fn pid_domain_matches(actual: &str, expected: &str) -> bool {
    // launchctl annotates PID domains with a name; the separate asid field binds the login.
    actual
        .strip_prefix(&format!("{expected} ["))
        .and_then(|suffix| suffix.strip_suffix(']'))
        .is_some_and(|name| {
            !name.is_empty()
                && name.len() <= 255
                && name.trim() == name
                && !name
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '[' | ']'))
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum XpcScope {
    User(u32),
    Process(work::ProcessOrigin),
}

impl XpcScope {
    fn observe(uid: u32, process: ProcessIdentity, shared_user: bool) -> Option<Self> {
        if shared_user {
            return Some(Self::User(uid));
        }
        let pid = glass_macos::private_session::service_requestor(uid, &process)?;
        let origin = work::observe(pid, uid).ok()?;
        (glass_macos::private_session::service_requestor(uid, &process)? == pid)
            .then_some(Self::Process(origin))
    }

    fn domain(self) -> String {
        match self {
            Self::User(uid) => format!("user/{uid}"),
            Self::Process(origin) => format!("pid/{}", origin.process.pid()),
        }
    }
}

fn instance_label<'a>(
    text: &'a str,
    domain: &str,
    process: ProcessIdentity,
    base: &str,
) -> Option<&'a str> {
    if text.len() > LIMIT || text.contains('\0') {
        return None;
    }
    let mut lines = text.lines();
    if lines.next()? != format!("{domain} = {{") {
        return None;
    }
    let mut stack = vec![domain];
    let mut services = false;
    let mut found = None;
    for line in lines {
        let line = line.trim();
        if stack.is_empty() {
            return None;
        }
        if matches!(line, "}" | "};") {
            stack.pop()?;
            continue;
        }
        if let Some(name) = line
            .strip_suffix(" = {")
            .or_else(|| line.strip_suffix(" => {"))
            .or_else(|| line.strip_suffix(": {"))
        {
            if stack.len() >= 32 || (stack.len() == 2 && stack[1] == "services") {
                return None;
            }
            if stack.len() == 1 && name == "services" {
                if services {
                    return None;
                }
                services = true;
            }
            stack.push(name);
            continue;
        }
        if stack.len() != 2 || stack[1] != "services" {
            continue;
        }
        let mut columns = line.split_whitespace();
        let Some(pid) = columns.next() else {
            continue;
        };
        if pid.parse::<u32>().ok() != Some(process.pid()) {
            continue;
        }
        let second = columns.next()?;
        let label = match columns.next() {
            None => second,
            Some(label)
                if columns.next().is_none()
                    && (second == "=" || second == "-" || second.parse::<i32>().is_ok()) =>
            {
                label
            }
            _ => return None,
        };
        if found.is_some()
            || !label_valid(label)
            || !label.strip_prefix(base)?.starts_with('.')
            || label.len() <= base.len() + 1
        {
            return None;
        }
        found = Some(label);
    }
    (stack.is_empty() && services).then_some(found).flatten()
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
    let program = system_program(&live.path)?;
    let info = program.parent()?.parent()?.join("Info.plist");
    let mut source = ProtectedFile::system(&info, false)?;
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
    let definition =
        XpcDefinition::parse(&program, &plist::from_bytes(&bytes).ok()?, &live.identifier)?;
    definition.kind(live.idle)?;
    let executable = ProtectedFile::system(&definition.program, false)?;
    let current = platform_executable(uid, &process)?;
    let kind = definition.kind(current.idle)?;
    let mut current_source = ProtectedFile::system(&info, false)?;
    let mut current_bytes = Vec::new();
    Read::by_ref(&mut current_source.file)
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut current_bytes)
        .ok()?;
    if current.path != live.path
        || !current.allows_idle_exit
        || current.identifier != definition.label
        || !source.same(&current_source)
        || bytes != current_bytes
        || !ProtectedFile::system(&definition.program, false)
            .is_some_and(|now| executable.same(&now))
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

async fn verified_xpc(
    uid: u32,
    expected_session: u32,
    process: ProcessIdentity,
) -> anyhow::Result<Option<SystemService>> {
    let Some((service, definition)) =
        tokio::task::spawn_blocking(move || xpc_candidate(uid, process)).await?
    else {
        return Ok(None);
    };
    let Some(scope) = XpcScope::observe(uid, process, definition.shared_user) else {
        return Ok(None);
    };
    let domain = scope.domain();
    let label = if definition.multiple_instances {
        let Some(listing) = loaded_target(&domain).await else {
            return Ok(None);
        };
        let Some(label) = instance_label(&listing, &domain, process, &definition.label) else {
            return Ok(None);
        };
        label.to_owned()
    } else {
        definition.label.clone()
    };
    let target = format!("{domain}/{label}");
    let Some(job) = loaded_target(&target).await else {
        return Ok(None);
    };
    let Some(session) = definition.matches_job(&target, process, expected_session, &job) else {
        return Ok(None);
    };
    if !glass_macos::private_session::session_exists(session, uid)? {
        return Ok(None);
    }
    let Some((current, current_definition, current_scope)) =
        tokio::task::spawn_blocking(move || {
            let (current, definition) = xpc_candidate(uid, process)?;
            let scope = XpcScope::observe(uid, process, definition.shared_user)?;
            Some((current, definition, scope))
        })
        .await?
    else {
        return Ok(None);
    };
    Ok((current.kind == service.kind
        && current_definition == definition
        && current_scope == scope
        && glass_macos::private_session::session_exists(session, uid)?)
    .then_some(current))
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
        let catalog = services
            .iter()
            .any(|service| service.kind == ServiceKind::SharedUserAgent)
            .then(system_catalog)
            .transpose()?;
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
                ServiceKind::SharedUserAgent => catalog.as_ref().is_some_and(|catalog| {
                    agent_candidate(uid, service.process, catalog, &service.label).is_some_and(
                        |candidate| {
                            candidate.kind == ServiceKind::SharedUserAgent
                                && candidate.definition.label == service.label
                                && candidate.still_valid(uid, service.process)
                        },
                    )
                }),
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
    shared_mach_services: bool,
    transactions: bool,
    persistent_login: bool,
    pressured_exit: bool,
    multiple_instances: bool,
    user_domain: bool,
    source: Option<(ProtectedFile, Vec<u8>)>,
}

struct ProtectedFile {
    file: File,
    parents: Vec<File>,
    directory: bool,
    anchors: Vec<File>,
    resolved: PathBuf,
    aliases: Vec<ProtectedAlias>,
}

struct ProtectedAlias {
    path: PathBuf,
    parent: ProtectedFile,
    device: u64,
    inode: u64,
    target: PathBuf,
}

impl ProtectedAlias {
    fn valid(&self) -> bool {
        self.parent.valid_chain()
            && std::fs::symlink_metadata(&self.path).is_ok_and(|info| {
                info.is_symlink()
                    && info.uid() == 0
                    && info.dev() == self.device
                    && info.ino() == self.inode
            })
            && std::fs::read_link(&self.path).is_ok_and(|target| target == self.target)
    }

    fn same(&self, other: &Self) -> bool {
        self.path == other.path
            && self.device == other.device
            && self.inode == other.inode
            && self.target == other.target
            && self.parent.same(&other.parent)
    }
}

fn system_program(path: &Path) -> Option<PathBuf> {
    if path.starts_with(SYSTEM_LIBRARY) {
        return Some(path.into());
    }
    let root = ProtectedFile::system(Path::new(CRYPTEX_LIBRARY), true)?;
    let logical = Path::new(SYSTEM_LIBRARY).join(path.strip_prefix(&root.resolved).ok()?);
    let executable = ProtectedFile::system(&logical, false)?;
    (executable.resolved == path && root.valid_chain() && executable.valid_chain())
        .then_some(logical)
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
            resolved: path.into(),
            aliases: Vec::new(),
        })
    }

    fn system(path: &Path, directory: bool) -> Option<Self> {
        if !(path.starts_with(SYSTEM_LIBRARY) || path.starts_with(CRYPTEX_LIBRARY))
            || path
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return None;
        }
        let canonical = path.canonicalize().ok()?;
        let mut file = Self::open_kind(&canonical, directory)?;
        if canonical == path {
            return Some(file);
        }
        let mut pending: VecDeque<_> = path
            .components()
            .map(|part| part.as_os_str().to_owned())
            .collect();
        let mut resolved = PathBuf::from("/");
        let mut steps = 0;
        while let Some(part) = pending.pop_front() {
            steps += 1;
            if steps > 1024 {
                return None;
            }
            if part == "/" {
                resolved = PathBuf::from("/");
            } else if part == ".." {
                if !resolved.pop() {
                    return None;
                }
            } else if part != "." {
                let next = resolved.join(part);
                let info = std::fs::symlink_metadata(&next).ok()?;
                if info.is_symlink() {
                    if info.uid() != 0 || file.aliases.len() >= 40 {
                        return None;
                    }
                    let parent = Self::open_kind(&resolved, true)?;
                    let target = std::fs::read_link(&next).ok()?;
                    for part in target.components().rev() {
                        pending.push_front(part.as_os_str().to_owned());
                    }
                    file.aliases.push(ProtectedAlias {
                        path: next,
                        parent,
                        device: info.dev(),
                        inode: info.ino(),
                        target,
                    });
                } else {
                    resolved = next;
                }
            }
        }
        (resolved == canonical && file.valid_chain()).then_some(file)
    }

    fn root_owned(file: &File, directory: bool) -> bool {
        file.metadata().is_ok_and(|metadata| {
            metadata.uid() == 0
                && if directory {
                    metadata.is_dir()
                } else {
                    metadata.is_file()
                        && metadata.nlink() > 0
                        && (metadata.nlink() == 1
                            || glass_macos::private_session::readonly_filesystem(file))
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
            && self.aliases.iter().all(ProtectedAlias::valid)
    }

    fn same(&self, other: &Self) -> bool {
        self.directory == other.directory
            && self.valid_chain()
            && other.valid_chain()
            && self.parents.len() == other.parents.len()
            && self.anchors.len() == other.anchors.len()
            && self.aliases.len() == other.aliases.len()
            && self
                .aliases
                .iter()
                .zip(&other.aliases)
                .all(|(before, after)| before.same(after))
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
        let multiple_instances = match value.get("MultipleInstances") {
            Some(value) => value.as_boolean()?,
            None => false,
        };
        let user_domain = match value.get("LimitLoadToSessionType") {
            Some(plist::Value::String(session)) => session == "Background",
            Some(plist::Value::Array(sessions)) => {
                let sessions = sessions
                    .iter()
                    .map(plist::Value::as_string)
                    .collect::<Option<Vec<_>>>()?;
                sessions.as_slice() == ["Background"]
            }
            Some(_) => return None,
            None => false,
        };
        let persistent_login = value
            .get("EnableTransactions")
            .and_then(plist::Value::as_boolean)
            == Some(false)
            && value.get("KeepAlive").and_then(plist::Value::as_boolean) == Some(true)
            && value.get("RunAtLoad").and_then(plist::Value::as_boolean) == Some(true)
            && value
                .get("LimitLoadToSessionType")
                .and_then(plist::Value::as_string)
                == Some("Aqua")
            && !pressured_exit
            && !multiple_instances;
        if !label_valid(&label) || !(transactions || pressured_exit || persistent_login) {
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
        let mut shared_mach_services = false;
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
                shared_mach_services = !services.is_empty()
                    && services
                        .iter()
                        .all(|(name, value)| label_valid(name) && value.as_boolean() == Some(true));
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
            shared_mach_services,
            transactions,
            persistent_login,
            pressured_exit,
            multiple_instances,
            user_domain,
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
        code.transaction_tracked && code.idle && code.allows_idle_exit == self.pressured_exit
    }

    fn service_kind(
        &self,
        code: &glass_macos::private_session::PlatformExecutable,
    ) -> Option<ServiceKind> {
        if self.shared_mach_services
            && !self.services.is_empty()
            && ((self.pressured_exit && code.transaction_tracked && code.allows_idle_exit)
                || (self.persistent_login
                    && !code.transaction_tracked
                    && !code.idle
                    && !code.allows_idle_exit))
        {
            Some(ServiceKind::SharedUserAgent)
        } else if self.transaction_idle(code) {
            Some(ServiceKind::IdleAgent)
        } else {
            None
        }
    }

    fn valid_shared_endpoints(&self, text: &str) -> bool {
        if self.services.is_empty() {
            return false;
        }
        let lines: Vec<_> = text.lines().collect();
        let Some(endpoints) = block(&lines, self.endpoint_block_name()) else {
            return false;
        };
        let mut any_active = false;
        let valid = self.services.iter().all(|name| {
            let header = format!("\t\t\"{name}\" = {{");
            let mut starts = endpoints
                .iter()
                .enumerate()
                .filter(|(_, line)| **line == header);
            let Some((start, _)) = starts.next() else {
                return false;
            };
            if starts.next().is_some() {
                return false;
            }
            let start = start + 1;
            let Some(end) = endpoints[start..].iter().position(|line| *line == "\t\t}") else {
                return false;
            };
            let mut fields = BTreeMap::new();
            for line in &endpoints[start..start + end] {
                let Some((key, value)) = line
                    .strip_prefix("\t\t\t")
                    .filter(|line| !line.starts_with('\t'))
                    .and_then(|line| line.split_once(" = "))
                else {
                    return false;
                };
                if fields.insert(key, value).is_some() {
                    return false;
                }
            }
            match fields.get("active") {
                Some(&"1") => any_active = true,
                Some(&"0") if self.pressured_exit => {}
                _ => return false,
            }
            fields.get("managed") == Some(&"1")
                && fields.get("port").is_some_and(|value| {
                    value
                        .strip_prefix("0x")
                        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                        .is_some_and(|port| port != 0)
                })
        });
        valid && any_active
    }

    fn kind_for_job(
        &self,
        code: &glass_macos::private_session::PlatformExecutable,
        text: &str,
    ) -> Option<ServiceKind> {
        if self.service_kind(code) == Some(ServiceKind::SharedUserAgent)
            && self.valid_shared_endpoints(text)
        {
            Some(ServiceKind::SharedUserAgent)
        } else if self.transaction_idle(code) {
            Some(ServiceKind::IdleAgent)
        } else {
            None
        }
    }

    fn domain(&self, uid: u32) -> String {
        format!("{}/{uid}", if self.user_domain { "user" } else { "gui" })
    }

    fn endpoint_block_name(&self) -> &'static str {
        if self.multiple_instances {
            "instance-specific endpoints"
        } else {
            "endpoints"
        }
    }

    fn verified_job_session(
        &self,
        uid: u32,
        session: u32,
        process: ProcessIdentity,
        text: &str,
    ) -> Option<u32> {
        let lines: Vec<_> = text.lines().collect();
        let domain = self.domain(uid);
        let label = lines
            .first()?
            .strip_suffix(" = {")?
            .strip_prefix(&format!("{domain}/"))?;
        if self.multiple_instances {
            if !label_valid(label)
                || !label.strip_prefix(&self.label)?.starts_with('.')
                || label.len() <= self.label.len() + 1
            {
                return None;
            }
        } else if label != self.label {
            return None;
        }
        let fields = job_fields(text, &format!("{domain}/{label} = {{"))?;
        let observed_session = fields.get("asid")?.parse::<u32>().ok()?;
        if observed_session == 0
            || observed_session >= i32::MAX as u32
            || (!self.user_domain && observed_session != session)
        {
            return None;
        }
        if self.multiple_instances && fields.get("original").copied() != Some(self.label.as_str()) {
            return None;
        }
        let expected = [
            ("path", self.loaded_path.to_string_lossy().into_owned()),
            ("type", "LaunchAgent".to_owned()),
            ("state", "running".to_owned()),
            ("program", self.program.to_string_lossy().into_owned()),
            ("pid", process.pid().to_string()),
            ("asid", observed_session.to_string()),
            (
                "domain",
                if self.user_domain {
                    domain
                } else {
                    format!("{domain} [{session}]")
                },
            ),
        ];
        if expected
            .iter()
            .any(|(key, value)| fields.get(key).copied() != Some(value.as_str()))
        {
            return None;
        }
        let properties = fields.get("properties")?;
        if self.multiple_instances && !properties.split(" | ").any(|value| value == "is copy") {
            return None;
        }
        if !properties
            .split(" | ")
            .any(|property| property == "system service")
            || properties
                .split(" | ")
                .any(|property| property == "supports transactions")
                != (self.transactions || self.pressured_exit)
            || (self.persistent_login
                && ["keepalive", "runatload"]
                    .iter()
                    .any(|needed| !properties.split(" | ").any(|property| property == *needed)))
        {
            return None;
        }
        if properties
            .split(" | ")
            .any(|property| property == "supports pressured exit")
            != self.pressured_exit
        {
            return None;
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
            _ => return None,
        }
        if self.services.is_empty() {
            return Some(observed_session);
        }
        let endpoints = block(&lines, self.endpoint_block_name())?;
        self.services
            .iter()
            .all(|name| endpoints.contains(&format!("\t\t\"{name}\" = {{").as_str()))
            .then_some(observed_session)
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

struct AgentHint {
    source: PathBuf,
    program: PathBuf,
}

impl AgentHint {
    fn read_for(&self, executable: &Path) -> Option<Definition> {
        if self.program != executable {
            return None;
        }
        let definition = Definition::read(self.source.clone())?;
        definition.program_matches(executable).then_some(definition)
    }
}

fn system_catalog() -> std::io::Result<Vec<AgentHint>> {
    let entries = std::fs::read_dir(ROOT)?
        .take(4097)
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    if entries.len() > 4096 {
        return Err(std::io::Error::other(
            "system service catalog exceeds bound",
        ));
    }
    Ok(entries
        .into_iter()
        .filter_map(|path| {
            let definition = Definition::read(path)?;
            let program = definition.program.canonicalize().ok()?;
            // Release discovery descriptors; hints never carry proof across processes.
            Some(AgentHint {
                source: definition.path,
                program,
            })
        })
        .collect())
}

struct AgentCandidate {
    code: Arc<glass_macos::private_session::PlatformExecutable>,
    executable: Arc<ProtectedFile>,
    definition: Definition,
    kind: ServiceKind,
    origin: work::ProcessOrigin,
}

impl AgentCandidate {
    fn still_valid(&self, uid: u32, process: ProcessIdentity) -> bool {
        let Some(current) = platform_executable(uid, &process) else {
            return false;
        };
        current.path == self.code.path
            && current.identifier == self.code.identifier
            && match self.kind {
                ServiceKind::IdleAgent => self.definition.transaction_idle(&current),
                ServiceKind::SharedUserAgent => {
                    self.definition.service_kind(&current) == Some(self.kind)
                }
                _ => false,
            }
            && self.definition.unchanged()
            && self.definition.program_matches(&current.path)
            && current
                .path
                .canonicalize()
                .ok()
                .and_then(|path| ProtectedFile::open(&path))
                .is_some_and(|file| self.executable.same(&file))
            && work::observe(process.pid(), uid).ok() == Some(self.origin)
    }
}

fn agent_candidate(
    uid: u32,
    process: ProcessIdentity,
    catalog: &[AgentHint],
    label: &str,
) -> Option<AgentCandidate> {
    unique_match(
        agent_candidates(uid, process, catalog)?
            .into_iter()
            .filter(|candidate| candidate.definition.label == label),
    )
}

fn unique_match<T>(values: impl IntoIterator<Item = T>) -> Option<T> {
    let mut values = values.into_iter();
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn agent_candidates(
    uid: u32,
    process: ProcessIdentity,
    catalog: &[AgentHint],
) -> Option<Vec<AgentCandidate>> {
    let origin = work::observe(process.pid(), uid).ok()?;
    if origin.process != process || origin.parent_unique != work::observe(1, 0).ok()?.unique {
        return None;
    }
    let code = Arc::new(platform_executable(uid, &process)?);
    let canonical = code.path.canonicalize().ok()?;
    let executable = Arc::new(ProtectedFile::open(&canonical)?);
    let matching = catalog
        .iter()
        .filter_map(|hint| hint.read_for(&canonical))
        .filter(|definition| {
            definition.program_matches(&code.path) && definition.service_kind(&code).is_some()
        });
    let candidates: Vec<_> = matching
        .filter_map(|definition| {
            let kind = definition.service_kind(&code)?;
            Some(AgentCandidate {
                code: code.clone(),
                executable: executable.clone(),
                definition,
                kind,
                origin,
            })
        })
        .take(AGENT_CANDIDATE_LIMIT + 1)
        .collect();
    (candidates.len() <= AGENT_CANDIDATE_LIMIT).then_some(candidates)
}

async fn inspect_for_account(
    uid: u32,
    session: u32,
    processes: Vec<ProcessIdentity>,
) -> anyhow::Result<Vec<SystemService>> {
    let entries = tokio::task::spawn_blocking(system_catalog).await??;
    let entries = Arc::new(entries);
    let mut verified = Vec::new();
    for process in processes {
        if let Some(service) = verified_xpc(uid, session, process).await? {
            verified.push(service);
            continue;
        }
        let catalog = entries.clone();
        let Some(candidates) =
            tokio::task::spawn_blocking(move || agent_candidates(uid, process, &catalog)).await?
        else {
            continue;
        };
        let mut matches = Vec::new();
        for mut candidate in candidates {
            let domain = candidate.definition.domain(uid);
            let label = if candidate.definition.multiple_instances {
                let Some(listing) = loaded_target(&domain).await else {
                    continue;
                };
                let Some(label) =
                    instance_label(&listing, &domain, process, &candidate.definition.label)
                else {
                    continue;
                };
                label.to_owned()
            } else {
                candidate.definition.label.clone()
            };
            let Some(job) = loaded_target(&format!("{domain}/{label}")).await else {
                continue;
            };
            let Some(job_session) = candidate
                .definition
                .verified_job_session(uid, session, process, &job)
            else {
                continue;
            };
            let Some(kind) = candidate.definition.kind_for_job(&candidate.code, &job) else {
                continue;
            };
            candidate.kind = kind;
            let service = tokio::task::spawn_blocking(move || {
                let definition = &candidate.definition;
                ((candidate.kind != ServiceKind::SharedUserAgent
                    || definition.valid_shared_endpoints(&job))
                    && candidate.still_valid(uid, process)
                    && glass_macos::private_session::session_exists(job_session, uid).ok()
                        == Some(true))
                .then_some(SystemService {
                    process,
                    label: candidate.definition.label,
                    kind: candidate.kind,
                })
            })
            .await?;
            if let Some(service) = service {
                matches.push(service);
                if matches.len() > 1 {
                    break;
                }
            }
        }
        if let Some(service) = unique_match(matches) {
            verified.push(service);
        }
    }
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Definition {
        fn matches_job(
            &self,
            uid: u32,
            session: u32,
            process: ProcessIdentity,
            text: &str,
        ) -> bool {
            self.verified_job_session(uid, session, process, text)
                .is_some()
        }
    }

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

    fn multiple_xpc_value(value: plist::Value) -> plist::Dictionary {
        let mut definition = xpc_value(Some("Application".into()));
        definition
            .get_mut("XPCService")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .insert("_MultipleInstances".into(), value);
        definition
    }

    #[test]
    fn multiple_instances_require_a_protected_boolean_application_definition() {
        for (value, expected) in [(true, true), (false, false)] {
            let definition = XpcDefinition::parse(
                Path::new(XPC_PROGRAM),
                &multiple_xpc_value(value.into()),
                "com.apple.example",
            )
            .unwrap();
            assert_eq!(definition.multiple_instances, expected);
            assert_eq!(definition.kind(false), None);
        }
        assert!(
            !XpcDefinition::parse(
                Path::new(XPC_PROGRAM),
                &xpc_value(None),
                "com.apple.example",
            )
            .unwrap()
            .multiple_instances
        );
        for value in ["true".into(), 1.into(), plist::Value::Array(vec![])] {
            assert!(
                XpcDefinition::parse(
                    Path::new(XPC_PROGRAM),
                    &multiple_xpc_value(value),
                    "com.apple.example"
                )
                .is_none()
            );
        }
        let mut value = multiple_xpc_value(true.into());
        value
            .get_mut("XPCService")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .insert("ServiceType".into(), "User".into());
        assert!(
            XpcDefinition::parse(Path::new(XPC_PROGRAM), &value, "com.apple.example").is_none()
        );
    }

    #[test]
    fn an_absent_executable_uses_only_the_xpc_bundle_stem() {
        let mut value = xpc_value(None);
        value.remove("CFBundleExecutable");
        assert!(
            XpcDefinition::parse(Path::new(XPC_PROGRAM), &value, "com.apple.example").is_some()
        );
        let wrong = XPC_PROGRAM.replace("/MacOS/Example", "/MacOS/Other");
        assert!(XpcDefinition::parse(Path::new(&wrong), &value, "com.apple.example").is_none());
        for executable in ["".into(), "Other".into(), true.into(), 1.into()] {
            value.insert("CFBundleExecutable".into(), executable);
            assert!(
                XpcDefinition::parse(Path::new(XPC_PROGRAM), &value, "com.apple.example").is_none()
            );
        }
    }

    #[test]
    fn only_declared_shared_mach_providers_can_remain_busy() {
        let mut definition = definition();
        let mut code = glass_macos::private_session::PlatformExecutable {
            transaction_tracked: true,
            path: definition.program.clone(),
            idle: false,
            allows_idle_exit: true,
            identifier: definition.label.clone(),
        };
        assert_eq!(
            definition.service_kind(&code),
            Some(ServiceKind::SharedUserAgent)
        );
        code.idle = true;
        assert_eq!(
            definition.service_kind(&code),
            Some(ServiceKind::SharedUserAgent)
        );
        definition.shared_mach_services = false;
        assert_eq!(definition.service_kind(&code), Some(ServiceKind::IdleAgent));
        code.idle = false;
        assert_eq!(definition.service_kind(&code), None);
        definition.shared_mach_services = true;
        code.allows_idle_exit = false;
        assert_eq!(definition.service_kind(&code), None);
    }

    #[test]
    fn shared_providers_require_valid_managed_ports_and_an_active_service() {
        let definition = definition();
        let names = definition.services.iter().map(|name| format!(
            "\t\t\"{name}\" = {{\n\t\t\tport = 0x123\n\t\t\tactive = 1\n\t\t\tmanaged = 1\n\t\t}}"
        )).collect::<Vec<_>>().join("\n");
        let job = format!("job = {{\n\tendpoints = {{\n{names}\n\t}}\n}}\n");
        assert!(definition.valid_shared_endpoints(&job));
        for (before, after) in [
            ("active = 1", "active = 0"),
            ("managed = 1", "managed = 0"),
            ("port = 0x123", "port = 0x0"),
            ("active = 1", "active = 1\n\t\t\tactive = 1"),
            ("\t\t}", "\t\tbroken"),
        ] {
            assert!(
                !definition.valid_shared_endpoints(&job.replace(before, after)),
                "{before}"
            );
        }
        let mut empty = definition;
        empty.services.clear();
        assert!(!empty.valid_shared_endpoints(&job));
    }

    #[test]
    fn shared_providers_can_have_dormant_declared_sibling_ports() {
        let mut value = definition_value();
        value
            .get_mut("MachServices")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .insert("com.apple.example.optional".into(), true.into());
        let mut definition =
            Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), value).unwrap();
        let primary = "\t\t\"com.apple.example\" = {\n\t\t\tport = 0x123\n\t\t\tactive = 1\n\t\t\tmanaged = 1\n\t\t}";
        let sibling = "\t\t\"com.apple.example.optional\" = {\n\t\t\tport = 0x456\n\t\t\tactive = 0\n\t\t\tmanaged = 1\n\t\t}";
        let loaded = |primary: &str, sibling: &str| {
            format!("job = {{\n\tendpoints = {{\n{primary}\n{sibling}\n\t}}\n}}\n")
        };
        let code = glass_macos::private_session::PlatformExecutable {
            transaction_tracked: true,
            path: definition.program.clone(),
            identifier: definition.label.clone(),
            idle: false,
            allows_idle_exit: true,
        };
        assert_eq!(
            definition.kind_for_job(&code, &loaded(primary, sibling)),
            Some(ServiceKind::SharedUserAgent)
        );
        assert!(definition.valid_shared_endpoints(&loaded(
            primary,
            &sibling.replace("active = 0", "active = 1")
        )));
        assert!(!definition.valid_shared_endpoints(&loaded(
            &primary.replace("active = 1", "active = 0"),
            sibling
        )));
        for active in ["0", "1"] {
            let sibling = sibling.replace("active = 0", &format!("active = {active}"));
            for malformed in [
                sibling.replace(&format!("active = {active}"), "active = 2"),
                sibling.replace(&format!("\t\t\tactive = {active}\n"), ""),
                sibling.replace(
                    &format!("active = {active}"),
                    &format!("active = {active}\n\t\t\tactive = {active}"),
                ),
                sibling.replace("managed = 1", "managed = 0"),
                sibling.replace("port = 0x456", "port = 0x0"),
                sibling.replace("port = 0x456", "port = invalid"),
                sibling.replace("\t\t\tmanaged = 1\n", ""),
                sibling.replace("managed = 1", "managed = 1\n\t\t\tmanaged = 1"),
            ] {
                assert!(
                    !definition.valid_shared_endpoints(&loaded(primary, &malformed)),
                    "{malformed}"
                );
            }
        }
        assert!(!definition.valid_shared_endpoints(&loaded(primary, "")));
        assert!(
            !definition.valid_shared_endpoints(&loaded(primary, &format!("{sibling}\n{sibling}")))
        );
        let dormant = loaded(&primary.replace("active = 1", "active = 0"), sibling);
        let undeclared_active = dormant.replace(
            "\tendpoints = {",
            &format!(
                "\tendpoints = {{\n{}",
                primary.replace("com.apple.example", "com.apple.undeclared")
            ),
        );
        assert!(!definition.valid_shared_endpoints(&undeclared_active));
        definition.pressured_exit = false;
        assert!(!definition.valid_shared_endpoints(&loaded(primary, sibling)));
    }

    #[test]
    fn shared_declarations_preserve_a_valid_idle_proof_without_active_ports() {
        let definition = definition();
        let mut code = glass_macos::private_session::PlatformExecutable {
            transaction_tracked: true,
            path: definition.program.clone(),
            identifier: definition.label.clone(),
            idle: true,
            allows_idle_exit: true,
        };
        let dormant = job().replace("active = 1", "active = 0");
        assert_eq!(
            definition.kind_for_job(&code, &dormant),
            Some(ServiceKind::IdleAgent)
        );
        code.idle = false;
        assert_eq!(definition.kind_for_job(&code, &dormant), None);
        let active = job().replace(
            "active = 1",
            "port = 0x123\n\t\t\tactive = 1\n\t\t\tmanaged = 1",
        );
        assert_eq!(
            definition.kind_for_job(&code, &active),
            Some(ServiceKind::SharedUserAgent)
        );
        code.idle = true;
        assert_eq!(
            definition.kind_for_job(&code, &active),
            Some(ServiceKind::SharedUserAgent)
        );
    }

    #[test]
    fn instance_lookup_requires_one_reported_pid_and_protected_label() {
        let label = "com.apple.example.opaque-instance";
        for row in [
            format!("123\t-\t{label}"),
            format!("123\t0\t{label}"),
            format!("123\t-9\t{label}"),
            format!("123\t{label}"),
            format!("123 = {label}"),
        ] {
            let listing = format!(
                "pid/42 = {{\n\tservices = {{\n\t\t999 - com.apple.other.instance\n\t\t{row}\n\t}}\n}}\n"
            );
            assert_eq!(
                instance_label(&listing, "pid/42", process(), "com.apple.example"),
                Some(label),
                "{row}"
            );
        }
    }

    #[test]
    fn instance_lookup_refuses_templates_ambiguity_and_malformed_target_rows() {
        for rows in [
            "123 - com.apple.example",
            "124 - com.apple.example.instance",
            "123 - com.apple.example.",
            "123 - com.apple.examples.instance",
            "123 - com.apple.other.instance",
            "123 - com.apple.example.bad/target",
            "123 - com.apple.example.bad\\target",
            "123 invalid com.apple.example.instance",
            "123 - com.apple.example.instance extra",
            "123 - com.apple.example.instance\n123 - com.apple.example.instance",
            "123 - com.apple.example.one\n123 - com.apple.example.two",
            "123 - com.apple.example.instance\n123 bad row",
            "123",
        ] {
            let listing = format!("pid/42 = {{\n\tservices = {{\n{rows}\n\t}}\n}}\n");
            assert!(
                instance_label(&listing, "pid/42", process(), "com.apple.example").is_none(),
                "{rows}"
            );
        }
    }

    #[test]
    fn instance_lookup_ignores_nested_data_and_refuses_incomplete_or_oversized_domains() {
        let row = "123 - com.apple.example.instance";
        let listing = format!("pid/42 = {{\n\tservices = {{\n{row}\n\t}}\n}}\n");
        for invalid in [
            listing.replace("pid/42", "pid/43"),
            listing.replace("\tservices = {", "\tenvironment = {"),
            listing.replace("\tservices = {", "\targuments = {\n\tservices = {") + "}\n",
            listing.replace(row, &format!("\tnested = {{\n{row}\n\t}}")),
            listing.replace("\t}\n}", "\t}"),
            listing.clone() + "}\n",
            listing.replace("\t}\n}", &format!("\t}}\n\tservices = {{\n{row}\n\t}}\n}}")),
            listing.clone() + &" ".repeat(LIMIT),
            listing.replace(row, &format!("123 - com.apple.example.{}", "x".repeat(255))),
            listing.replace(row, &format!("{row}\0")),
        ] {
            assert!(instance_label(&invalid, "pid/42", process(), "com.apple.example").is_none());
        }
    }

    #[test]
    fn dynamic_instance_still_requires_the_exact_live_job_and_base_bundle_identity() {
        let definition = XpcDefinition::parse(
            Path::new(XPC_PROGRAM),
            &multiple_xpc_value(true.into()),
            "com.apple.example",
        )
        .unwrap();
        let target = "pid/42/com.apple.example.instance";
        let bundle = XPC_PROGRAM.strip_suffix("/Contents/MacOS/Example").unwrap();
        let job = format!(
            "{target} = {{\n\tpath = {bundle}\n\ttype = XPCService\n\tstate = running\n\tbundle id = com.apple.example\n\tprogram = {XPC_PROGRAM}\n\tpid = 123\n\tdomain = pid/42 [com.apple.example.requestor.]\n\tasid = 456\n\tproperties = xpc bundle | system service | supports transactions | supports pressured exit\n}}\n"
        );
        assert_eq!(
            definition.matches_job(target, process(), 456, &job),
            Some(456)
        );
        assert!(
            definition
                .matches_job(target, process(), 457, &job)
                .is_none()
        );
        for (before, after) in [
            ("state = running", "state = not running"),
            ("pid = 123\n", "pid = 124\n"),
            ("domain = pid/42 [", "domain = pid/43 ["),
            ("[com.apple.example.requestor.]", "[]"),
            ("[com.apple.example.requestor.]", "[example] [another]"),
            ("[com.apple.example.requestor.]", "[example"),
            ("\tasid = 456", "\tasid = 457"),
            (
                "bundle id = com.apple.example\n",
                "bundle id = com.apple.example.instance\n",
            ),
            ("supports transactions", "unrelated"),
            ("\tasid = 456", "\targuments = {\n\t}\n\tasid = 456"),
            ("\tasid = 456", "\tasid = 0"),
        ] {
            assert!(
                definition
                    .matches_job(target, process(), 456, &job.replace(before, after))
                    .is_none()
            );
        }
        assert!(
            definition
                .matches_job("pid/42/com.apple.example", process(), 456, &job)
                .is_none()
        );
    }

    #[test]
    fn pid_domain_names_are_opaque_and_do_not_replace_audit_session_proof() {
        for name in ["com.apple.example.requestor.", "Example App Ω", "456"] {
            assert!(pid_domain_matches(&format!("pid/42 [{name}]"), "pid/42"));
        }
        for actual in [
            "pid/43 [com.apple.example.requestor.]",
            "pid/42 []",
            "pid/42 [ ]",
            "pid/42 [nested[name]]",
            "pid/42 [name] [second]",
            "pid/42 [name\n]",
            "pid/42 [name\t]",
            "pid/42 [name] suffix",
            "pid/42 [ name]",
        ] {
            assert!(!pid_domain_matches(actual, "pid/42"), "{actual}");
        }
        assert!(!pid_domain_matches(
            &format!("pid/42 [{}]", "x".repeat(256)),
            "pid/42"
        ));
    }

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
        assert_eq!(
            definition.matches_job(target, process(), 456, &job),
            Some(456)
        );
        assert_eq!(
            definition.matches_job(target, process(), 457, &job),
            Some(456)
        );
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
                    .matches_job(target, process(), 456, &job.replace(before, after))
                    .is_none(),
                "{before}"
            );
        }
        assert!(
            definition
                .matches_job("user/502/user-submitted", process(), 456, &job)
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
                        transaction_tracked: true,
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

    fn persistent_definition_value() -> plist::Dictionary {
        let mut value = definition_value();
        for (key, flag) in [
            ("EnableTransactions", false),
            ("EnablePressuredExit", false),
            ("KeepAlive", true),
            ("RunAtLoad", true),
        ] {
            value.insert(key.into(), flag.into());
        }
        value.insert("LimitLoadToSessionType".into(), "Aqua".into());
        value
    }

    fn persistent_job() -> String {
        job()
            .replace(
                "supports transactions | supports pressured exit | system service",
                "keepalive | runatload | system service",
            )
            .replace(
                "active = 1",
                "port = 0x123\n\t\t\tactive = 1\n\t\t\tmanaged = 1",
            )
    }

    #[test]
    fn untracked_persistent_login_services_need_full_declaration_and_live_endpoint_proof() {
        let definition = Definition::parse(
            format!("{ROOT}/com.apple.example.plist").into(),
            persistent_definition_value(),
        )
        .unwrap();
        let code = glass_macos::private_session::PlatformExecutable {
            path: definition.program.clone(),
            identifier: definition.label.clone(),
            transaction_tracked: false,
            idle: false,
            allows_idle_exit: false,
        };
        assert!(!definition.transaction_idle(&code));
        assert!(definition.matches_job(502, 456, process(), &persistent_job()));
        assert_eq!(
            definition.kind_for_job(&code, &persistent_job()),
            Some(ServiceKind::SharedUserAgent)
        );
        for (before, after) in [
            ("active = 1", "active = 0"),
            ("managed = 1", "managed = 0"),
            ("port = 0x123", "port = 0x0"),
        ] {
            assert_eq!(
                definition.kind_for_job(&code, &persistent_job().replace(before, after)),
                None,
                "{before}"
            );
        }
    }

    #[test]
    fn persistent_service_source_flags_are_explicit_and_nonconditional() {
        for (key, replacement) in [
            ("EnableTransactions", None),
            ("EnableTransactions", Some("false".into())),
            ("KeepAlive", None),
            ("KeepAlive", Some(false.into())),
            ("KeepAlive", Some(plist::Dictionary::new().into())),
            ("RunAtLoad", None),
            ("RunAtLoad", Some(false.into())),
            ("LimitLoadToSessionType", None),
            ("LimitLoadToSessionType", Some("Background".into())),
            ("MultipleInstances", Some(true.into())),
        ] {
            let mut value = persistent_definition_value();
            value.remove(key);
            if let Some(value_to_set) = replacement {
                value.insert(key.into(), value_to_set);
            }
            assert!(
                Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), value)
                    .is_none(),
                "{key}"
            );
        }
    }

    #[test]
    fn persistent_login_jobs_require_corresponding_loaded_lifecycle_properties() {
        let definition = Definition::parse(
            format!("{ROOT}/com.apple.example.plist").into(),
            persistent_definition_value(),
        )
        .unwrap();
        for (before, after) in [
            ("keepalive | ", ""),
            ("runatload | ", ""),
            ("system service", "not system service"),
            (
                "keepalive | runatload",
                "keepalive | runatload | supports transactions",
            ),
            (
                "keepalive | runatload",
                "keepalive | runatload | supports pressured exit",
            ),
        ] {
            assert!(
                !definition.matches_job(
                    502,
                    456,
                    process(),
                    &persistent_job().replace(before, after)
                ),
                "{before}"
            );
        }
    }

    #[test]
    fn untracked_code_cannot_claim_inactivity_or_an_ordinary_transient_agent_role() {
        let ordinary = definition();
        let mut code = glass_macos::private_session::PlatformExecutable {
            path: ordinary.program.clone(),
            identifier: ordinary.label.clone(),
            transaction_tracked: false,
            idle: false,
            allows_idle_exit: false,
        };
        assert_eq!(ordinary.service_kind(&code), None);
        code.idle = true;
        assert!(!ordinary.transaction_idle(&code));
        assert_eq!(ordinary.service_kind(&code), None);
    }

    fn definition() -> Definition {
        Definition::parse(
            format!("{ROOT}/com.apple.example.plist").into(),
            definition_value(),
        )
        .unwrap()
    }

    fn background_instance() -> Definition {
        let mut value = definition_value();
        value.insert("MultipleInstances".into(), true.into());
        value.insert("LimitLoadToSessionType".into(), "Background".into());
        let mut options = plist::Dictionary::new();
        options.insert("ResetAtClose".into(), true.into());
        let mut services = plist::Dictionary::new();
        services.insert("com.apple.example".into(), options.into());
        value.insert("MachServices".into(), services.into());
        Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), value).unwrap()
    }

    fn background_instance_job() -> String {
        job()
            .replacen(
                "gui/502/com.apple.example = {",
                "user/502/com.apple.example.00000000-0100-0000 = {\n\toriginal = com.apple.example",
                1,
            )
            .replace("domain = gui/502 [456]", "domain = user/502")
            .replace("asid = 456", "asid = 789")
            .replace("system service", "is copy | system service")
            .replace("\tendpoints = {", "\tinstance-specific endpoints = {")
    }

    #[test]
    fn declared_background_instances_verify_their_own_user_session() {
        let instance = background_instance();
        assert_eq!(instance.domain(502), "user/502");
        assert_eq!(
            instance.verified_job_session(502, 456, process(), &background_instance_job()),
            Some(789)
        );
        assert!(
            definition()
                .verified_job_session(502, 456, process(), &background_instance_job())
                .is_none()
        );
    }

    #[test]
    fn loaded_provenance_disambiguates_templates_sharing_one_program() {
        let first = background_instance();
        let mut second = background_instance();
        second.label = "com.apple.other".into();
        second.path = format!("{ROOT}/com.apple.other.plist").into();
        second.loaded_path = second.path.clone();
        second
            .arguments
            .as_mut()
            .unwrap()
            .push("--other-role".into());
        assert_eq!(first.program, second.program);
        let templates = [&first, &second];
        let matches = |job: &str| {
            templates
                .iter()
                .filter_map(|definition| definition.verified_job_session(502, 456, process(), job))
                .collect::<Vec<_>>()
        };
        assert_eq!(unique_match(matches(&background_instance_job())), Some(789));
        assert!(
            unique_match(matches(
                &background_instance_job().replace("pid = 123", "pid = 124")
            ))
            .is_none()
        );
        let duplicate_matches = [&first, &first].into_iter().filter_map(|definition| {
            definition.verified_job_session(502, 456, process(), &background_instance_job())
        });
        assert!(unique_match(duplicate_matches).is_none());
    }

    #[test]
    fn existing_mixed_session_declarations_keep_the_exact_gui_proof() {
        for session in [
            "Aqua".into(),
            plist::Value::Array(vec!["LoginWindow".into(), "Aqua".into()]),
            plist::Value::Array(vec!["Aqua".into(), "CryptexContainer".into()]),
            plist::Value::Array(vec!["Background".into(), "Aqua".into()]),
        ] {
            let mut value = definition_value();
            value.insert("LimitLoadToSessionType".into(), session);
            let definition =
                Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), value).unwrap();
            assert_eq!(definition.domain(502), "gui/502");
            assert_eq!(
                definition.verified_job_session(502, 456, process(), &job()),
                Some(456)
            );
            assert!(
                definition
                    .verified_job_session(502, 457, process(), &job())
                    .is_none()
            );
        }
    }

    #[test]
    fn instance_jobs_require_the_declared_copy_identity_and_exact_runtime_bindings() {
        let definition = background_instance();
        for (before, after) in [
            ("original = com.apple.example", "original = com.apple.other"),
            ("\toriginal = com.apple.example\n", ""),
            ("is copy | ", ""),
            ("user/502", "user/503"),
            ("com.apple.example.00000000", "com.apple.other.00000000"),
            ("com.apple.example.00000000-0100-0000", "com.apple.example."),
            ("asid = 789", "asid = 0"),
            ("asid = 789", "asid = 2147483647"),
            ("pid = 123", "pid = 124"),
            ("/usr/libexec/example", "/usr/libexec/other"),
            ("supports transactions", "other property"),
            ("system service", "user service"),
            ("instance-specific endpoints", "endpoints"),
        ] {
            assert!(
                definition
                    .verified_job_session(
                        502,
                        456,
                        process(),
                        &background_instance_job().replace(before, after),
                    )
                    .is_none(),
                "{before}"
            );
        }
    }

    #[test]
    fn dictionary_endpoints_do_not_turn_background_instances_into_shared_providers() {
        let definition = background_instance();
        assert!(!definition.shared_mach_services);
        let mut code = glass_macos::private_session::PlatformExecutable {
            transaction_tracked: true,
            path: definition.program.clone(),
            identifier: definition.label.clone(),
            idle: false,
            allows_idle_exit: true,
        };
        assert!(
            definition
                .kind_for_job(&code, &background_instance_job())
                .is_none()
        );
        code.idle = true;
        assert_eq!(
            definition.kind_for_job(&code, &background_instance_job()),
            Some(ServiceKind::IdleAgent)
        );
    }

    #[test]
    fn instance_and_session_metadata_must_have_the_declared_types() {
        for value in ["true".into(), 1u64.into(), plist::Value::Array(Vec::new())] {
            let mut fields = definition_value();
            fields.insert("MultipleInstances".into(), value);
            assert!(
                Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), fields)
                    .is_none()
            );
        }
        for value in [
            true.into(),
            1u64.into(),
            plist::Value::Array(vec![false.into()]),
        ] {
            let mut fields = definition_value();
            fields.insert("LimitLoadToSessionType".into(), value);
            assert!(
                Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), fields)
                    .is_none()
            );
        }
        let mut fields = definition_value();
        fields.insert(
            "LimitLoadToSessionType".into(),
            plist::Value::Array(vec!["Background".into()]),
        );
        assert_eq!(
            Definition::parse(format!("{ROOT}/com.apple.example.plist").into(), fields)
                .unwrap()
                .domain(502),
            "user/502"
        );
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
    fn a_matching_discovery_hint_cannot_authorize_an_unprotected_definition() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("com.apple.example.plist");
        plist::to_file_xml(&source, &definition_value()).unwrap();
        let executable = Path::new("/usr/libexec/example");
        let hint = AgentHint {
            source,
            program: executable.into(),
        };
        assert!(hint.read_for(executable).is_none());
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
