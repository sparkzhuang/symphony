mod hooks;
pub mod path_safety;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{info, warn};

use crate::types::{Workspace, WorkspaceHooks, WorkspaceKey};
pub use hooks::HookName;
use hooks::{run_hook as execute_hook, run_remote_command};
pub use path_safety::PathSafetyError;

const DEFAULT_HOOK_TIMEOUT_MS: u64 = 60_000;
const REMOTE_WORKSPACE_MARKER: &str = "__SYMPHONY_WORKSPACE__";

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error(transparent)]
    PathSafety(#[from] PathSafetyError),
    #[error("failed to spawn `{program}`: {source}")]
    CommandSpawn { program: String, source: io::Error },
    #[error("failed while waiting for `{program}`: {source}")]
    CommandWait { program: String, source: io::Error },
    #[error("workspace hook `{hook:?}` timed out after {timeout_ms} ms")]
    HookTimeout { hook: HookName, timeout_ms: u64 },
    #[error("workspace hook `{hook:?}` failed with status {status}")]
    HookFailed {
        hook: HookName,
        status: i32,
        output: String,
    },
    #[error("remote workspace preparation failed on `{host}` with status {status}")]
    RemotePrepareFailed {
        host: String,
        status: i32,
        output: String,
    },
    #[error("remote workspace removal failed on `{host}` with status {status}")]
    RemoteRemoveFailed {
        host: String,
        status: i32,
        output: String,
    },
    #[error("invalid remote workspace output: {output}")]
    InvalidRemoteWorkspaceOutput { output: String },
    #[error("invalid remote workspace path `{path}`: {reason}")]
    InvalidRemoteWorkspacePath { path: String, reason: String },
    #[error("failed to remove existing path `{path}`: {source}")]
    RemoveExistingPath { path: PathBuf, source: io::Error },
    #[error("failed to create workspace directory `{path}`: {source}")]
    CreateDirectory { path: PathBuf, source: io::Error },
}

pub struct WorkspaceManager {
    root: PathBuf,
    hooks: WorkspaceHooks,
    ssh_hosts: Vec<String>,
}

