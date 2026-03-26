use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use symphony_rust::agent::{AgentRunner, AppServerEventKind};
use symphony_rust::config::{HooksConfig, TrackerConfig, WorkflowConfig};
use symphony_rust::tracker::{Tracker, TrackerError, TrackerFuture};
use symphony_rust::types::{Issue, IssueId, IssueIdentifier, WorkflowDefinition};
use tokio::time::{sleep, Duration};

fn unique_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();

    std::env::temp_dir().join(format!("symphony-rust-{name}-{nanos}"))
}

fn sample_issue() -> Issue {
    Issue {
        id: IssueId::new("issue-runner"),
        identifier: IssueIdentifier::new("SPA-14"),
        title: "Runner".to_owned(),
        description: Some("Run multiple turns".to_owned()),
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

#[derive(Clone)]
struct ScriptedTracker {
    issue: Issue,
    states: Arc<Mutex<Vec<String>>>,
}

impl Tracker for ScriptedTracker {
    fn fetch_candidate_issues(&self) -> TrackerFuture<'_, Vec<Issue>> {
        Box::pin(async move { Ok(vec![self.issue.clone()]) })
    }

    fn fetch_issues_by_states<'a>(
        &'a self,
        _state_names: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        Box::pin(async move { Ok(vec![self.issue.clone()]) })
    }

    fn fetch_issue_states_by_ids<'a>(
        &'a self,
        _issue_ids: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        Box::pin(async move {
            let mut issue = self.issue.clone();
            let state = self.states.lock().expect("states mutex").remove(0);
            issue.state = state;
            Ok(vec![issue])
        })
    }

    fn create_comment<'a>(&'a self, _issue_id: &'a str, _body: &'a str) -> TrackerFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn update_issue_state<'a>(
        &'a self,
        _issue_id: &'a str,
        _state_name: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }
}

#[derive(Clone)]
struct FailingRefreshTracker {
    issue: Issue,
}

impl Tracker for FailingRefreshTracker {
    fn fetch_candidate_issues(&self) -> TrackerFuture<'_, Vec<Issue>> {
        Box::pin(async move { Ok(vec![self.issue.clone()]) })
    }

    fn fetch_issues_by_states<'a>(
        &'a self,
        _state_names: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        Box::pin(async move { Ok(vec![self.issue.clone()]) })
    }

    fn fetch_issue_states_by_ids<'a>(
        &'a self,
        _issue_ids: &'a [String],
    ) -> TrackerFuture<'a, Vec<Issue>> {
        Box::pin(async move { Err(TrackerError::StateNotFound) })
    }

    fn create_comment<'a>(&'a self, _issue_id: &'a str, _body: &'a str) -> TrackerFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn update_issue_state<'a>(
        &'a self,
        _issue_id: &'a str,
        _state_name: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }
}

