use std::cmp::max;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Write};

use chrono::{DateTime, Utc};
use serde_json::Value;
use terminal_size::{terminal_size, Width};

use crate::dashboard::{Dashboard, DashboardSink, RenderOutcome};
use crate::types::{LiveSession, OrchestratorState, RetryEntry, RunStatus, RunningEntry};

const MIN_IDLE_INTERVAL_MS: u64 = 1_000;
const THROUGHPUT_WINDOW_MS: u64 = 5_000;
const THROUGHPUT_GRAPH_WINDOW_MS: u64 = 10 * 60 * 1_000;
const THROUGHPUT_GRAPH_COLUMNS: usize = 24;
const SPARKLINE_BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

const RUNNING_ID_WIDTH: usize = 8;
const RUNNING_STAGE_WIDTH: usize = 14;
const RUNNING_PID_WIDTH: usize = 8;
const RUNNING_AGE_WIDTH: usize = 12;
const RUNNING_TOKENS_WIDTH: usize = 10;
const RUNNING_SESSION_WIDTH: usize = 14;
const RUNNING_EVENT_MIN_WIDTH: usize = 12;
const RUNNING_ROW_CHROME_WIDTH: usize = 10;

const DEFAULT_TERMINAL_COLUMNS: u16 = 115;

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_MAGENTA: &str = "\x1b[35m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_WHITE: &str = "\x1b[37m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_GRAY: &str = "\x1b[90m";

/// Terminal dashboard configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalDashboardConfig {
    /// Minimum polling cadence for renderer updates.
    pub refresh_interval_ms: u64,
    /// Minimum interval between consecutive writes.
    pub render_interval_ms: u64,
    /// Width used when terminal size is unavailable.
    pub default_terminal_columns: u16,
    /// Optional width override for deterministic tests or fixed layouts.
    pub terminal_columns_override: Option<u16>,
    /// Explicit enable/disable override. `None` defers to TTY detection.
    pub enabled_override: Option<bool>,
}

impl Default for TerminalDashboardConfig {
    fn default() -> Self {
        Self {
            refresh_interval_ms: MIN_IDLE_INTERVAL_MS,
            render_interval_ms: MIN_IDLE_INTERVAL_MS,
            default_terminal_columns: DEFAULT_TERMINAL_COLUMNS,
            terminal_columns_override: None,
            enabled_override: None,
        }
    }
}

/// ANSI terminal dashboard renderer backed by a configurable output sink.
#[derive(Debug)]
pub struct TerminalDashboard<S> {
    config: TerminalDashboardConfig,
    sink: S,
    token_samples: Vec<(u64, u64)>,
    last_rendered_fingerprint: Option<u64>,
    last_rendered_at_ms: Option<u64>,
    pending_frame: Option<PendingFrame>,
}

#[derive(Debug, Clone)]
struct PendingFrame {
    fingerprint: u64,
    content: String,
}

