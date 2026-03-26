use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use chrono::{Duration, TimeZone, Utc};
use serde_json::json;
use symphony_rust::dashboard::terminal::{sparkline, TerminalDashboard, TerminalDashboardConfig};
use symphony_rust::dashboard::{Dashboard, DashboardSink, RenderOutcome};
use symphony_rust::types::{
    BlockerRef, CodexTotals, Issue, IssueId, IssueIdentifier, LiveSession, OrchestratorState,
    RetryEntry, RunAttempt, RunStatus, RunningEntry,
};

#[test]
fn renders_running_sessions_retry_queue_global_stats_and_sparkline() {
    let now = Utc.with_ymd_and_hms(2026, 3, 26, 12, 0, 0).unwrap();
    let snapshot = sample_snapshot(now);
    let graph = sparkline(
        &[
            (now.timestamp_millis() as u64 - 500_000, 100),
            (now.timestamp_millis() as u64 - 420_000, 140),
            (now.timestamp_millis() as u64 - 300_000, 260),
            (now.timestamp_millis() as u64 - 180_000, 480),
            (now.timestamp_millis() as u64 - 60_000, 820),
        ],
        now.timestamp_millis() as u64,
        snapshot.codex_totals.total_tokens,
    );

    let frame = TerminalDashboard::<RecordingSink>::render_frame(
        &snapshot,
        now,
        now.timestamp_millis() as u64,
        &graph,
        115,
    );

    assert!(frame.contains("SYMPHONY STATUS"));
    assert!(frame.contains("Running Sessions"));
    assert!(frame.contains("Retry Queue"));
    assert!(frame.contains("Global Stats"));
    assert!(frame.contains("Throughput"));
    assert!(frame.contains("SPA-18"));
    assert!(frame.contains("StreamingTurn"));
    assert!(frame.contains("sess-main-001"));
    assert!(frame.contains("bootstrap completed"));
    assert!(frame.contains("attempt=2"));
    assert!(frame.contains("total 9,876"));
    assert!(frame.contains("7m 0s"));
    assert!(frame.contains("primary 12/20"));
    assert_eq!(graph.chars().count(), 24);
    assert!(graph.chars().any(|ch| ch != '▁'));
}

#[test]
fn adapts_running_event_width_to_terminal_columns() {
    let now = Utc.with_ymd_and_hms(2026, 3, 26, 12, 0, 0).unwrap();
    let mut snapshot = sample_snapshot(now);
    let running_entry = snapshot.running.values_mut().next().unwrap();
    running_entry.live_session.as_mut().unwrap().last_codex_message = Some(
        "this is a deliberately long event message that should be truncated when the terminal is narrow"
            .to_owned(),
    );

    let narrow = TerminalDashboard::<RecordingSink>::render_frame(
        &snapshot,
        now,
        now.timestamp_millis() as u64,
        "▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁",
        90,
    );
    let wide = TerminalDashboard::<RecordingSink>::render_frame(
        &snapshot,
        now,
        now.timestamp_millis() as u64,
        "▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁",
        140,
    );

    assert!(narrow.contains("..."));
    assert!(wide.len() > narrow.len());
}

#[test]
fn flushes_pending_updates_after_render_interval_without_new_snapshot() {
    let now = Utc.with_ymd_and_hms(2026, 3, 26, 12, 0, 0).unwrap();
    let mut snapshot = sample_snapshot(now);
    let sink = RecordingSink::tty();
    let config = TerminalDashboardConfig {
        render_interval_ms: 1_000,
        terminal_columns_override: Some(115),
        ..TerminalDashboardConfig::default()
    };
    let mut dashboard = TerminalDashboard::new(config, sink);

    let outcome_1 = dashboard
        .render_snapshot(&snapshot, now, now.timestamp_millis() as u64)
        .expect("first render should succeed");

    snapshot.codex_totals.total_tokens += 500;
    let outcome_2 = dashboard
        .render_snapshot(
            &snapshot,
            now + Duration::milliseconds(100),
            now.timestamp_millis() as u64 + 100,
        )
        .expect("changed frame should be queued");
    let outcome_3 = dashboard
        .flush_pending(now.timestamp_millis() as u64 + 1_000)
        .expect("queued frame should flush once interval elapses");

    assert_eq!(outcome_1, RenderOutcome::Rendered);
    assert_eq!(outcome_2, RenderOutcome::SkippedRateLimited);
    assert_eq!(outcome_3, RenderOutcome::Rendered);
    assert_eq!(dashboard.sink().frames.len(), 2);
}

#[test]
fn flush_pending_returns_rate_limited_until_interval_elapses() {
    let now = Utc.with_ymd_and_hms(2026, 3, 26, 12, 0, 0).unwrap();
    let mut snapshot = sample_snapshot(now);
    let sink = RecordingSink::tty();
    let config = TerminalDashboardConfig {
        render_interval_ms: 1_000,
        terminal_columns_override: Some(115),
        ..TerminalDashboardConfig::default()
    };
    let mut dashboard = TerminalDashboard::new(config, sink);

    dashboard
        .render_snapshot(&snapshot, now, now.timestamp_millis() as u64)
        .expect("first render should succeed");

    snapshot.codex_totals.total_tokens += 500;
    dashboard
        .render_snapshot(
            &snapshot,
            now + Duration::milliseconds(100),
            now.timestamp_millis() as u64 + 100,
        )
        .expect("changed frame should be queued");

    let outcome = dashboard
        .flush_pending(now.timestamp_millis() as u64 + 500)
        .expect("early flush should not fail");

    assert_eq!(outcome, RenderOutcome::SkippedRateLimited);
    assert_eq!(dashboard.sink().frames.len(), 1);
}

