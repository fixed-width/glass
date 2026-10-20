//! Read-only attribution of idle platform infrastructure in the worker's own login.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use glass_macos::private_session::{ProcessIdentity, idle_system_executable};
use rustix::fs::{Mode, OFlags, open, openat};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

const ROOT: &str = "/System/Library/LaunchAgents";
const LIMIT: usize = 128 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IdleService {
    pub process: ProcessIdentity,
    pub label: String,
}

struct Definition {
    label: String,
    path: PathBuf,
    program: PathBuf,
    arguments: Vec<String>,
    services: Vec<String>,
    source: Option<(ProtectedFile, Vec<u8>)>,
}

struct ProtectedFile {
    file: File,
    parents: Vec<File>,
}

impl ProtectedFile {
    fn open(path: &Path) -> Option<Self> {
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
            if !Self::protected(&current, true) {
                return None;
            }
            let flags = OFlags::RDONLY
                | OFlags::NOFOLLOW
                | OFlags::CLOEXEC
                | if index + 1 < parts.len() {
                    OFlags::DIRECTORY
                } else {
                    OFlags::empty()
                };
            let next = File::from(openat(&current, *part, flags, Mode::empty()).ok()?);
            parents.push(current);
            current = next;
        }
        if !Self::protected(&current, false) {
            return None;
        }
        Some(Self {
            file: current,
            parents,
        })
    }

    fn protected(file: &File, directory: bool) -> bool {
        file.metadata().is_ok_and(|metadata| {
            metadata.uid() == 0
                && metadata.mode() & 0o022 == 0
                && if directory {
                    metadata.is_dir()
                } else {
                    metadata.is_file() && metadata.nlink() == 1
                }
        }) && glass_macos::private_session::require_write_protected(file, 0).is_ok()
    }

    fn same(&self, other: &Self) -> bool {
        self.parents.len() == other.parents.len()
            && self
                .parents
                .iter()
                .zip(&other.parents)
                .all(|(before, after)| Self::same_file(before, after, true))
            && Self::same_file(&self.file, &other.file, false)
    }

    fn same_file(before: &File, after: &File, directory: bool) -> bool {
        Self::protected(before, directory)
            && Self::protected(after, directory)
            && before
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
        if !label_valid(&label) || !value.get("EnablePressuredExit")?.as_boolean()? {
            return None;
        }
        let arguments = value
            .get("ProgramArguments")?
            .as_array()?
            .iter()
            .map(|argument| argument.as_string().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        if arguments.is_empty()
            || arguments
                .iter()
                .any(|argument| argument.contains(['\n', '\r', '\0']))
        {
            return None;
        }
        let program = value
            .get("Program")
            .map_or(Some(arguments[0].as_str()), plist::Value::as_string)?;
        if !Path::new(program).is_absolute() {
            return None;
        }
        let services = value.get("MachServices")?.as_dictionary()?;
        if services.is_empty()
            || services.iter().any(|(name, enabled)| {
                name.is_empty()
                    || name.contains(['\n', '\r', '\0', '"'])
                    || !(enabled.as_boolean() == Some(true) || enabled.as_dictionary().is_some())
            })
        {
            return None;
        }
        Some(Self {
            label,
            path,
            program: program.into(),
            arguments,
            services: services.keys().cloned().collect(),
            source: None,
        })
    }

    fn read(path: PathBuf) -> Option<Self> {
        if path.parent()? != Path::new(ROOT) {
            return None;
        }
        let source = ProtectedFile::open(&path)?;
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
        definition.source = Some((source, bytes));
        Some(definition)
    }

    fn unchanged(&self) -> bool {
        let Some((source, bytes)) = &self.source else {
            return false;
        };
        Self::read(self.path.clone())
            .and_then(|definition| definition.source)
            .is_some_and(|(current, current_bytes)| {
                source.same(&current) && bytes == &current_bytes
            })
    }

    fn matches_job(&self, uid: u32, session: u32, process: ProcessIdentity, text: &str) -> bool {
        let lines: Vec<_> = text.lines().collect();
        if lines.first().copied() != Some(format!("gui/{uid}/{} = {{", self.label).as_str())
            || lines.last().copied() != Some("}")
        {
            return false;
        }
        let mut fields = BTreeMap::new();
        for line in &lines[1..lines.len() - 1] {
            if let Some(line) = line
                .strip_prefix('\t')
                .filter(|line| !line.starts_with('\t'))
            {
                let Some((key, value)) = line.split_once(" = ") else {
                    continue;
                };
                if fields.insert(key, value).is_some() {
                    return false;
                }
            }
        }
        let expected = [
            ("path", self.path.to_string_lossy().into_owned()),
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
        if [
            "system service",
            "supports pressured exit",
            "supports transactions",
        ]
        .iter()
        .any(|needed| !properties.split(" | ").any(|property| property == *needed))
        {
            return false;
        }
        let Some(arguments) = block(&lines, "arguments") else {
            return false;
        };
        if arguments
            .iter()
            .map(|line| line.strip_prefix("\t\t"))
            .collect::<Option<Vec<_>>>()
            != Some(self.arguments.iter().map(String::as_str).collect())
        {
            return false;
        }
        let Some(endpoints) = block(&lines, "endpoints") else {
            return false;
        };
        self.services
            .iter()
            .all(|name| endpoints.contains(&format!("\t\t\"{name}\" = {{").as_str()))
    }
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
    let mut child = tokio::process::Command::new("/bin/launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
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

pub(super) async fn inspect(bytes: &[u8]) -> anyhow::Result<Vec<IdleService>> {
    let processes: Vec<ProcessIdentity> = serde_json::from_slice(bytes)?;
    anyhow::ensure!(processes.len() <= 64, "too many idle-service candidates");
    let uid = rustix::process::geteuid().as_raw();
    let session = glass_macos::private_session::session_id()?;
    let entries = std::fs::read_dir(ROOT)?
        .take(4097)
        .collect::<std::io::Result<Vec<_>>>()?;
    anyhow::ensure!(
        entries.len() <= 4096,
        "system service catalog exceeds bound"
    );
    let mut idle = Vec::new();
    for process in processes {
        let Some(path) = idle_system_executable(uid, &process) else {
            continue;
        };
        let Some(executable) = ProtectedFile::open(&path) else {
            continue;
        };
        let mut matching = entries
            .iter()
            .filter_map(|entry| Definition::read(entry.path()))
            .filter(|definition| definition.program == path);
        let Some(definition) = matching.next() else {
            continue;
        };
        if matching.next().is_some() {
            continue;
        }
        let Some(job) = loaded_job(uid, &definition.label).await else {
            continue;
        };
        if definition.matches_job(uid, session, process, &job)
            && idle_system_executable(uid, &process).as_ref() == Some(&path)
            && definition.unchanged()
            && ProtectedFile::open(&path).is_some_and(|current| executable.same(&current))
        {
            idle.push(IdleService {
                process,
                label: definition.label.clone(),
            });
        }
    }
    Ok(idle)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn definition() -> Definition {
        Definition::parse(
            format!("{ROOT}/com.apple.example.plist").into(),
            plist::from_bytes(
                br#"
            <plist version="1.0"><dict>
            <key>Label</key><string>com.apple.example</string>
            <key>ProgramArguments</key><array><string>/usr/libexec/example</string></array>
            <key>EnablePressuredExit</key><true/>
            <key>MachServices</key><dict><key>com.apple.example</key><true/></dict>
            </dict></plist>"#,
            )
            .unwrap(),
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