#[tokio::test]
async fn runner_executes_hooks_and_continuation_turns() {
    let root = unique_path("agent-runner");
    let workspace_root = root.join("workspaces");
    let before_marker = root.join("before.log");
    let after_marker = root.join("after.log");
    let trace_file = root.join("trace.log");
    let fake_codex = root.join("fake-codex.sh");
    fs::create_dir_all(&workspace_root).expect("workspace root should exist");

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
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-runner"}}}}}}'
      ;;
    3)
      ;;
    4)
      printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'
      printf '%s\n' '{{"method":"turn/completed"}}'
      ;;
    5)
      printf '%s\n' '{{"id":4,"result":{{"turn":{{"id":"turn-2"}}}}}}'
      printf '%s\n' '{{"method":"turn/completed"}}'
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

    let workflow = WorkflowDefinition::new(
        serde_json::json!({}),
        "Issue {{ issue.identifier }}: {{ issue.title }}",
    );
    let mut config =
        WorkflowConfig::from_value(serde_json::json!({})).expect("config should parse");
    config.workspace.root = workspace_root.clone();
    config.hooks = HooksConfig {
        before_run: Some(format!("printf before > {}", before_marker.display())),
        after_run: Some(format!("printf after > {}", after_marker.display())),
        timeout_ms: 5_000,
        ..HooksConfig::default()
    };
    config.codex.command = fake_codex.display().to_string();
    config.agent.max_turns = 3;
    config.tracker = TrackerConfig {
        kind: Some("linear".to_owned()),
        api_key: Some("token".to_owned()),
        ..TrackerConfig::default()
    };

    let tracker = Arc::new(ScriptedTracker {
        issue: sample_issue(),
        states: Arc::new(Mutex::new(vec![
            "In Progress".to_owned(),
            "Done".to_owned(),
        ])),
    });

    let events = Arc::new(Mutex::new(Vec::new()));
    let runner = AgentRunner::new(workflow, config, tracker).expect("runner should be created");
    let result = runner
        .run(
            sample_issue(),
            Some({
                let events = Arc::clone(&events);
                Arc::new(move |event| {
                    events.lock().expect("events mutex").push(event);
                })
            }),
        )
        .await
        .expect("runner should succeed");

    assert_eq!(result.turn_count, 2);
    assert!(result.workspace.path.ends_with("SPA-14"));
    assert_eq!(
        fs::read_to_string(&before_marker).expect("before marker"),
        "before"
    );
    assert_eq!(
        fs::read_to_string(&after_marker).expect("after marker"),
        "after"
    );

    let trace = fs::read_to_string(&trace_file).expect("trace should exist");
    let turn_start_count = trace.matches(r#""method":"turn/start""#).count();
    assert_eq!(turn_start_count, 2);
    assert!(trace.contains("Continuation guidance"));

    let emitted = events.lock().expect("events mutex");
    assert!(
        emitted
            .iter()
            .filter(|event| event.kind == AppServerEventKind::TurnCompleted)
            .count()
            >= 2
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runner_stops_app_server_when_state_refresh_fails() {
    let root = unique_path("agent-runner-cleanup");
    let workspace_root = root.join("workspaces");
    let after_marker = root.join("after.log");
    let pid_file = root.join("codex.pid");
    let fake_codex = root.join("fake-codex.sh");
    fs::create_dir_all(&workspace_root).expect("workspace root should exist");

    fs::write(
        &fake_codex,
        format!(
            r#"#!/bin/sh
printf '%s' "$$" > "{pid_file}"
IFS= read -r _line
printf '%s\n' '{{"id":1,"result":{{}}}}'
IFS= read -r _line
IFS= read -r _line
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-cleanup"}}}}}}'
IFS= read -r _line
printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"turn-cleanup"}}}}}}'
printf '%s\n' '{{"method":"turn/completed"}}'
while true; do
  sleep 0.1
done
"#,
            pid_file = pid_file.display()
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

    let workflow = WorkflowDefinition::new(
        serde_json::json!({}),
        "Issue {{ issue.identifier }}: {{ issue.title }}",
    );
    let mut config =
        WorkflowConfig::from_value(serde_json::json!({})).expect("config should parse");
    config.workspace.root = workspace_root.clone();
    config.hooks = HooksConfig {
        after_run: Some(format!("printf after > {}", after_marker.display())),
        timeout_ms: 5_000,
        ..HooksConfig::default()
    };
    config.codex.command = fake_codex.display().to_string();
    config.agent.max_turns = 3;

    let tracker = Arc::new(FailingRefreshTracker {
        issue: sample_issue(),
    });

    let runner = AgentRunner::new(workflow, config, tracker).expect("runner should be created");
    let result = runner.run(sample_issue(), None).await;

    assert!(result.is_err(), "runner should fail when refresh fails");
    assert_eq!(
        fs::read_to_string(&after_marker).expect("after marker"),
        "after"
    );

    let pid = fs::read_to_string(&pid_file).expect("fake codex should record its pid");

    let mut stopped = false;
    for _ in 0..10 {
        let status = std::process::Command::new("kill")
            .arg("-0")
            .arg(&pid)
            .status()
            .expect("kill -0 should run");
        if !status.success() {
            stopped = true;
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(
        stopped,
        "expected app-server process {pid} to be terminated"
    );

    let _ = fs::remove_dir_all(root);
}