#[test]
fn disables_rendering_when_sink_is_not_a_tty() {
    let now = Utc.with_ymd_and_hms(2026, 3, 26, 12, 0, 0).unwrap();
    let snapshot = sample_snapshot(now);
    let mut dashboard =
        TerminalDashboard::new(TerminalDashboardConfig::default(), RecordingSink::non_tty());

    let outcome = dashboard
        .render_snapshot(&snapshot, now, now.timestamp_millis() as u64)
        .expect("disabled render should not fail");

    assert_eq!(outcome, RenderOutcome::Disabled);
    assert!(dashboard.sink().frames.is_empty());
}

#[test]
fn clamps_render_interval_to_idle_minimum() {
    let dashboard = TerminalDashboard::new(
        TerminalDashboardConfig {
            render_interval_ms: 50,
            ..TerminalDashboardConfig::default()
        },
        RecordingSink::tty(),
    );

    assert_eq!(dashboard.config().render_interval_ms, 1_000);
}

#[derive(Debug, Default)]
struct RecordingSink {
    tty: bool,
    frames: Vec<String>,
}

impl RecordingSink {
    fn tty() -> Self {
        Self {
            tty: true,
            frames: Vec::new(),
        }
    }

    fn non_tty() -> Self {
        Self {
            tty: false,
            frames: Vec::new(),
        }
    }
}

impl DashboardSink for RecordingSink {
    fn is_tty(&self) -> bool {
        self.tty
    }

    fn write_frame(&mut self, frame: &str) -> io::Result<()> {
        self.frames.push(frame.to_owned());
        Ok(())
    }
}

fn sample_snapshot(now: chrono::DateTime<Utc>) -> OrchestratorState {
    let issue = Issue {
        id: IssueId::new("issue-18"),
        identifier: IssueIdentifier::new("SPA-18"),
        title: "Terminal Dashboard".to_owned(),
        description: Some("Render orchestrator status with ANSI".to_owned()),
        priority: Some(1),
        state: "In Progress".to_owned(),
        branch_name: Some("feature/SPA-18".to_owned()),
        url: Some("https://linear.app/sparkzhuang/issue/SPA-18".to_owned()),
        labels: vec!["rust".to_owned(), "module-11".to_owned()],
        blocked_by: vec![BlockerRef {
            id: Some(IssueId::new("dep-1")),
            identifier: Some(IssueIdentifier::new("SPA-8")),
            state: Some("Done".to_owned()),
        }],
        created_at: Some(now - Duration::hours(2)),
        updated_at: Some(now - Duration::minutes(1)),
    };

    let started_at = now - Duration::minutes(5);
    let running_entry = RunningEntry {
        issue: issue.clone(),
        run_attempt: RunAttempt {
            issue_id: issue.id.clone(),
            issue_identifier: issue.identifier.clone(),
            attempt: None,
            workspace_path: PathBuf::from("/tmp/SPA-18"),
            started_at,
            status: RunStatus::StreamingTurn,
            error: None,
        },
        live_session: Some(LiveSession {
            session_id: symphony_rust::types::SessionId::from_parts("sess-main", "001"),
            thread_id: "sess-main".to_owned(),
            turn_id: "001".to_owned(),
            codex_app_server_pid: Some("4242".to_owned()),
            last_codex_event: Some("token_count".to_owned()),
            last_codex_timestamp: Some(now - Duration::seconds(10)),
            last_codex_message: Some("bootstrap completed".to_owned()),
            codex_input_tokens: 1_234,
            codex_output_tokens: 8_642,
            codex_total_tokens: 9_876,
            last_reported_input_tokens: 1_234,
            last_reported_output_tokens: 8_642,
            last_reported_total_tokens: 9_876,
            turn_count: 7,
        }),
        worker_host: Some("localhost".to_owned()),
    };

    OrchestratorState {
        poll_interval_ms: 5_000,
        max_concurrent_agents: 4,
        running: HashMap::from([(issue.id.clone(), running_entry)]),
        claimed: HashSet::from([issue.id.clone()]),
        retry_attempts: HashMap::from([(
            IssueId::new("issue-19"),
            RetryEntry {
                issue_id: IssueId::new("issue-19"),
                identifier: IssueIdentifier::new("SPA-19"),
                attempt: 2,
                due_at_ms: now.timestamp_millis() as u64 + 90_000,
                timer_handle: Some("timer-1".to_owned()),
                error: Some("rate limited by upstream".to_owned()),
            },
        )]),
        completed: HashSet::new(),
        codex_totals: CodexTotals {
            input_tokens: 1_234,
            output_tokens: 8_642,
            total_tokens: 9_876,
            seconds_running: 420,
        },
        codex_rate_limits: Some(json!({
            "limit_id": "codex-default",
            "primary": { "remaining": 12, "limit": 20, "reset_in_seconds": 34 },
            "secondary": { "remaining": 5, "limit": 10, "reset_in_seconds": 17 },
            "credits": { "has_credits": true, "balance": 42.5 }
        })),
    }
}
