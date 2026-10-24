//! prepare requested workspace access before forwarding a backend start call.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;

use crate::params::StartArgs;

pub(crate) enum PreparationError {
    Refused(anyhow::Error),
    CleanupUnconfirmed(anyhow::Error),
}

pub(crate) trait Prepare: Send + 'static {
    fn prepare(&mut self, args: &StartArgs) -> Result<(), PreparationError>;
}

struct Paths {
    cwd: PathBuf,
    search_dirs: Vec<PathBuf>,
    arguments: Vec<PathBuf>,
}

fn requested_paths(args: &StartArgs, base: &Path, inherited_path: Option<&str>) -> Paths {
    let cwd = args
        .cwd
        .as_ref()
        .map_or_else(|| base.to_path_buf(), |path| base.join(path));
    let search_dirs = args
        .env
        .get("PATH")
        .map(String::as_str)
        .or(inherited_path)
        .map(|path| {
            std::env::split_paths(path)
                .map(|path| cwd.join(path))
                .collect()
        })
        .unwrap_or_default();
    let arguments = args
        .run
        .iter()
        .enumerate()
        .filter(|(index, token)| *index != 0 || token.contains('/'))
        .map(|(_, token)| cwd.join(token))
        .collect();
    Paths {
        cwd,
        search_dirs,
        arguments,
    }
}

