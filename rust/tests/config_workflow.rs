use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use symphony_rust::config::{
    parse_workflow, validate_dispatch_preflight, ConfigError, ConfigValueError, WorkflowConfig,
    WorkflowError, WorkflowStore,
};

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{unique}"))
}

fn write_workflow(path: &Path, content: &str) {
    fs::write(path, content).expect("workflow file should be written");
}

#[test]
fn parses_yaml_front_matter_and_prompt_body() {
    let workflow = parse_workflow(
        r#"---
tracker:
  kind: linear
  project_slug: SPA
---

# Prompt

Work on {{ issue.identifier }}.
"#,
    )
    .expect("workflow should parse");

    assert_eq!(
        workflow.prompt_template,
        "# Prompt\n\nWork on {{ issue.identifier }}."
    );
    assert_eq!(workflow.config["tracker"]["kind"], json!("linear"));
    assert_eq!(workflow.config["tracker"]["project_slug"], json!("SPA"));
}

#[test]
fn rejects_non_map_front_matter() {
    let error = parse_workflow(
        r#"---
- not
- a
- map
---
Prompt
"#,
    )
    .expect_err("workflow front matter must be a map");

    assert!(matches!(error, WorkflowError::FrontMatterNotAMap));
}

#[test]
fn reports_yaml_line_information_for_parse_errors() {
    let error = parse_workflow(
        r#"---
tracker:
  kind: linear
  project_slug: [broken
---
Prompt
"#,
    )
    .expect_err("invalid yaml should fail");

    match error {
        WorkflowError::Parse { line, .. } => assert!(line.is_some()),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn resolves_defaults_env_vars_paths_and_state_limits() {
    let home = unique_temp_dir("symphony-home");
    fs::create_dir_all(&home).expect("home dir should exist");
    let old_home = std::env::var_os("HOME");

    std::env::set_var("HOME", &home);
    std::env::set_var("LINEAR_TOKEN", "secret-token");

    let parsed = WorkflowConfig::from_value(json!({
        "tracker": {
            "kind": "linear",
            "api_key": "$LINEAR_TOKEN",
            "project_slug": "SPA",
            "assignee": "$MISSING_ASSIGNEE"
        },
        "workspace": {
            "root": "~/repo/workspaces"
        },
        "agent": {
            "max_concurrent_agents_by_state": {
                "In Progress": 2,
                "todo": 1
            }
        }
    }))
    .expect("config should parse");

    assert_eq!(parsed.polling.interval_ms, 30_000);
    assert_eq!(parsed.agent.max_concurrent_agents, 10);
    assert_eq!(parsed.agent.max_turns, 20);
    assert_eq!(parsed.codex.command, "codex app-server");
    assert_eq!(parsed.tracker.api_key.as_deref(), Some("secret-token"));
    assert_eq!(parsed.tracker.assignee, None);
    assert_eq!(parsed.workspace.root, home.join("repo/workspaces"));
    assert_eq!(
        parsed.agent.max_concurrent_agents_by_state,
        HashMap::from([
            ("in progress".to_owned(), 2_usize),
            ("todo".to_owned(), 1_usize),
        ])
    );

    restore_var("HOME", old_home);
    std::env::remove_var("LINEAR_TOKEN");
}

#[test]
fn returns_structured_validation_errors() {
    let error = WorkflowConfig::from_value(json!({
        "polling": {
            "interval_ms": 0
        },
        "codex": {
            "command": ""
        }
    }))
    .expect_err("invalid config should fail");

    match error {
        ConfigError::InvalidConfig(errors) => {
            assert!(errors.iter().any(|entry| {
                entry.path == "polling.interval_ms"
                    && matches!(entry.kind, ConfigValueError::MustBePositive)
            }));
            assert!(errors.iter().any(|entry| {
                entry.path == "codex.command" && matches!(entry.kind, ConfigValueError::Required)
            }));
        }
    }
}

#[test]
fn rejects_invalid_state_limit_entries() {
    let error = WorkflowConfig::from_value(json!({
        "agent": {
            "max_concurrent_agents_by_state": {
                "": 2,
                "Todo": 0,
                "In Progress": "abc"
            }
        }
    }))
    .expect_err("invalid state limits should fail validation");

    match error {
        ConfigError::InvalidConfig(errors) => {
            assert!(errors.iter().any(|entry| {
                entry.path == "agent.max_concurrent_agents_by_state."
                    && matches!(entry.kind, ConfigValueError::Required)
            }));
            assert!(errors.iter().any(|entry| {
                entry.path == "agent.max_concurrent_agents_by_state.Todo"
                    && matches!(entry.kind, ConfigValueError::MustBePositive)
            }));
            assert!(errors.iter().any(|entry| {
                entry.path == "agent.max_concurrent_agents_by_state.In Progress"
                    && matches!(entry.kind, ConfigValueError::ExpectedInteger)
            }));
        }
    }
}

#[test]
fn validates_dispatch_preflight_requirements() {
    let old_linear_token = std::env::var_os("LINEAR_API_KEY");
    std::env::remove_var("LINEAR_API_KEY");

    let config = WorkflowConfig::from_value(json!({
        "tracker": {
            "kind": "linear",
            "project_slug": "SPA"
        }
    }))
    .expect("schema parse should succeed");

    let errors = validate_dispatch_preflight(&config);

    assert!(errors.iter().any(|entry| entry.path == "tracker.api_key"));

    restore_var("LINEAR_API_KEY", old_linear_token);
}

#[test]
fn hot_reload_keeps_last_good_snapshot_when_reload_fails() {
    let temp_dir = unique_temp_dir("workflow-store");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let workflow_path = temp_dir.join("WORKFLOW.md");

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: SPA
---
First prompt
"#,
    );

    let mut store = WorkflowStore::load(&workflow_path).expect("initial workflow should load");
    let initial = store.current().clone();

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: [broken
---
Broken prompt
"#,
    );

    let reload = store.reload_if_changed();

    assert!(matches!(reload, Err(WorkflowError::Parse { .. })));
    assert_eq!(store.current(), &initial);

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: SPA
polling:
  interval_ms: 1500
---
Second prompt
"#,
    );

    store
        .reload_if_changed()
        .expect("valid workflow should reload");

    assert_eq!(store.current().workflow.prompt_template, "Second prompt");
    assert_eq!(store.current().config.polling.interval_ms, 1500);

    fs::remove_dir_all(&temp_dir).expect("temp dir should be removed");
}

#[tokio::test]
async fn async_reload_keeps_last_good_snapshot_when_reload_fails() {
    let temp_dir = unique_temp_dir("workflow-store-async");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let workflow_path = temp_dir.join("WORKFLOW.md");

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: SPA
---
Async prompt
"#,
    );

    let mut store = WorkflowStore::load(&workflow_path).expect("initial workflow should load");
    let initial = store.current().clone();

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: [broken
---
Broken async prompt
"#,
    );

    let reload = store.reload_if_changed_async().await;

    assert!(matches!(reload, Err(WorkflowError::Parse { .. })));
    assert_eq!(store.current(), &initial);

    fs::remove_dir_all(&temp_dir).expect("temp dir should be removed");
}

fn restore_var(name: &str, value: Option<std::ffi::OsString>) {
    match value {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}
