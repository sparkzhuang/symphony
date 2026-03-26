use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use symphony_rust::ssh::{run_local_command, run_ssh_command, ShellExecutionError};

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();

    std::env::temp_dir().join(format!("{prefix}-{nanos}"))
}

#[tokio::test]
async fn runs_local_commands_via_bash_with_configurable_cwd() {
    let cwd = unique_temp_dir("symphony-rust-ssh-local");
    std::fs::create_dir_all(&cwd).expect("temp cwd should be created");

    let output = run_local_command("pwd", Some(cwd.as_path()), None)
        .await
        .expect("local command should succeed");
    let canonical_cwd = std::fs::canonicalize(&cwd).expect("temp cwd should canonicalize");

    assert_eq!(output.status_code, 0);
    assert_eq!(output.stdout.trim(), canonical_cwd.display().to_string());
    assert!(output.stderr.is_empty());
    std::fs::remove_dir_all(&cwd).expect("temp cwd should be removed");
}

#[tokio::test]
async fn captures_stdout_stderr_and_exit_status_from_local_commands() {
    let output = run_local_command("printf 'hello'; printf 'oops' >&2; exit 7", None, None)
        .await
        .expect("local command should complete");

    assert_eq!(output.status_code, 7);
    assert_eq!(output.stdout, "hello");
    assert_eq!(output.stderr, "oops");
}

#[tokio::test]
async fn returns_timeout_errors_for_local_commands() {
    let error = run_local_command("sleep 5", None, Some(Duration::from_millis(50)))
        .await
        .expect_err("command should time out");

    assert!(matches!(error, ShellExecutionError::TimedOut { .. }));
}

#[tokio::test]
async fn ssh_commands_escape_single_quotes_for_remote_bash() {
    let error = run_ssh_command(
        "invalid-host-for-escape-test",
        "printf 'a'\nprintf \"don'\"\"t\"",
        Some(Duration::from_millis(50)),
    )
    .await
    .expect_err("invalid ssh destination should fail after argument construction");

    assert_eq!(
        error
            .command()
            .expect("ssh errors should retain the remote command"),
        "bash -lc 'printf '\\''a'\\''\nprintf \"don'\\''\"\"t\"'"
    );
}