fn protected_path(path: &Path) -> anyhow::Result<PathBuf> {
    match path.canonicalize() {
        Ok(real) => Ok(real),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .context("protected path has no existing ancestor")?;
            let name = path
                .file_name()
                .context("protected path cannot be resolved")?;
            Ok(protected_path(parent)?.join(name))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct NativePrepare {
    owner: u32,
    worker: u32,
    base: PathBuf,
    home: PathBuf,
    inherited_path: Option<String>,
    protected: Vec<PathBuf>,
    closed: bool,
    scopes: Vec<glass_macos::workspace_access::Sharing>,
    recorder: Option<glass_macos::workspace_access::Recorder>,
    output: Option<glass_macos::workspace_access::outputs::OutputScope>,
    started: bool,
}

#[cfg(target_os = "macos")]
impl NativePrepare {
    pub(crate) fn for_backend(
        worker: u32,
        base: PathBuf,
        home: PathBuf,
        inherited_path: Option<String>,
        protected: Vec<PathBuf>,
        recorder: glass_macos::workspace_access::Recorder,
    ) -> anyhow::Result<Self> {
        let owner = rustix::process::geteuid().as_raw();
        let mut result = Self {
            owner,
            worker,
            base,
            home,
            inherited_path,
            protected: protected
                .iter()
                .map(|path| protected_path(path))
                .collect::<anyhow::Result<_>>()?,
            closed: false,
            scopes: Vec::new(),
            recorder: Some(recorder),
            output: None,
            started: false,
        };
        let mut scope = result.new_scope()?;
        if let Err(error) = result.share(
            &mut scope,
            &result.base,
            glass_macos::workspace_access::Access::Search,
        ) {
            scope
                .restore()
                .context("backend bootstrap permission cleanup failed")?;
            return Err(error);
        }
        result.scopes.push(scope);
        Ok(result)
    }

    pub(crate) fn protect(
        &mut self,
        paths: &[glass_core::ProtectedHostPath],
    ) -> Result<(), PreparationError> {
        use std::os::unix::fs::MetadataExt;

        if self.started {
            return Err(PreparationError::Refused(anyhow::anyhow!(
                "protected paths cannot change after private application preparation"
            )));
        }

        let mut scope = self
            .new_scope()
            .map_err(|error| PreparationError::Refused(error.into()))?;
        let prepared = (|| {
            glass_sandbox_macos::profile::validate_protected_paths(paths)?;
            let resolved = paths
                .iter()
                .map(|path| protected_path(&path.path))
                .collect::<anyhow::Result<Vec<_>>>()?;
            for path in &resolved {
                // The trusted worker can stat the protected node without reading its contents.
                for parent in path.ancestors().skip(1) {
                    if parent.metadata()?.uid() == self.owner {
                        scope.search(parent)?;
                    }
                }
            }
            Ok::<_, anyhow::Error>(resolved)
        })();
        match prepared {
            Ok(resolved) => {
                self.protected.extend(resolved);
                self.scopes.push(scope);
                Ok(())
            }
            Err(error) => match scope.restore() {
                Ok(()) => Err(PreparationError::Refused(error)),
                Err(cleanup) => {
                    self.scopes.push(scope);
                    Err(PreparationError::CleanupUnconfirmed(anyhow::anyhow!(
                        "{error}; protected-path ancestry cleanup: {cleanup}"
                    )))
                }
            },
        }
    }

    pub(crate) fn prepare_spec(
        &mut self,
        spec: &glass_core::AppSpec,
    ) -> Result<Option<glass_macos::workspace_access::outputs::OutputScope>, PreparationError> {
        self.prepare(&StartArgs {
            _rejected_input_mode: None,
            build: spec.build.clone(),
            run: spec.run.clone(),
            backend: Some("macos".into()),
            sandbox: Some(spec.sandbox.to_string()),
            cwd: spec
                .cwd
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            env: spec.env.iter().cloned().collect(),
            window_hint: None,
            timeout_ms: Some(spec.timeout_ms),
            a11y: Some(spec.a11y),
        })?;
        Ok(self.output.take())
    }

    fn share(
        &self,
        scope: &mut glass_macos::workspace_access::Sharing,
        literal: &Path,
        access: glass_macos::workspace_access::Access,
    ) -> anyhow::Result<()> {
        use glass_macos::workspace_access::Access;
        use std::os::unix::fs::MetadataExt;
        let path = literal.canonicalize()?;
        let metadata = path.metadata()?;
        anyhow::ensure!(
            !self
                .protected
                .iter()
                .any(|protected| path.starts_with(protected)),
            "requested workspace overlaps protected Glass evidence or service state"
        );
        for ancestor in literal.ancestors().skip(1).chain(path.ancestors().skip(1)) {
            let real = ancestor.canonicalize()?;
            if real.metadata()?.uid() == self.owner {
                scope.search(&real)?;
            }
        }
        if metadata.uid() == self.owner || metadata.uid() == self.worker {
            scope.path_excluding(
                &path,
                if path == self.home {
                    Access::Search
                } else {
                    access
                },
                &self.protected,
            )?;
        }
        Ok(())
    }

    pub(crate) fn restore(&mut self) -> anyhow::Result<()> {
        self.closed = true;
        let mut failure = None;
        for scope in self.scopes.iter_mut().rev() {
            if let Err(error) = scope.restore() {
                failure = Some(error);
            }
        }
        match failure {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    pub(crate) fn revoke_access(&mut self, deadline: glass_core::Deadline) -> anyhow::Result<()> {
        for scope in self.scopes.iter_mut().rev() {
            scope.revoke_access(deadline)?;
        }
        Ok(())
    }

    fn new_scope(&self) -> std::io::Result<glass_macos::workspace_access::Sharing> {
        match &self.recorder {
            Some(recorder) => {
                glass_macos::workspace_access::Sharing::recorded(self.worker, Arc::clone(recorder))
            }
            None => glass_macos::workspace_access::Sharing::new(self.worker),
        }
    }
}

#[cfg(target_os = "macos")]
impl Prepare for NativePrepare {
    fn prepare(&mut self, args: &StartArgs) -> Result<(), PreparationError> {
        use glass_macos::workspace_access::Access;
        if self.closed {
            return Err(PreparationError::Refused(anyhow::anyhow!(
                "private worker connection has ended"
            )));
        }
        let paths = requested_paths(args, &self.base, self.inherited_path.as_deref());
        self.output = None;
        let mut scope = self
            .new_scope()
            .map_err(|error| PreparationError::Refused(error.into()))?;
        let result = (|| {
            anyhow::ensure!(
                paths.cwd.is_dir(),
                "requested cwd is not an accessible directory"
            );
            self.share(&mut scope, &paths.cwd, Access::ReadWrite)?;
            for directory in &paths.search_dirs {
                if directory.is_dir() {
                    self.share(&mut scope, directory, Access::Read)?;
                }
            }
            for argument in &paths.arguments {
                if argument.exists() {
                    self.share(&mut scope, argument, Access::Read)?;
                }
            }
            let cwd = paths.cwd.canonicalize()?;
            let uid = std::os::unix::fs::MetadataExt::uid(&cwd.metadata()?);
            let output = if cwd != self.home && [self.owner, self.worker].contains(&uid) {
                Some(
                    glass_macos::workspace_access::outputs::OutputScope::capture_shared(
                        &cwd,
                        self.worker,
                        &self.protected,
                        &scope,
                    )?,
                )
            } else {
                None
            };
            Ok::<_, anyhow::Error>(output)
        })();
        match result {
            Ok(output) => {
                self.output = output;
                self.started = true;
                self.scopes.push(scope);
                Ok(())
            }
            Err(error) => match scope.restore() {
                Ok(()) => Err(PreparationError::Refused(error)),
                Err(cleanup) => {
                    self.scopes.push(scope);
                    Err(PreparationError::CleanupUnconfirmed(anyhow::anyhow!(
                        "workspace preparation failed: {error:#}; permission cleanup unconfirmed: {cleanup}"
                    )))
                }
            },
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn a_trace_directory_below_the_requested_project_does_not_refuse_the_launch() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        let trace = project.join(".traces");
        std::fs::create_dir_all(&trace).unwrap();
        std::fs::write(project.join("source"), "source").unwrap();
        let mut prepare = NativePrepare {
            owner: rustix::process::geteuid().as_raw(),
            worker: u32::MAX,
            base: project.canonicalize().unwrap(),
            home: root.path().canonicalize().unwrap(),
            inherited_path: None,
            protected: vec![trace.canonicalize().unwrap()],
            closed: false,
            scopes: Vec::new(),
            recorder: None,
            output: None,
            started: false,
        };
        let args =
            serde_json::from_value(serde_json::json!({"run":["/usr/bin/true"], "a11y":false}))
                .unwrap();
        if let Err(failure) = prepare.prepare(&args) {
            let error = match failure {
                PreparationError::Refused(error) | PreparationError::CleanupUnconfirmed(error) => {
                    error
                }
            };
            panic!("workspace with nested trace root was refused: {error:#}");
        }
        prepare.restore().unwrap();
        std::fs::write(project.join("source"), "owner can still edit").unwrap();
    }
}
