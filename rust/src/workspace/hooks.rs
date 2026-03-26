use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::workspace::WorkspaceError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookName {
    AfterCreate,
    BeforeRun,
    AfterRun,
    BeforeRemove,
}

impl HookName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AfterCreate => "after_create",
            Self::BeforeRun => "before_run",
            Self::AfterRun => "after_run",
            Self::BeforeRemove => "before_remove",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    pub(crate) output: String,
    pub(crate) status: i32,
}

pub(crate) async fn run_hook(
    command: &str,
    workspace: &Path,
    hook: HookName,
    timeout_ms: u64,
    worker_host: Option<&str>,
) -> Result<(), WorkspaceError> {
    info!(
        hook = hook.as_str(),
        workspace = %workspace.display(),
        worker_host = worker_host.unwrap_or("local"),
        "running workspace hook"
    );

    let output = match worker_host {
        Some(host) => {
            let script = format!("cd {} && {}", shell_escape(workspace), command);
            run_remote_command(host, &script, timeout_ms, hook).await?
        }
        None => run_local_command(command, workspace, timeout_ms, hook).await?,
    };

    if output.status == 0 {
        return Ok(());
    }

    warn!(
        hook = hook.as_str(),
        workspace = %workspace.display(),
        status = output.status,
        output = %truncate_output(&output.output),
        "workspace hook failed"
    );

    Err(WorkspaceError::HookFailed {
        hook,
        status: output.status,
        output: output.output,
    })
}

pub(crate) async fn run_remote_command(
    host: &str,
    script: &str,
    timeout_ms: u64,
    hook: HookName,
) -> Result<CommandOutput, WorkspaceError> {
    let mut command = Command::new("ssh");
    command
        .args(ssh_args(host, script))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    run_command(command, timeout_ms, hook).await
}

async fn run_local_command(
    command: &str,
    workspace: &Path,
    timeout_ms: u64,
    hook: HookName,
) -> Result<CommandOutput, WorkspaceError> {
    let mut process = Command::new("sh");
    process
        .arg("-lc")
        .arg(command)
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    run_command(process, timeout_ms, hook).await
}

async fn run_command(
    mut command: Command,
    timeout_ms: u64,
    hook: HookName,
) -> Result<CommandOutput, WorkspaceError> {
    let child = command
        .spawn()
        .map_err(|source| WorkspaceError::CommandSpawn {
            program: command
                .as_std()
                .get_program()
                .to_string_lossy()
                .into_owned(),
            source,
        })?;

    let output = timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
        .await
        .map_err(|_| WorkspaceError::HookTimeout { hook, timeout_ms })?
        .map_err(|source| WorkspaceError::CommandWait {
            program: command
                .as_std()
                .get_program()
                .to_string_lossy()
                .into_owned(),
            source,
        })?;

    Ok(CommandOutput {
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        status: output.status.code().unwrap_or(-1),
    })
}

fn ssh_args(host: &str, script: &str) -> Vec<String> {
    let target = host.trim();
    let mut args = Vec::new();

    if let Ok(config) = std::env::var("SYMPHONY_SSH_CONFIG") {
        if !config.trim().is_empty() {
            args.push("-F".to_owned());
            args.push(config);
        }
    }

    args.push("-T".to_owned());

    match parse_target(target) {
        Some((destination, port)) => {
            args.push("-p".to_owned());
            args.push(port);
            args.push(destination);
        }
        None => args.push(target.to_owned()),
    }

    args.push("bash".to_owned());
    args.push("-lc".to_owned());
    args.push(shell_escape_raw(script));
    args
}

fn parse_target(target: &str) -> Option<(String, String)> {
    let (destination, port) = target.rsplit_once(':')?;

    if destination.is_empty() || port.is_empty() || !port.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }

    if destination.contains(':') && !(destination.starts_with('[') && destination.ends_with(']')) {
        return None;
    }

    Some((destination.to_owned(), port.to_owned()))
}

fn shell_escape(path: &Path) -> String {
    shell_escape_raw(&path.to_string_lossy())
}

fn shell_escape_raw(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn truncate_output(output: &str) -> String {
    const MAX_BYTES: usize = 2_048;

    if output.len() <= MAX_BYTES {
        output.to_owned()
    } else {
        format!("{}... (truncated)", &output[..MAX_BYTES])
    }
}
