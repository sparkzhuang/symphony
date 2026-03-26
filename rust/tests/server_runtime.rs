use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use symphony_rust::server::bind_from_workflow;

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

#[tokio::test]
async fn workflow_config_port_starts_server_when_cli_port_is_absent() {
    let temp_dir = unique_temp_dir("server-runtime-config");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let workflow_path = temp_dir.join("WORKFLOW.md");

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: SPA
server:
  port: 0
---
Prompt
"#,
    );

    let server = bind_from_workflow(&workflow_path, None)
        .await
        .expect("workflow-configured server should bind")
        .expect("server should be enabled");

    let response = reqwest::get(format!("http://{}/healthz", server.local_addr()))
        .await
        .expect("health check should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(server.local_addr().ip().is_loopback());

    server.shutdown().await;
}

#[tokio::test]
async fn cli_port_override_takes_precedence_over_workflow_server_port() {
    let temp_dir = unique_temp_dir("server-runtime-cli");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let workflow_path = temp_dir.join("WORKFLOW.md");

    write_workflow(
        &workflow_path,
        r#"---
tracker:
  kind: linear
  api_key: literal-token
  project_slug: SPA
server:
  port: 4010
---
Prompt
"#,
    );

    let server = bind_from_workflow(&workflow_path, Some(0))
        .await
        .expect("cli override server should bind")
        .expect("server should be enabled");

    assert_ne!(server.local_addr().port(), 4010);

    let response = reqwest::get(format!("http://{}/healthz", server.local_addr()))
        .await
        .expect("health check should succeed");

    assert_eq!(response.status(), StatusCode::OK);

    server.shutdown().await;
}

#[tokio::test]
async fn disabled_server_returns_none_when_no_cli_or_config_port_is_set() {
    let temp_dir = unique_temp_dir("server-runtime-disabled");
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
Prompt
"#,
    );

    let server = tokio::time::timeout(
        Duration::from_secs(2),
        bind_from_workflow(&workflow_path, None),
    )
    .await
    .expect("server bind should not hang")
    .expect("workflow should load");

    assert!(server.is_none());
}
