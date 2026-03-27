use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde_json::json;

use super::presenter;
use super::{AppState, RefreshError, SnapshotError};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/state", get(state_snapshot))
        .route("/api/v1/refresh", post(refresh))
        .route("/api/v1/{issue_identifier}", get(issue_detail))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
}

async fn healthz() -> impl IntoResponse {
    StatusCode::OK
}

async fn state_snapshot(State(state): State<AppState>) -> Response {
    match state.snapshots.snapshot(state.snapshot_timeout).await {
        Ok(snapshot) => {
            let now = Utc::now();
            Json(presenter::state_payload(&snapshot, now)).into_response()
        }
        Err(SnapshotError::Timeout) => Json(presenter::state_error_payload(
            "snapshot_timeout",
            "Snapshot timed out",
            Utc::now(),
        ))
        .into_response(),
        Err(SnapshotError::Unavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(presenter::state_error_payload(
                "snapshot_unavailable",
                "Snapshot unavailable",
                Utc::now(),
            )),
        )
            .into_response(),
    }
}

async fn issue_detail(
    State(state): State<AppState>,
    Path(issue_identifier): Path<String>,
) -> Response {
    match state.snapshots.snapshot(state.snapshot_timeout).await {
        Ok(snapshot) => match presenter::issue_payload(
            &snapshot,
            &issue_identifier,
            &state.workspace_root,
            Utc::now(),
        ) {
            Some(payload) => Json(payload).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(error_payload("issue_not_found", "Issue not found")),
            )
                .into_response(),
        },
        Err(SnapshotError::Timeout) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_payload("snapshot_timeout", "Snapshot timed out")),
        )
            .into_response(),
        Err(SnapshotError::Unavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_payload(
                "snapshot_unavailable",
                "Snapshot unavailable",
            )),
        )
            .into_response(),
    }
}

async fn refresh(State(state): State<AppState>) -> Response {
    match state.refresh.request_refresh() {
        Ok(result) => (
            StatusCode::ACCEPTED,
            Json(presenter::refresh_payload(result, Utc::now())),
        )
            .into_response(),
        Err(RefreshError::Unavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_payload(
                "orchestrator_unavailable",
                "Orchestrator is unavailable",
            )),
        )
            .into_response(),
    }
}

async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(error_payload("method_not_allowed", "Method not allowed")),
    )
        .into_response()
}

async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(error_payload("not_found", "Route not found")),
    )
        .into_response()
}

fn error_payload(code: &str, message: &str) -> serde_json::Value {
    json!({
        "error": {
            "code": code,
            "message": message
        }
    })
}