impl WorkspaceManager {
    pub fn new(root: impl AsRef<Path>, hooks: WorkspaceHooks) -> Result<Self, WorkspaceError> {
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            hooks,
            ssh_hosts: Vec::new(),
        })
    }

    pub fn with_ssh_hosts<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.ssh_hosts = hosts.into_iter().map(Into::into).collect();
        self
    }

    pub async fn create_for_issue(
        &self,
        identifier: &str,
        worker_host: Option<&str>,
    ) -> Result<Workspace, WorkspaceError> {
        let workspace_key = workspace_key(identifier);

        match worker_host {
            Some(host) => self.create_remote_workspace(workspace_key, host).await,
            None => self.create_local_workspace(workspace_key).await,
        }
    }

    pub async fn run_hook(
        &self,
        hook: HookName,
        workspace: &Path,
        issue_identifier: &str,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        let command = match self.command_for_hook(hook) {
            Some(command) => command,
            None => return Ok(()),
        };

        info!(
            hook = hook.as_str(),
            workspace = %workspace.display(),
            issue_identifier,
            worker_host = worker_host.unwrap_or("local"),
            "dispatching workspace hook"
        );

        execute_hook(
            command,
            workspace,
            hook,
            self.hook_timeout_ms(),
            worker_host,
        )
        .await
    }

    pub async fn run_after_run_hook(
        &self,
        workspace: &Path,
        issue_identifier: &str,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        if let Err(error) = self
            .run_hook(HookName::AfterRun, workspace, issue_identifier, worker_host)
            .await
        {
            warn!(
                workspace = %workspace.display(),
                issue_identifier,
                worker_host = worker_host.unwrap_or("local"),
                error = %error,
                "after_run hook failed and was ignored"
            );
        }

        Ok(())
    }

    pub async fn remove(
        &self,
        workspace: &Path,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        match worker_host {
            Some(host) => self.remove_remote(workspace, host).await,
            None => self.remove_local(workspace).await,
        }
    }

    pub async fn remove_issue_workspaces(&self, identifier: &str) {
        if self.ssh_hosts.is_empty() {
            let path = self.root.join(workspace_key(identifier).as_str());
            let _ = self.remove(&path, None).await;
            return;
        }

        for host in &self.ssh_hosts {
            let path = self.root.join(workspace_key(identifier).as_str());
            let _ = self.remove(&path, Some(host)).await;
        }
    }

    async fn create_local_workspace(
        &self,
        workspace_key: WorkspaceKey,
    ) -> Result<Workspace, WorkspaceError> {
        let workspace_path = self.root.join(workspace_key.as_str());
        let safe_path = path_safety::validate_workspace_path(&self.root, &workspace_path)?;

        fs::create_dir_all(&self.root).map_err(|source| WorkspaceError::CreateDirectory {
            path: self.root.clone(),
            source,
        })?;

        let created_now = ensure_local_workspace(&safe_path)?;
        let workspace = Workspace {
            path: safe_path,
            workspace_key,
            created_now,
        };

        if workspace.created_now {
            self.run_hook(
                HookName::AfterCreate,
                &workspace.path,
                workspace.workspace_key.as_str(),
                None,
            )
            .await?;
        }

        Ok(workspace)
    }

    async fn create_remote_workspace(
        &self,
        workspace_key: WorkspaceKey,
        host: &str,
    ) -> Result<Workspace, WorkspaceError> {
        let workspace_path = self.root.join(workspace_key.as_str());
        validate_remote_workspace_path(&workspace_path)?;

        let script = remote_workspace_script(&workspace_path);
        let output =
            run_remote_command(host, &script, self.hook_timeout_ms(), HookName::AfterCreate)
                .await?;

        if output.status != 0 {
            return Err(WorkspaceError::RemotePrepareFailed {
                host: host.to_owned(),
                status: output.status,
                output: output.output,
            });
        }

        let mut workspace = parse_remote_workspace_output(&output.output, &workspace_path)?;
        workspace.workspace_key = workspace_key;

        if workspace.created_now {
            self.run_hook(
                HookName::AfterCreate,
                &workspace.path,
                workspace.workspace_key.as_str(),
                Some(host),
            )
            .await?;
        }

        Ok(workspace)
    }

    async fn remove_local(&self, workspace: &Path) -> Result<(), WorkspaceError> {
        if !workspace.exists() {
            return Ok(());
        }

        let safe_path = path_safety::validate_workspace_path(&self.root, workspace)?;

        if safe_path.is_dir() {
            self.run_before_remove(&safe_path, None).await;
        }

        remove_existing_path(&safe_path)?;
        Ok(())
    }

    async fn remove_remote(&self, workspace: &Path, host: &str) -> Result<(), WorkspaceError> {
        validate_remote_workspace_path(workspace)?;
        if let Some(command) = self.command_for_hook(HookName::BeforeRemove) {
            let hook_script = remote_before_remove_script(workspace, command);
            if let Err(error) = run_remote_command(
                host,
                &hook_script,
                self.hook_timeout_ms(),
                HookName::BeforeRemove,
            )
            .await
            .and_then(|output| handle_hook_output(output, HookName::BeforeRemove))
            {
                warn!(
                    workspace = %workspace.display(),
                    worker_host = host,
                    error = %error,
                    "before_remove hook failed and was ignored"
                );
            }
        }

        let script = format!(
            "{}\nrm -rf \"$workspace\"",
            remote_workspace_assignment(workspace)
        );
        let output = run_remote_command(
            host,
            &script,
            self.hook_timeout_ms(),
            HookName::BeforeRemove,
        )
        .await?;

        if output.status == 0 {
            return Ok(());
        }

        Err(WorkspaceError::RemoteRemoveFailed {
            host: host.to_owned(),
            status: output.status,
            output: output.output,
        })
    }

    async fn run_before_remove(&self, workspace: &Path, worker_host: Option<&str>) {
        if let Err(error) = self
            .run_hook(
                HookName::BeforeRemove,
                workspace,
                workspace
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("issue"),
                worker_host,
            )
            .await
        {
            warn!(
                workspace = %workspace.display(),
                worker_host = worker_host.unwrap_or("local"),
                error = %error,
                "before_remove hook failed and was ignored"
            );
        }
    }

    fn command_for_hook(&self, hook: HookName) -> Option<&str> {
        match hook {
            HookName::AfterCreate => self.hooks.after_create.as_deref(),
            HookName::BeforeRun => self.hooks.before_run.as_deref(),
            HookName::AfterRun => self.hooks.after_run.as_deref(),
            HookName::BeforeRemove => self.hooks.before_remove.as_deref(),
        }
    }

    fn hook_timeout_ms(&self) -> u64 {
        match self.hooks.timeout_ms {
            Some(timeout_ms) if timeout_ms > 0 => timeout_ms,
            _ => DEFAULT_HOOK_TIMEOUT_MS,
        }
    }
}

