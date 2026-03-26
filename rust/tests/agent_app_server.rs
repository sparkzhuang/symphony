use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use symphony_rust::agent::{
    AppServerClient, AppServerConfig, AppServerError, AppServerEventKind,
    NonInteractiveDynamicToolExecutor,
};
use symphony_rust::config::{CodexConfig, TrackerConfig};
use symphony_rust::types::{Issue, IssueId, IssueIdentifier};
use tokio::time::{timeout, Duration};

fn unique_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();

    std::env::temp_dir().join(format!("symphony-rust-{name}-{nanos}"))
}

fn sample_issue() -> Issue {
    Issue {
        id: IssueId::new("issue-app-server"),
        identifier: IssueIdentifier::new("SPA-14"),
        title: "Codex App Server".to_owned(),
        description: Some("Exercise the app-server client".to_owned()),
        priority: Some(1),
        state: "In Progress".to_owned(),
        branch_name: None,
        url: None,
        labels: vec!["module-7".to_owned()],
        blocked_by: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

#[tokio::test]
async fn app_server_completes_handshake_and_handles_stream_requests() {
    let root = unique_path("agent-app-server");
    let workspace = root.join("workspace");
    let trace_file = root.join("trace.log");
    let fake_codex = root.join("fake-codex.sh");
    fs::create_dir_all(&workspace).expect("workspace should exist");

    fs::write(
        &fake_codex,
        format!(
            r#"#!/bin/sh
trace_file="{trace}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-14"}}}}}}'
      ;;
    4)
      printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'
      printf '%s\n' '{{"id":"approval-1","method":"item/commandExecution/requestApproval","params":{{"command":"cargo test"}}}}'
      printf '%s\n' '{{"id":"input-1","method":"item/tool/requestUserInput","params":{{"questions":[{{"id":"freeform","question":"Need input","options":null}}]}}}}'
      printf '%s\n' '{{"id":"tool-1","method":"item/tool/call","params":{{"tool":"unsupported_tool","arguments":{{}}}}}}'
      printf '%s\n' '{{"method":"turn/completed","params":{{"usage":{{"inputTokens":7,"outputTokens":11,"totalTokens":18}},"rateLimits":{{"primary":{{"remaining":42}}}}}}}}'
      ;;
    *)
      exit 0
      ;;
  esac
done
"#,
            trace = trace_file.display()
        ),
    )
    .expect("script should be written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fake_codex)
            .expect("metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_codex, perms).expect("script should be executable");
    }

    let mut codex = CodexConfig::default();
    codex.command = fake_codex.display().to_string();
    codex.approval_policy = Some(json!("never"));

    let tracker = TrackerConfig {
        kind: Some("linear".to_owned()),
        api_key: Some("token".to_owned()),
        ..TrackerConfig::default()
    };

    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let events = Arc::clone(&events);
        Arc::new(move |event| {
            events.lock().expect("events mutex").push(event);
        })
    };

    let executor = NonInteractiveDynamicToolExecutor::new(tracker.clone());
    let mut client = AppServerClient::new(
        AppServerConfig::from_codex_config(&codex),
        tracker,
        executor,
    );
    let mut session = client
        .start_session(&workspace, Some(sink))
        .await
        .expect("session should start");
    let outcome = client
        .run_turn(&mut session, "Solve SPA-14", &sample_issue())
        .await
        .expect("turn should complete");

    assert_eq!(outcome.thread_id, "thread-14");
    assert_eq!(outcome.turn_id, "turn-1");
    assert_eq!(outcome.session_id.as_str(), "thread-14-turn-1");
    assert_eq!(
        outcome.usage,
        Some(json!({"inputTokens": 7, "outputTokens": 11, "totalTokens": 18}))
    );
    assert_eq!(
        outcome.rate_limits,
        Some(json!({"primary": {"remaining": 42}}))
    );

    let trace = fs::read_to_string(&trace_file).expect("trace should exist");
    assert!(trace.contains(r#""method":"initialize""#));
    assert!(trace.contains(r#""method":"initialized""#));
    assert!(trace.contains(r#""method":"thread/start""#));
    assert!(trace.contains(r#""method":"turn/start""#));
    let emitted = events.lock().expect("events mutex");
    assert!(emitted
        .iter()
        .any(|event| event.kind == AppServerEventKind::SessionStarted));
    assert!(emitted
        .iter()
        .any(|event| event.kind == AppServerEventKind::ApprovalResolved));
    assert!(emitted
        .iter()
        .any(|event| event.kind == AppServerEventKind::ToolInputResolved));
    assert!(emitted
        .iter()
        .any(|event| event.kind == AppServerEventKind::UnsupportedToolCall));
    assert!(emitted
        .iter()
        .any(|event| event.kind == AppServerEventKind::TurnCompleted));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn app_server_auto_approves_tool_input_approval_prompts_when_policy_is_never() {
    let root = unique_path("agent-app-server-approval-prompt");
    let workspace = root.join("workspace");
    let trace_file = root.join("trace.log");
    let fake_codex = root.join("fake-codex.sh");
    fs::create_dir_all(&workspace).expect("workspace should exist");

    fs::write(
        &fake_codex,
        format!(
            r#"#!/bin/sh
trace_file="{trace}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-717"}}}}}}'
      ;;
    4)
      printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"turn-717"}}}}}}'
      printf '%s\n' '{{"id":"input-approval","method":"item/tool/requestUserInput","params":{{"questions":[{{"id":"mcp_tool_call_approval_call-717","question":"Allow?","options":[{{"label":"Approve Once","description":"Run the tool and continue."}},{{"label":"Approve this Session","description":"Run the tool and remember this choice for this session."}},{{"label":"Deny","description":"Decline this tool call and continue."}},{{"label":"Cancel","description":"Cancel this tool call."}}]}}]}}}}'
      ;;
    5)
      printf '%s\n' '{{"method":"turn/completed"}}'
      exit 0
      ;;
    *)
      exit 0
      ;;
  esac
