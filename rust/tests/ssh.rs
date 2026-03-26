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
    let test_root = unique_temp_dir("symphony-rust-ssh-escape");
    let fake_bin_dir = test_root.join("bin");
    let fake_ssh = fake_bin_dir.join("ssh");
    let trace_file = test_root.join("ssh.trace");
    let previous_path = std::env::var_os("PATH");

    std::fs::create_dir_all(&fake_bin_dir).expect("fake ssh bin dir should be created");
    std::fs::write(
        &fake_ssh,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nexit 0\n",
            trace_file.display()
        ),
    )
    .expect("fake ssh script should be written");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(&fake_ssh)
            .expect("fake ssh metadata should be readable")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_ssh, permissions).expect("fake ssh should be executable");
    }

    let path_prefix = fake_bin_dir.display().to_string();
    let updated_path = match &previous_path {
        Some(path) => format!("{path_prefix}:{}", path.to_string_lossy()),
        None => path_prefix,
    };
    std::env::set_var("PATH", updated_path);

    let output = run_ssh_command("localhost", "printf 'a'\nprintf \"don'\"\"t\"", None)
        .await
        .expect("ssh command should complete via fake ssh");

    match previous_path {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }

    let trace = std::fs::read_to_string(&trace_file).expect("trace file should be readable");
    std::fs::remove_dir_all(&test_root).expect("fake ssh dir should be removed");

    assert_eq!(output.status_code, 0);
    assert_eq!(
        trace.trim(),
        "-T localhost bash -lc 'printf '\\''a'\\''\nprintf \"don'\\''\"\"t\"'"
    );
}

#[tokio::test]
async fn ssh_commands_return_output_and_nonzero_exit_status() {
    let test_root = unique_temp_dir("symphony-rust-ssh-fake");
    let fake_bin_dir = test_root.join("bin");
    let fake_ssh = fake_bin_dir.join("ssh");
    let previous_path = std::env::var_os("PATH");

    std::fs::create_dir_all(&fake_bin_dir).expect("fake ssh bin dir should be created");
    std::fs::write(
        &fake_ssh,
        "#!/bin/sh\nprintf 'remote-out'\nprintf 'remote-err' >&2\nexit 7\n",
    )
    .expect("fake ssh script should be written");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(&fake_ssh)
            .expect("fake ssh metadata should be readable")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_ssh, permissions).expect("fake ssh should be executable");
    }

    let path_prefix = fake_bin_dir.display().to_string();
    let updated_path = match &previous_path {
        Some(path) => format!("{path_prefix}:{}", path.to_string_lossy()),
        None => path_prefix,
    };
    std::env::set_var("PATH", updated_path);

    let result = run_ssh_command("localhost", "printf ignored", None).await;

    match previous_path {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }
    std::fs::remove_dir_all(&test_root).expect("fake ssh dir should be removed");

    let output = result.expect("ssh execution should return command output even on nonzero exit");
    assert_eq!(output.status_code, 7);
    assert_eq!(output.stdout, "remote-out");
    assert_eq!(output.stderr, "remote-err");
}