pub fn parse_remote_workspace_output(
    output: &str,
    expected_workspace: &Path,
) -> Result<Workspace, WorkspaceError> {
    let payload = output.lines().find_map(|line| {
        let mut parts = line.splitn(4, '\t');
        let marker = parts.next()?;
        let created = parts.next()?;
        let root = parts.next()?;
        let path = parts.next()?;

        if marker == REMOTE_WORKSPACE_MARKER
            && matches!(created, "0" | "1")
            && !root.is_empty()
            && !path.is_empty()
        {
            Some((created == "1", PathBuf::from(root), PathBuf::from(path)))
        } else {
            None
        }
    });

    match payload {
        Some((created_now, root, path)) => {
            let workspace_key =
                validate_remote_workspace_canonical_path(expected_workspace, &root, &path)?;

            Ok(Workspace {
                workspace_key,
                path,
                created_now,
            })
        }
        None => Err(WorkspaceError::InvalidRemoteWorkspaceOutput {
            output: output.to_owned(),
        }),
    }
}

fn workspace_key(identifier: &str) -> WorkspaceKey {
    if identifier.trim().is_empty() {
        WorkspaceKey::from_issue_identifier("issue")
    } else {
        WorkspaceKey::from_issue_identifier(identifier)
    }
}

fn ensure_local_workspace(path: &Path) -> Result<bool, WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(false),
        Ok(_) => {
            remove_existing_path(path)?;
            fs::create_dir_all(path).map_err(|source| WorkspaceError::CreateDirectory {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|source| WorkspaceError::CreateDirectory {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(true)
        }
        Err(error) => Err(WorkspaceError::RemoveExistingPath {
            path: path.to_path_buf(),
            source: error,
        }),
    }
}

fn remove_existing_path(path: &Path) -> Result<(), WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(path).map_err(|source| WorkspaceError::RemoveExistingPath {
                path: path.to_path_buf(),
                source,
            })
        }
        Ok(_) => fs::remove_file(path).map_err(|source| WorkspaceError::RemoveExistingPath {
            path: path.to_path_buf(),
            source,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WorkspaceError::RemoveExistingPath {
            path: path.to_path_buf(),
            source: error,
        }),
    }
}

fn validate_remote_workspace_path(path: &Path) -> Result<(), WorkspaceError> {
    let display = path.to_string_lossy();

    if display.trim().is_empty() {
        return Err(WorkspaceError::InvalidRemoteWorkspacePath {
            path: display.into_owned(),
            reason: "path is empty".to_owned(),
        });
    }

    if display.contains('\n') || display.contains('\r') || display.contains('\0') {
        return Err(WorkspaceError::InvalidRemoteWorkspacePath {
            path: display.into_owned(),
            reason: "path contains control characters".to_owned(),
        });
    }

    Ok(())
}

fn validate_remote_workspace_canonical_path(
    expected_workspace: &Path,
    canonical_root: &Path,
    canonical_workspace: &Path,
) -> Result<WorkspaceKey, WorkspaceError> {
    validate_remote_workspace_path(expected_workspace)?;
    validate_remote_workspace_path(canonical_root)?;
    validate_remote_workspace_path(canonical_workspace)?;

    let expected_key = expected_workspace
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| WorkspaceError::InvalidRemoteWorkspacePath {
            path: expected_workspace.display().to_string(),
            reason: "missing workspace key".to_owned(),
        })?;
    let actual_key = canonical_workspace
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| WorkspaceError::InvalidRemoteWorkspacePath {
            path: canonical_workspace.display().to_string(),
            reason: "missing workspace key".to_owned(),
        })?;

    if canonical_workspace == canonical_root {
        return Err(WorkspaceError::InvalidRemoteWorkspacePath {
            path: canonical_workspace.display().to_string(),
            reason: "workspace must not equal canonical root".to_owned(),
        });
    }

    if actual_key != expected_key {
        return Err(WorkspaceError::InvalidRemoteWorkspacePath {
            path: canonical_workspace.display().to_string(),
            reason: format!(
                "workspace key mismatch: expected `{expected_key}`, got `{actual_key}`"
            ),
        });
    }

    if canonical_workspace.parent() != Some(canonical_root) {
        return Err(WorkspaceError::InvalidRemoteWorkspacePath {
            path: canonical_workspace.display().to_string(),
            reason: format!(
                "workspace is not a direct child of canonical root `{}`",
                canonical_root.display()
            ),
        });
    }

    if let Some(expected_root) = expected_workspace.parent() {
        if expected_root.is_absolute() && !canonical_workspace.starts_with(expected_root) {
            return Err(WorkspaceError::InvalidRemoteWorkspacePath {
                path: canonical_workspace.display().to_string(),
                reason: format!(
                    "workspace escaped requested root `{}`",
                    expected_root.display()
                ),
            });
        }
    }

    Ok(WorkspaceKey::new(actual_key))
}