done
"#,
            trace = trace_file.display()
        ),
    )
    .expect("script should be written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fake_codex)
            .expect("metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_codex, perms).expect("script should be executable");
    }

    let mut codex = CodexConfig::default();
    codex.command = fake_codex.display().to_string();
    codex.approval_policy = Some(json!("never"));

    let tracker = TrackerConfig {
        kind: Some("linear".to_owned()),
        api_key: Some("token".to_owned()),
        ..TrackerConfig::default()
    };

    let executor = NonInteractiveDynamicToolExecutor::new(tracker.clone());
    let mut client = AppServerClient::new(
        AppServerConfig::from_codex_config(&codex),
        tracker,
        executor,
    );
    let mut session = client
        .start_session(&workspace, None)
        .await
        .expect("session should start");

    client
        .run_turn(&mut session, "Solve SPA-14", &sample_issue())
        .await
        .expect("turn should complete");

    let trace = fs::read_to_string(&trace_file).expect("trace should exist");
    assert!(trace.contains(r#""id":"input-approval""#));
    assert!(
        trace.contains(r#""Approve this Session""#),
        "expected approval prompt to be answered with session approval, trace:\n{trace}"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn app_server_enforces_total_turn_timeout_even_when_events_continue_streaming() {
    let root = unique_path("agent-app-server-turn-timeout");
    let workspace = root.join("workspace");
    let trace_file = root.join("trace.log");
    let fake_codex = root.join("fake-codex.sh");
    fs::create_dir_all(&workspace).expect("workspace should exist");

    fs::write(
        &fake_codex,
        format!(
            r#"#!/bin/sh
trace_file="{trace}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-timeout"}}}}}}'
      ;;
    4)
      printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"turn-timeout"}}}}}}'
      while true; do
        printf '%s\n' '{{"method":"turn/updated","params":{{"status":"still-running"}}}}'
        sleep 0.05
      done
      ;;
    *)
      exit 0
      ;;
  esac
done
"#,
            trace = trace_file.display()
        ),
    )
    .expect("script should be written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fake_codex)
            .expect("metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_codex, perms).expect("script should be executable");
    }

    let mut codex = CodexConfig::default();
    codex.command = fake_codex.display().to_string();
    codex.turn_timeout_ms = 200;
    codex.read_timeout_ms = 5_000;
    codex.approval_policy = Some(json!("never"));

    let tracker = TrackerConfig {
        kind: Some("linear".to_owned()),
        api_key: Some("token".to_owned()),
        ..TrackerConfig::default()
    };
    let executor = NonInteractiveDynamicToolExecutor::new(tracker.clone());
    let mut client = AppServerClient::new(
        AppServerConfig::from_codex_config(&codex),
        tracker,
        executor,
    );
    let mut session = client
        .start_session(&workspace, None)
        .await
        .expect("session should start");

    let result = timeout(
        Duration::from_millis(700),
        client.run_turn(&mut session, "Solve SPA-14", &sample_issue()),
    )
    .await
    .expect("run_turn should finish before the outer timeout");

    assert!(matches!(result, Err(AppServerError::TurnTimeout(200))));

    let _ = fs::remove_dir_all(root);
}
