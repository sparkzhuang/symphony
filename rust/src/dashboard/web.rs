use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::Utc;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;

use crate::server::presenter;
use crate::server::{SnapshotSource, SnapshotState};

pub(crate) fn event_stream(
    snapshots: SnapshotSource,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = WatchStream::new(snapshots.subscribe()).map(|state| {
        let payload = payload_for_state(state);
        let data = serde_json::to_string(&payload)
            .expect("serializing dashboard SSE payload should never fail");

        Ok(Event::default().event("state").data(data))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}

fn payload_for_state(state: SnapshotState) -> serde_json::Value {
    match state {
        SnapshotState::Ready(snapshot) => {
            serde_json::to_value(presenter::state_payload(&snapshot, Utc::now()))
                .expect("serializing presenter payload should never fail")
        }
        SnapshotState::Pending => presenter::state_error_payload(
            "snapshot_pending",
            "Snapshot is not ready yet",
            Utc::now(),
        ),
        SnapshotState::Unavailable => presenter::state_error_payload(
            "snapshot_unavailable",
            "Snapshot unavailable",
            Utc::now(),
        ),
    }
}
