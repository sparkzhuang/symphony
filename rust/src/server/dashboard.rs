use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::dashboard::web;

use super::AppState;

const INDEX_HTML: &str = include_str!("../../static/index.html");
const DASHBOARD_CSS: &str = include_str!("../../static/dashboard.css");
const DASHBOARD_JS: &str = include_str!("../../static/dashboard.js");

pub(crate) fn attach(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/", get(index))
        .route("/assets/dashboard.css", get(stylesheet))
        .route("/assets/dashboard.js", get(script))
        .route("/api/v1/events", get(events))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn stylesheet() -> Response {
    (
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, "no-cache"),
        ],
        DASHBOARD_CSS,
    )
        .into_response()
}

async fn script() -> Response {
    (
        [
            (CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (CACHE_CONTROL, "no-cache"),
        ],
        DASHBOARD_JS,
    )
        .into_response()
}

async fn events(State(state): State<AppState>) -> impl IntoResponse {
    web::event_stream(state.snapshots)
}
