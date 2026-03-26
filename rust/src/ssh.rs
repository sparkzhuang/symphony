use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub status_code: i32,
}

#[derive(Debug, Error)]
pub enum ShellExecutionError {
    #[error("local command timed out after {timeout:?}: {command}")]
    TimedOut { command: String, timeout: Duration },
    #[error("failed to start local command `{command}`")]
    LocalSpawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to start ssh command on `{host}`")]
    SshSpawn {
        host: String,
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("ssh executable not found in PATH")]
    SshNotFound,
    #[error("ssh command failed on `{host}` with status {status_code}")]
    SshFailed {
        host: String,
        command: String,
        status_code: i32,
        stdout: String,
        stderr: String,
    },
}

impl ShellExecutionError {
    pub fn command(&self) -> Option<&str> {
        match self {
            Self::TimedOut { command, .. }
            | Self::LocalSpawn { command, .. }
            | Self::SshSpawn { command, .. }
            | Self::SshFailed { command, .. } => Some(command.as_str()),
            Self::SshNotFound => None,
        }
    }
}

pub async fn run_local_command(
    command: &str,
    cwd: Option<&Path>,
    timeout: Option<Duration>,
) -> Result<ShellOutput, ShellExecutionError> {
    let mut process = Command::new("bash");
    process
        .arg("-lc")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process.kill_on_drop(true);

    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }

    execute_command(process, command, timeout, local_spawn_error).await
}

pub async fn run_ssh_command(
    host: &str,
    command: &str,
    timeout: Option<Duration>,
) -> Result<ShellOutput, ShellExecutionError> {
    let executable = find_ssh_executable()?;
    let remote_command = remote_shell_command(command);
    let target = parse_target(host);

    let mut process = Command::new(executable);
    process
        .args(ssh_args(
            &target.destination,
            target.port.as_deref(),
            &remote_command,
        ))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process.kill_on_drop(true);

    execute_command(process, &remote_command, timeout, |source, command| {
        ShellExecutionError::SshSpawn {
            host: host.to_owned(),
            command,
            source,
        }
    })
    .await
    .and_then(|output| {
        if output.status_code == 0 {
            Ok(output)
        } else {
            Err(ShellExecutionError::SshFailed {
                host: host.to_owned(),
                command: remote_command,
                status_code: output.status_code,
                stdout: output.stdout,
                stderr: output.stderr,
            })
        }
    })
}

async fn execute_command<F>(
    mut process: Command,
    command: &str,
    timeout: Option<Duration>,
    spawn_error: F,
) -> Result<ShellOutput, ShellExecutionError>
where
    F: Fn(std::io::Error, String) -> ShellExecutionError,
{
    let child = process
        .spawn()
        .map_err(|source| spawn_error(source, command.to_owned()))?;

    let output = if let Some(timeout) = timeout {
        tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| ShellExecutionError::TimedOut {
                command: command.to_owned(),
                timeout,
            })?
            .map_err(|source| spawn_error(source, command.to_owned()))?
    } else {
        child
            .wait_with_output()
            .await
            .map_err(|source| spawn_error(source, command.to_owned()))?
    };

    Ok(ShellOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status_code: exit_status_code(output.status.code()),
    })
}

fn local_spawn_error(source: std::io::Error, command: String) -> ShellExecutionError {
    ShellExecutionError::LocalSpawn { command, source }
}

fn find_ssh_executable() -> Result<String, ShellExecutionError> {
    let executable = std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|path| path.join("ssh"))
                .find(|candidate| candidate.is_file())
        })
        .ok_or(ShellExecutionError::SshNotFound)?;

    Ok(executable.to_string_lossy().into_owned())
}

fn ssh_args(destination: &str, port: Option<&str>, command: &str) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(config_path) = std::env::var_os("SYMPHONY_SSH_CONFIG")
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string_lossy().into_owned())
    {
        args.push("-F".to_owned());
        args.push(config_path);
    }

    args.push("-T".to_owned());

    if let Some(port) = port {
        args.push("-p".to_owned());
        args.push(port.to_owned());
    }

    args.push(destination.to_owned());
    args.push(command.to_owned());
    args
}

fn remote_shell_command(command: &str) -> String {
    format!("bash -lc {}", shell_escape(command))
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

fn exit_status_code(status_code: Option<i32>) -> i32 {
    status_code.unwrap_or(-1)
}

struct ParsedTarget {
    destination: String,
    port: Option<String>,
}

fn parse_target(target: &str) -> ParsedTarget {
    let trimmed = target.trim();

    if let Some((destination, port)) = trimmed.rsplit_once(':') {
        if is_valid_port_destination(destination) && port.chars().all(|ch| ch.is_ascii_digit()) {
            return ParsedTarget {
                destination: destination.to_owned(),
                port: Some(port.to_owned()),
            };
        }
    }

    ParsedTarget {
        destination: trimmed.to_owned(),
        port: None,
    }
}

fn is_valid_port_destination(destination: &str) -> bool {
    !destination.is_empty() && (!destination.contains(':') || is_bracketed_host(destination))
}

fn is_bracketed_host(destination: &str) -> bool {
    destination.contains('[') && destination.contains(']')
}

#[cfg(test)]
mod tests {
    use super::{parse_target, remote_shell_command};

    #[test]
    fn builds_remote_bash_command_with_shell_escape() {
        assert_eq!(
            remote_shell_command("echo 'hello'"),
            "bash -lc 'echo '\\''hello'\\'''"
        );
    }

    #[test]
    fn parses_host_port_shorthand() {
        let parsed = parse_target("worker.example.com:2222");

        assert_eq!(parsed.destination, "worker.example.com");
        assert_eq!(parsed.port.as_deref(), Some("2222"));
    }

    #[test]
    fn preserves_ipv6_literal_without_port_when_unbracketed() {
        let parsed = parse_target("::1");

        assert_eq!(parsed.destination, "::1");
        assert!(parsed.port.is_none());
    }
}
