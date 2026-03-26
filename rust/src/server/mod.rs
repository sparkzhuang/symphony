use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::{routing::get, Json, Router};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;

pub struct ServerHandle {
    local_addr: SocketAddr,
    shutdown_tx: watch::Sender<bool>,
    join_handle: tokio::task::JoinHandle<Result<()>>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub async fn wait(&mut self) -> Result<()> {
        (&mut self.join_handle)
            .await
            .context("server task join failed")?
            .context("server task failed")
    }
}

pub async fn start(port: u16) -> Result<ServerHandle> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("failed to bind HTTP server on 127.0.0.1:{port}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to determine HTTP server local address")?;

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health));
    let join_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(wait_for_shutdown(shutdown_rx))
            .await
            .context("http server exited unexpectedly")
    });

    Ok(ServerHandle {
        local_addr,
        shutdown_tx,
        join_handle,
    })
}

async fn root() -> Json<serde_json::Value> {
    Json(json!({
        "service": "symphony-rust",
        "status": "ok"
    }))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }

    let _ = shutdown.changed().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serves_health_endpoint_and_shuts_down_cleanly() {
        let mut server = start(0)
            .await
            .expect("server should start on an ephemeral port");
        let url = format!("http://{}/health", server.local_addr());

        let response = reqwest::get(url)
            .await
            .expect("health request should succeed");
        let payload = response
            .json::<serde_json::Value>()
            .await
            .expect("health response should decode as json");

        assert_eq!(payload, json!({ "ok": true }));

        server.request_shutdown();
        server.wait().await.expect("server should stop cleanly");
    }
}