impl<S> TerminalDashboard<S>
where
    S: DashboardSink,
{
    /// Creates a terminal dashboard with a custom sink.
    pub fn new(mut config: TerminalDashboardConfig, sink: S) -> Self {
        config.refresh_interval_ms = max(config.refresh_interval_ms, MIN_IDLE_INTERVAL_MS);
        config.render_interval_ms = max(config.render_interval_ms, MIN_IDLE_INTERVAL_MS);

        Self {
            config,
            sink,
            token_samples: Vec::new(),
            last_rendered_fingerprint: None,
            last_rendered_at_ms: None,
            pending_frame: None,
        }
    }

    /// Returns the effective dashboard configuration.
    pub fn config(&self) -> &TerminalDashboardConfig {
        &self.config
    }

    /// Returns the underlying sink.
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Formats a dashboard frame for the provided snapshot and terminal width.
    pub fn render_frame(
        snapshot: &OrchestratorState,
        now: DateTime<Utc>,
        monotonic_now_ms: u64,
        graph: &str,
        columns: u16,
    ) -> String {
        let running_event_width = running_event_width(columns);
        let running_rows = format_running_rows(snapshot, now, running_event_width);
        let retry_rows = format_retry_rows(snapshot, monotonic_now_ms);
        let rate_limits = format_rate_limits(snapshot.codex_rate_limits.as_ref());
        let totals = &snapshot.codex_totals;
        let runtime = format_duration_seconds(totals.seconds_running);

        [
            colorize("╭─ SYMPHONY STATUS", ANSI_BOLD),
            colorize("│ Global Stats", ANSI_BOLD),
            format!(
                "{}{}",
                colorize("│   Runtime: ", ANSI_BOLD),
                colorize(&runtime, ANSI_MAGENTA)
            ),
            format!(
                "{}{}{}{}{}{}{}",
                colorize("│   Tokens: ", ANSI_BOLD),
                colorize(
                    &format!("in {}", format_count(totals.input_tokens)),
                    ANSI_YELLOW
                ),
                colorize(" | ", ANSI_GRAY),
                colorize(
                    &format!("out {}", format_count(totals.output_tokens)),
                    ANSI_YELLOW
                ),
                colorize(" | ", ANSI_GRAY),
                colorize(
                    &format!("total {}", format_count(totals.total_tokens)),
                    ANSI_YELLOW
                ),
                ""
            ),
            format!(
                "{}{}",
                colorize("│   Rate Limits: ", ANSI_BOLD),
                rate_limits
            ),
            format!(
                "{}{}",
                colorize("│   Throughput: ", ANSI_BOLD),
                colorize(graph, ANSI_CYAN)
            ),
            colorize("├─ Running Sessions", ANSI_BOLD),
            "│".to_owned(),
            running_table_header_row(running_event_width),
            running_table_separator_row(running_event_width),
            running_rows.join("\n"),
            colorize("├─ Retry Queue", ANSI_BOLD),
            retry_rows.join("\n"),
            closing_border(),
        ]
        .join("\n")
    }

    fn resolve_columns(&self) -> u16 {
        if let Some(columns) = self.config.terminal_columns_override {
            return columns;
        }

        terminal_size()
            .map(|(Width(width), _)| width)
            .filter(|width| *width > 0)
            .unwrap_or(self.config.default_terminal_columns)
    }

    fn write_rendered_frame(
        &mut self,
        fingerprint: u64,
        content: String,
        monotonic_now_ms: u64,
    ) -> io::Result<RenderOutcome> {
        let frame = format!("\x1b[2J\x1b[H{content}\n");
        self.sink.write_frame(&frame)?;
        self.last_rendered_fingerprint = Some(fingerprint);
        self.last_rendered_at_ms = Some(monotonic_now_ms);
        self.pending_frame = None;
        Ok(RenderOutcome::Rendered)
    }

    fn ready_to_render(&self, monotonic_now_ms: u64) -> bool {
        self.last_rendered_at_ms
            .map(|last| monotonic_now_ms.saturating_sub(last) >= self.config.render_interval_ms)
            .unwrap_or(true)
    }
}

