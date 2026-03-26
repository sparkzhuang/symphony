use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use symphony_rust::types::WorkspaceHooks;
use symphony_rust::workspace::{
    parse_remote_workspace_output, HookName, PathSafetyError, WorkspaceError, WorkspaceManager,
};

fn unique_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();

    std::env::temp_dir().join(format!("symphony-rust-{name}-{nanos}"))
}

#[tokio::test]
async fn create_workspace_runs_after_create_only_for_new_directory() {
    let root = unique_path("after-create-root");
    let marker = unique_path("after-create-marker");

    let hooks = WorkspaceHooks {
        after_create: Some(format!("printf created >> {}", marker.display())),
        ..WorkspaceHooks::default()
    };

    let manager = WorkspaceManager::new(&root, hooks).expect("manager should be created");

    let first = manager
        .create_for_issue("SPA-12 / path safety", None)
        .await
        .expect("workspace should be created");
    let second = manager
        .create_for_issue("SPA-12 / path safety", None)
        .await
        .expect("workspace should be reused");

    assert!(first.created_now);
    assert!(!second.created_now);
    assert_eq!(first.path, second.path);

    let content = fs::read_to_string(&marker).expect("marker should exist");
    assert_eq!(content, "created");

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(marker);
}

#[tokio::test]
async fn create_workspace_replaces_existing_non_directory_path() {
    let root = unique_path("replace-file-root");
    fs::create_dir_all(&root).expect("root should exist");

    let manager =
        WorkspaceManager::new(&root, WorkspaceHooks::default()).expect("manager should be created");
    let target = root.join("SPA-12");
    fs::write(&target, "not a directory").expect("file should exist");

    let workspace = manager
        .create_for_issue("SPA-12", None)
        .await
        .expect("workspace should be recreated as a directory");

    assert!(workspace.created_now);
    assert!(workspace.path.is_dir());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn before_run_failure_blocks_agent_start() {
    let root = unique_path("before-run-root");
    let manager = WorkspaceManager::new(
        &root,
        WorkspaceHooks {
            before_run: Some("exit 7".to_owned()),
            ..WorkspaceHooks::default()
        },
    )
    .expect("manager should be created");

    let workspace = manager
        .create_for_issue("SPA-12", None)
        .await
        .expect("workspace should be created");
    let error = manager
        .run_hook(HookName::BeforeRun, &workspace.path, "SPA-12", None)
        .await
        .expect_err("before_run should fail");

    match error {
        WorkspaceError::HookFailed { hook, status, .. } => {
            assert_eq!(hook, HookName::BeforeRun);
            assert_eq!(status, 7);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn after_run_failure_is_logged_and_ignored() {
    let root = unique_path("after-run-root");
    let manager = WorkspaceManager::new(
        &root,
        WorkspaceHooks {
            after_run: Some("exit 9".to_owned()),
            ..WorkspaceHooks::default()
        },
    )
    .expect("manager should be created");

    let workspace = manager
        .create_for_issue("SPA-12", None)
        .await
        .expect("workspace should be created");

    manager
        .run_after_run_hook(&workspace.path, "SPA-12", None)
        .await
        .expect("after_run failures should be ignored");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn before_remove_runs_before_deletion() {
    let root = unique_path("before-remove-root");
    let marker = unique_path("before-remove-marker");
    let manager = WorkspaceManager::new(
        &root,
        WorkspaceHooks {
            before_remove: Some(format!("printf before-remove > {}", marker.display())),
            ..WorkspaceHooks::default()
        },
    )
    .expect("manager should be created");

    let workspace = manager
        .create_for_issue("SPA-12", None)
        .await
        .expect("workspace should be created");

    manager
        .remove(&workspace.path, None)
        .await
        .expect("workspace should be removed");

    assert!(!workspace.path.exists());
    assert_eq!(
        fs::read_to_string(&marker).expect("before_remove marker should exist"),
        "before-remove"
    );

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(marker);
}

#[tokio::test]
async fn hook_timeout_returns_explicit_error() {
    let root = unique_path("timeout-root");
    let manager = WorkspaceManager::new(
        &root,
        WorkspaceHooks {
            before_run: Some("sleep 1".to_owned()),
            timeout_ms: Some(10),
            ..WorkspaceHooks::default()
        },
    )
    .expect("manager should be created");

    let workspace = manager
        .create_for_issue("SPA-12", None)
        .await
        .expect("workspace should be created");
    let error = manager
        .run_hook(HookName::BeforeRun, &workspace.path, "SPA-12", None)
        .await
        .expect_err("hook should time out");

    match error {
        WorkspaceError::HookTimeout { hook, timeout_ms } => {
            assert_eq!(hook, HookName::BeforeRun);
            assert_eq!(timeout_ms, 10);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn remove_issue_workspaces_removes_local_workspace() {
    let root = unique_path("remove-issue-root");
    let manager =
        WorkspaceManager::new(&root, WorkspaceHooks::default()).expect("manager should be created");
    let workspace = manager
        .create_for_issue("SPA-12 / with spaces", None)
        .await
        .expect("workspace should be created");

    manager
        .remove_issue_workspaces("SPA-12 / with spaces")
        .await;

    assert!(!workspace.path.exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn parses_remote_workspace_marker_line() {
    let output = "noise\n__SYMPHONY_WORKSPACE__\t1\t/tmp/remote/workspace\n";

    let parsed = parse_remote_workspace_output(output).expect("marker output should parse");

    assert!(parsed.created_now);
    assert_eq!(parsed.path, PathBuf::from("/tmp/remote/workspace"));
}

#[cfg(unix)]
#[tokio::test]
async fn create_workspace_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let root = unique_path("symlink-root");
    let outside = unique_path("symlink-outside");
    fs::create_dir_all(&root).expect("root should exist");
    fs::create_dir_all(&outside).expect("outside should exist");
    symlink(&outside, root.join("escaped")).expect("symlink should be created");

    let manager =
        WorkspaceManager::new(&root, WorkspaceHooks::default()).expect("manager should be created");
    let error = manager
        .create_for_issue("escaped", None)
        .await
        .expect_err("symlink escape should be rejected");

    match error {
        WorkspaceError::PathSafety(PathSafetyError::SymlinkEscape { .. }) => {}
        other => panic!("unexpected error: {other:?}"),
    }

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(outside);
}
