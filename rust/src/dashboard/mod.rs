use std::io;

use chrono::{DateTime, Utc};

use crate::types::OrchestratorState;

pub mod terminal;

/// Result of attempting to render a dashboard snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderOutcome {
    /// Rendering is disabled for the current environment or configuration.
    Disabled,
    /// The frame content matches the last rendered fingerprint.
    SkippedDuplicate,
    /// A new frame was observed but deferred until the render interval elapses.
    SkippedRateLimited,
    /// A frame was written to the sink.
    Rendered,
}

/// Shared dashboard interface for operator-facing status surfaces.
pub trait Dashboard {
    /// Returns whether the dashboard is enabled for the current environment.
    fn is_enabled(&self) -> bool;

    /// Consumes a new orchestrator snapshot and renders it when appropriate.
    fn render_snapshot(
        &mut self,
        snapshot: &OrchestratorState,
        now: DateTime<Utc>,
        monotonic_now_ms: u64,
    ) -> io::Result<RenderOutcome>;
}

/// Output target used by dashboard renderers.
pub trait DashboardSink {
    /// Returns whether the sink is connected to an interactive terminal.
    fn is_tty(&self) -> bool;

    /// Writes a fully formatted frame to the output target.
    fn write_frame(&mut self, frame: &str) -> io::Result<()>;
}