impl<S> Dashboard for TerminalDashboard<S>
where
    S: DashboardSink,
{
    fn is_enabled(&self) -> bool {
        self.config
            .enabled_override
            .unwrap_or_else(|| self.sink.is_tty())
    }

    fn render_snapshot(
        &mut self,
        snapshot: &OrchestratorState,
        now: DateTime<Utc>,
        monotonic_now_ms: u64,
    ) -> io::Result<RenderOutcome> {
        if !self.is_enabled() {
            return Ok(RenderOutcome::Disabled);
        }

        let current_total_tokens = snapshot.codex_totals.total_tokens;
        let should_record_sample = self
            .token_samples
            .last()
            .map(|(_, total_tokens)| *total_tokens != current_total_tokens)
            .unwrap_or(true);
        if should_record_sample {
            self.token_samples
                .push((monotonic_now_ms, current_total_tokens));
        }
        prune_graph_samples(&mut self.token_samples, monotonic_now_ms);

        let columns = self.resolve_columns();
        let graph = sparkline(&self.token_samples, monotonic_now_ms, current_total_tokens);
        let content = Self::render_frame(snapshot, now, monotonic_now_ms, &graph, columns);
        let fingerprint = frame_fingerprint(&content);

        if self.last_rendered_fingerprint == Some(fingerprint) {
            self.pending_frame = None;
            return Ok(RenderOutcome::SkippedDuplicate);
        }

        if self.ready_to_render(monotonic_now_ms) {
            return self.write_rendered_frame(fingerprint, content, monotonic_now_ms);
        }

        if self
            .pending_frame
            .as_ref()
            .map(|pending| pending.fingerprint == fingerprint && pending.content == content)
            .unwrap_or(false)
        {
            return Ok(RenderOutcome::SkippedRateLimited);
        }

        self.pending_frame = Some(PendingFrame {
            fingerprint,
            content,
        });
        Ok(RenderOutcome::SkippedRateLimited)
    }

    fn flush_pending(&mut self, monotonic_now_ms: u64) -> io::Result<RenderOutcome> {
        if !self.is_enabled() {
            self.pending_frame = None;
            return Ok(RenderOutcome::Disabled);
        }

        if !self.ready_to_render(monotonic_now_ms) {
            return Ok(RenderOutcome::SkippedRateLimited);
        }

        match self.pending_frame.clone() {
            Some(pending) => {
                self.write_rendered_frame(pending.fingerprint, pending.content, monotonic_now_ms)
            }
            None => Ok(RenderOutcome::SkippedDuplicate),
        }
    }
}

/// Default sink that writes ANSI frames to standard output.
#[derive(Debug, Default)]
pub struct StdoutDashboardSink;

impl DashboardSink for StdoutDashboardSink {
    fn is_tty(&self) -> bool {
        io::stdout().is_terminal()
    }

    fn write_frame(&mut self, frame: &str) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(frame.as_bytes())?;
        stdout.flush()
    }
}

/// Builds a 24-column sparkline from token samples over the last 10 minutes.
pub fn sparkline(samples: &[(u64, u64)], now_ms: u64, current_tokens: u64) -> String {
    let bucket_ms = THROUGHPUT_GRAPH_WINDOW_MS / THROUGHPUT_GRAPH_COLUMNS as u64;
    let active_bucket_start = (now_ms / bucket_ms) * bucket_ms;
    let graph_window_start =
        active_bucket_start.saturating_sub((THROUGHPUT_GRAPH_COLUMNS as u64 - 1) * bucket_ms);

    let mut ordered_samples = samples.to_vec();
    let append_current_sample = ordered_samples
        .last()
        .map(|(_, tokens)| *tokens != current_tokens)
        .unwrap_or(true);
    if append_current_sample {
        ordered_samples.push((now_ms, current_tokens));
    }
    ordered_samples.sort_by_key(|(timestamp, _)| *timestamp);
    prune_graph_samples(&mut ordered_samples, now_ms);

    let mut rates = Vec::new();
    for pair in ordered_samples.windows(2) {
        let (start_ms, start_tokens) = pair[0];
        let (end_ms, end_tokens) = pair[1];
        let elapsed_ms = end_ms.saturating_sub(start_ms);
        let delta_tokens = end_tokens.saturating_sub(start_tokens);
        let tps = if elapsed_ms == 0 {
            0.0
        } else {
            delta_tokens as f64 / (elapsed_ms as f64 / 1_000.0)
        };
        rates.push((end_ms, tps));
    }

    let mut bucketed = Vec::with_capacity(THROUGHPUT_GRAPH_COLUMNS);
    for bucket_idx in 0..THROUGHPUT_GRAPH_COLUMNS {
        let bucket_start = graph_window_start + bucket_idx as u64 * bucket_ms;
        let bucket_end = bucket_start + bucket_ms;
        let last_bucket = bucket_idx == THROUGHPUT_GRAPH_COLUMNS - 1;

        let values = rates
            .iter()
            .filter_map(|(timestamp, tps)| {
                let in_bucket = if last_bucket {
                    *timestamp >= bucket_start && *timestamp <= bucket_end
                } else {
                    *timestamp >= bucket_start && *timestamp < bucket_end
                };
                in_bucket.then_some(*tps)
            })
            .collect::<Vec<_>>();

        let average = if values.is_empty() {
            0.0
        } else {
            values.iter().sum::<f64>() / values.len() as f64
        };

        bucketed.push(average);
    }

    let max_tps = bucketed.iter().fold(0.0_f64, |acc, value| acc.max(*value));
    bucketed
        .into_iter()
        .map(|value| {
            let index = if max_tps <= 0.0 {
                0
            } else {
                ((value / max_tps) * (SPARKLINE_BLOCKS.len() - 1) as f64).round() as usize
            };
            SPARKLINE_BLOCKS.get(index).copied().unwrap_or('▁')
        })
        .collect()
}