fn remote_workspace_script(workspace: &Path) -> String {
    let mut script = String::new();
    script.push_str("set -eu\n");
    script.push_str(&remote_workspace_assignment(workspace));
    script.push('\n');
    script.push_str("if [ -d \"$workspace\" ]; then\n");
    script.push_str("  created=0\n");
    script.push_str("elif [ -e \"$workspace\" ]; then\n");
    script.push_str("  rm -rf \"$workspace\"\n");
    script.push_str("  mkdir -p \"$workspace\"\n");
    script.push_str("  created=1\n");
    script.push_str("else\n");
    script.push_str("  mkdir -p \"$workspace\"\n");
    script.push_str("  created=1\n");
    script.push_str("fi\n");
    script.push_str("cd \"$workspace\"\n");
    script.push_str("canonical_workspace=\"$(pwd -P)\"\n");
    script.push_str("canonical_root=\"$(cd \"$(dirname \"$workspace\")\" && pwd -P)\"\n");
    script.push_str(&format!(
        "printf '%s\\t%s\\t%s\\t%s\\n' '{REMOTE_WORKSPACE_MARKER}' \"$created\" \"$canonical_root\" \"$canonical_workspace\"\n"
    ));

    script
}

fn remote_before_remove_script(workspace: &Path, command: &str) -> String {
    format!(
        "{}\nif [ -d \"$workspace\" ]; then\n  cd \"$workspace\"\n  {}\nfi",
        remote_workspace_assignment(workspace),
        command
    )
}

fn remote_workspace_assignment(workspace: &Path) -> String {
    format!(
        "workspace={}\ncase \"$workspace\" in\n  '~') workspace=\"$HOME\" ;;\n  '~/'*) workspace=\"$HOME/${{workspace#~/}}\" ;;\nesac",
        shell_escape(workspace)
    )
}

fn shell_escape(path: &Path) -> String {
    shell_escape_raw(&path.to_string_lossy())
}

fn shell_escape_raw(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn handle_hook_output(output: hooks::CommandOutput, hook: HookName) -> Result<(), WorkspaceError> {
    if output.status == 0 {
        return Ok(());
    }

    Err(WorkspaceError::HookFailed {
        hook,
        status: output.status,
        output: output.output,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        parse_remote_workspace_output, remote_before_remove_script, workspace_key, WorkspaceError,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn parses_remote_workspace_marker_line_when_path_matches_root_and_key() {
        let output = "noise\n__SYMPHONY_WORKSPACE__\t1\t/tmp/remote\t/tmp/remote/workspace\n";

        let parsed = parse_remote_workspace_output(output, Path::new("/tmp/remote/workspace"))
            .expect("marker output should parse");

        assert!(parsed.created_now);
        assert_eq!(parsed.path, PathBuf::from("/tmp/remote/workspace"));
        assert_eq!(parsed.workspace_key, workspace_key("workspace"));
    }

    #[test]
    fn rejects_remote_workspace_marker_line_when_canonical_path_escapes_root() {
        let output = "noise\n__SYMPHONY_WORKSPACE__\t1\t/tmp/remote\t/\n";

        let error = parse_remote_workspace_output(output, Path::new("/tmp/remote/workspace"))
            .expect_err("escaped canonical path should be rejected");

        match error {
            WorkspaceError::InvalidRemoteWorkspacePath { path, .. } => {
                assert_eq!(path, "/");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn remote_before_remove_script_guards_missing_directories() {
        let script =
            remote_before_remove_script(Path::new("/tmp/remote/workspace"), "echo cleanup");

        assert!(script.contains("if [ -d \"$workspace\" ]; then"));
        assert!(script.contains("  cd \"$workspace\""));
        assert!(script.contains("  echo cleanup"));
        assert!(script.contains("fi"));
    }
}
