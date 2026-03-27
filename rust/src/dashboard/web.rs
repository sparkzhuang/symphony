use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::Utc;
use serde::Serialize;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;
use tracing::warn;

use crate::server::presenter;
use crate::server::{SnapshotSource, SnapshotState};

pub(crate) fn event_stream(
    snapshots: SnapshotSource,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = WatchStream::new(snapshots.subscribe()).map(|state| {
        let data = event_data_for_state(state);

        Ok(Event::default().event("state").data(data))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}

fn event_data_for_state(state: SnapshotState) -> String {
    match state {
        SnapshotState::Ready(snapshot) => {
            serialize_event_payload(&presenter::state_payload(&snapshot, Utc::now()))
        }
        SnapshotState::Pending => serialize_event_payload(&presenter::state_error_payload(
            "snapshot_pending",
            "Snapshot is not ready yet",
            Utc::now(),
        )),
        SnapshotState::Unavailable => serialize_event_payload(&presenter::state_error_payload(
            "snapshot_unavailable",
            "Snapshot unavailable",
            Utc::now(),
        )),
    }
}

fn serialize_event_payload<T>(payload: &T) -> String
where
    T: Serialize,
{
    serde_json::to_string(payload).unwrap_or_else(|error| {
        warn!(?error, "failed to serialize dashboard SSE payload");
        presenter::state_error_payload(
            "serialization_error",
            "Failed to serialize dashboard update",
            Utc::now(),
        )
        .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::serialize_event_payload;
    use serde::ser::{Error as _, Serializer};
    use serde::Serialize;

    struct FailingPayload;

    impl Serialize for FailingPayload {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(S::Error::custom("forced serialization failure"))
        }
    }

    #[test]
    fn serialize_event_payload_falls_back_to_error_payload() {
        let payload = serialize_event_payload(&FailingPayload);
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("fallback payload should be valid json");

        assert_eq!(parsed["error"]["code"], "serialization_error");
        assert_eq!(
            parsed["error"]["message"],
            "Failed to serialize dashboard update"
        );
        assert!(parsed.get("generated_at").is_some());
    }
}