fn prune_graph_samples(samples: &mut Vec<(u64, u64)>, now_ms: u64) {
    let min_timestamp = now_ms.saturating_sub(THROUGHPUT_GRAPH_WINDOW_MS.max(THROUGHPUT_WINDOW_MS));
    samples.retain(|(timestamp, _)| *timestamp >= min_timestamp);
}

fn frame_fingerprint(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

fn format_running_rows(
    snapshot: &OrchestratorState,
    now: DateTime<Utc>,
    event_width: usize,
) -> Vec<String> {
    if snapshot.running.is_empty() {
        return vec![format!("│  {}", colorize("No active sessions", ANSI_GRAY))];
    }

    let mut running = snapshot.running.values().collect::<Vec<_>>();
    running.sort_by_key(|entry| entry.issue.identifier.as_str().to_owned());
    running
        .into_iter()
        .map(|entry| format_running_row(entry, now, event_width))
        .collect()
}

fn format_running_row(entry: &RunningEntry, now: DateTime<Utc>, event_width: usize) -> String {
    let live_session = entry.live_session.as_ref();
    let age = format_age(now, &entry.run_attempt, live_session);
    let session = live_session
        .map(|session| session.session_id.as_str().to_owned())
        .unwrap_or_else(|| "n/a".to_owned());
    let pid = live_session
        .and_then(|session| session.codex_app_server_pid.clone())
        .unwrap_or_else(|| "n/a".to_owned());
    let tokens = live_session
        .map(|session| session.codex_total_tokens)
        .unwrap_or_default();
    let event = summarize_last_event(live_session);

    format!(
        "│ {} {} {} {} {} {} {}",
        colorize(
            &format_cell(
                entry.issue.identifier.as_str(),
                RUNNING_ID_WIDTH,
                Alignment::Left
            ),
            ANSI_CYAN
        ),
        colorize(
            &format_cell(
                &format!("{:?}", entry.run_attempt.status),
                RUNNING_STAGE_WIDTH,
                Alignment::Left
            ),
            status_color(&entry.run_attempt.status)
        ),
        colorize(
            &format_cell(&pid, RUNNING_PID_WIDTH, Alignment::Left),
            ANSI_YELLOW
        ),
        colorize(
            &format_cell(&age, RUNNING_AGE_WIDTH, Alignment::Left),
            ANSI_MAGENTA
        ),
        colorize(
            &format_cell(
                &format_count(tokens),
                RUNNING_TOKENS_WIDTH,
                Alignment::Right
            ),
            ANSI_YELLOW
        ),
        colorize(
            &format_cell(&session, RUNNING_SESSION_WIDTH, Alignment::Left),
            ANSI_CYAN
        ),
        colorize(
            &format_cell(&event, event_width, Alignment::Left),
            ANSI_WHITE
        )
    )
}

fn format_retry_rows(snapshot: &OrchestratorState, monotonic_now_ms: u64) -> Vec<String> {
    if snapshot.retry_attempts.is_empty() {
        return vec![format!("│  {}", colorize("No queued retries", ANSI_GRAY))];
    }

    let mut retry_entries = snapshot.retry_attempts.values().collect::<Vec<_>>();
    retry_entries.sort_by_key(|entry| entry.due_at_ms);
    retry_entries
        .into_iter()
        .map(|entry| format_retry_row(entry, monotonic_now_ms))
        .collect()
}

fn format_retry_row(entry: &RetryEntry, monotonic_now_ms: u64) -> String {
    let due = if entry.due_at_ms <= monotonic_now_ms {
        "now".to_owned()
    } else {
        format_duration_ms(entry.due_at_ms - monotonic_now_ms)
    };
    let error = entry
        .error
        .as_deref()
        .map(sanitize_inline)
        .filter(|value| !value.is_empty())
        .map(|value| {
            format!(
                " {}",
                colorize(&format!("error={}", truncate(&value, 48)), ANSI_DIM)
            )
        })
        .unwrap_or_default();

    format!(
        "│  {} {} {} {}{}",
        colorize("↻", ANSI_YELLOW),
        colorize(entry.identifier.as_str(), ANSI_RED),
        colorize(&format!("attempt={}", entry.attempt), ANSI_YELLOW),
        colorize(&format!("due={due}"), ANSI_CYAN),
        error
    )
}

fn summarize_last_event(live_session: Option<&LiveSession>) -> String {
    let Some(live_session) = live_session else {
        return "no session".to_owned();
    };

    if let Some(message) = live_session.last_codex_message.as_deref() {
        let inline = sanitize_inline(message);
        if !inline.is_empty() {
            return inline;
        }
    }

    live_session
        .last_codex_event
        .clone()
        .unwrap_or_else(|| "none".to_owned())
}

fn running_table_header_row(event_width: usize) -> String {
    let header = [
        format_cell("ID", RUNNING_ID_WIDTH, Alignment::Left),
        format_cell("STATE", RUNNING_STAGE_WIDTH, Alignment::Left),
        format_cell("PID", RUNNING_PID_WIDTH, Alignment::Left),
        format_cell("AGE", RUNNING_AGE_WIDTH, Alignment::Left),
        format_cell("TOKENS", RUNNING_TOKENS_WIDTH, Alignment::Right),
        format_cell("SESSION", RUNNING_SESSION_WIDTH, Alignment::Left),
        format_cell("EVENT", event_width, Alignment::Left),
    ]
    .join(" ");

    format!("│ {}", colorize(&header, ANSI_GRAY))
}

fn running_table_separator_row(event_width: usize) -> String {
    let width = RUNNING_ID_WIDTH
        + RUNNING_STAGE_WIDTH
        + RUNNING_PID_WIDTH
        + RUNNING_AGE_WIDTH
        + RUNNING_TOKENS_WIDTH
        + RUNNING_SESSION_WIDTH
        + event_width
        + 6;
    format!("│ {}", colorize(&"─".repeat(width), ANSI_GRAY))
}

fn running_event_width(columns: u16) -> usize {
    let columns = columns as usize;
    let fixed = RUNNING_ID_WIDTH
        + RUNNING_STAGE_WIDTH
        + RUNNING_PID_WIDTH
        + RUNNING_AGE_WIDTH
        + RUNNING_TOKENS_WIDTH
        + RUNNING_SESSION_WIDTH;
    columns
        .saturating_sub(fixed + RUNNING_ROW_CHROME_WIDTH)
        .max(RUNNING_EVENT_MIN_WIDTH)
}

fn format_rate_limits(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return colorize("unavailable", ANSI_GRAY);
    };

    let Some(map) = value.as_object() else {
        return colorize(&truncate(&value.to_string(), 80), ANSI_GRAY);
    };

    let limit_id = map
        .get("limit_id")
        .or_else(|| map.get("limit_name"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let primary = format_rate_limit_bucket(map.get("primary"));
    let secondary = format_rate_limit_bucket(map.get("secondary"));
    let credits = format_rate_limit_credits(map.get("credits"));

    format!(
        "{}{}{}{}{}{}{}",
        colorize(limit_id, ANSI_YELLOW),
        colorize(" | ", ANSI_GRAY),
        colorize(&format!("primary {primary}"), ANSI_CYAN),
        colorize(" | ", ANSI_GRAY),
        colorize(&format!("secondary {secondary}"), ANSI_CYAN),
        colorize(" | ", ANSI_GRAY),
        colorize(&credits, ANSI_GREEN),
    )
}

fn format_rate_limit_bucket(value: Option<&Value>) -> String {
    let Some(Value::Object(map)) = value else {
        return "n/a".to_owned();
    };

    let remaining = map.get("remaining").and_then(Value::as_u64);
    let limit = map.get("limit").and_then(Value::as_u64);
    let reset = map
        .get("reset_in_seconds")
        .or_else(|| map.get("resetInSeconds"))
        .and_then(Value::as_u64);

    let base = match (remaining, limit) {
        (Some(remaining), Some(limit)) => {
            format!("{}/{}", format_count(remaining), format_count(limit))
        }
        (Some(remaining), None) => format!("remaining {}", format_count(remaining)),
        (None, Some(limit)) => format!("limit {}", format_count(limit)),
        (None, None) => "n/a".to_owned(),
    };

    reset
        .map(|reset| format!("{base} reset {}s", format_count(reset)))
        .unwrap_or(base)
}

fn format_rate_limit_credits(value: Option<&Value>) -> String {
    let Some(Value::Object(map)) = value else {
        return "credits n/a".to_owned();
    };

    if map.get("unlimited").and_then(Value::as_bool) == Some(true) {
        return "credits unlimited".to_owned();
    }

    if map.get("has_credits").and_then(Value::as_bool) == Some(true) {
        return map
            .get("balance")
            .and_then(Value::as_f64)
            .map(|balance| format!("credits {:.2}", balance))
            .unwrap_or_else(|| "credits available".to_owned());
    }

    "credits none".to_owned()
}

fn status_color(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Succeeded => ANSI_GREEN,
        RunStatus::Failed | RunStatus::TimedOut | RunStatus::Stalled => ANSI_RED,
        RunStatus::CanceledByReconciliation => ANSI_GRAY,
        RunStatus::Finishing => ANSI_MAGENTA,
        _ => ANSI_CYAN,
    }
}

fn format_age(
    now: DateTime<Utc>,
    attempt: &crate::types::RunAttempt,
    live_session: Option<&LiveSession>,
) -> String {
    let seconds = now
        .signed_duration_since(attempt.started_at)
        .num_seconds()
        .max(0) as u64;
    let turns = live_session
        .map(|session| session.turn_count)
        .unwrap_or_default();
    if turns > 0 {
        format!("{} / {}", format_duration_seconds(seconds), turns)
    } else {
        format_duration_seconds(seconds)
    }
}

fn format_duration_seconds(seconds: u64) -> String {
    format!("{}m {}s", seconds / 60, seconds % 60)
}

fn format_duration_ms(duration_ms: u64) -> String {
    let seconds = duration_ms / 1_000;
    let minutes = seconds / 60;
    let remaining_seconds = seconds % 60;

    if minutes > 0 {
        format!("{minutes}m {remaining_seconds}s")
    } else {
        format!("{remaining_seconds}s")
    }
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut reversed = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            reversed.push(',');
        }
        reversed.push(ch);
    }
    reversed.chars().rev().collect()
}

fn sanitize_inline(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }

    let take = width.saturating_sub(3);
    format!("{}...", value.chars().take(take).collect::<String>())
}

fn colorize(value: &str, code: &str) -> String {
    format!("{code}{value}{ANSI_RESET}")
}

fn closing_border() -> String {
    "╰─".to_owned()
}

#[derive(Debug, Clone, Copy)]
enum Alignment {
    Left,
    Right,
}

fn format_cell(value: &str, width: usize, alignment: Alignment) -> String {
    let truncated = truncate(&sanitize_inline(value), width);
    let fill = width.saturating_sub(truncated.chars().count());

    match alignment {
        Alignment::Left => format!("{truncated}{}", " ".repeat(fill)),
        Alignment::Right => format!("{}{truncated}", " ".repeat(fill)),
    }
}
