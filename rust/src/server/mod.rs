pub mod api;
pub mod presenter;

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::net::{lookup_host, TcpListener};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::config::WorkflowStore;
use crate::types::OrchestratorState;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub state: Box<OrchestratorState>,
    pub observed_at: DateTime<Utc>,
    pub monotonic_now_ms: u64,
}

impl Snapshot {
    pub fn new(
        state: OrchestratorState,
        observed_at: DateTime<Utc>,
        monotonic_now_ms: u64,
    ) -> Self {
        Self {
            state: Box::new(state),
            observed_at,
            monotonic_now_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotState {
    Pending,
    Ready(Box<Snapshot>),
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotError {
    Timeout,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshQueued {
    pub coalesced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshError {
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct ServerOptions {
    host: Option<String>,
    cli_port: Option<u16>,
    config_port: Option<u16>,
    snapshot_timeout: Duration,
    snapshot_rx: watch::Receiver<SnapshotState>,
    refresh_tx: mpsc::Sender<()>,
}

impl ServerOptions {
    pub fn new(snapshot_rx: watch::Receiver<SnapshotState>, refresh_tx: mpsc::Sender<()>) -> Self {
        Self {
            host: None,
            cli_port: None,
            config_port: None,
            snapshot_timeout: DEFAULT_SNAPSHOT_TIMEOUT,
            snapshot_rx,
            refresh_tx,
        }
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    pub fn with_cli_port(mut self, port: Option<u16>) -> Self {
        self.cli_port = port;
        self
    }

    pub fn with_config_port(mut self, port: Option<u16>) -> Self {
        self.config_port = port;
        self
    }

    pub fn with_snapshot_timeout(mut self, timeout: Duration) -> Self {
        self.snapshot_timeout = timeout;
        self
    }

    pub fn effective_port(&self) -> Option<u16> {
        self.cli_port.or(self.config_port)
    }

    fn bind_host(&self) -> &str {
        self.host.as_deref().unwrap_or(DEFAULT_HOST)
    }

    fn bind_port(&self) -> u16 {
        self.effective_port().unwrap_or(0)
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) snapshots: SnapshotSource,
    pub(crate) refresh: RefreshTrigger,
    pub(crate) snapshot_timeout: Duration,
}

impl AppState {
    fn new(options: &ServerOptions) -> Self {
        Self {
            snapshots: SnapshotSource::new(options.snapshot_rx.clone()),
            refresh: RefreshTrigger::new(options.refresh_tx.clone()),
            snapshot_timeout: options.snapshot_timeout,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SnapshotSource {
    receiver: watch::Receiver<SnapshotState>,
}

impl SnapshotSource {
    fn new(receiver: watch::Receiver<SnapshotState>) -> Self {
        Self { receiver }
    }

    pub(crate) async fn snapshot(&self, timeout: Duration) -> Result<Snapshot, SnapshotError> {
        let mut receiver = self.receiver.clone();

        match receiver.borrow().clone() {
            SnapshotState::Ready(snapshot) => return Ok(*snapshot),
            SnapshotState::Unavailable => return Err(SnapshotError::Unavailable),
            SnapshotState::Pending => {}
        }

        let wait_for_change = tokio::time::timeout(timeout, async move {
            loop {
                receiver
                    .changed()
                    .await
                    .map_err(|_| SnapshotError::Unavailable)?;

                match receiver.borrow().clone() {
                    SnapshotState::Ready(snapshot) => return Ok(*snapshot),
                    SnapshotState::Unavailable => return Err(SnapshotError::Unavailable),
                    SnapshotState::Pending => {}
                }
            }
        })
        .await;

        match wait_for_change {
            Ok(result) => result,
            Err(_) => Err(SnapshotError::Timeout),
        }
    }
}

#[derive(Clone)]
pub(crate) struct RefreshTrigger {
    sender: mpsc::Sender<()>,
}

impl RefreshTrigger {
    fn new(sender: mpsc::Sender<()>) -> Self {
        Self { sender }
    }

    pub(crate) fn request_refresh(&self) -> Result<RefreshQueued, RefreshError> {
        match self.sender.try_send(()) {
            Ok(()) => Ok(RefreshQueued { coalesced: false }),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(RefreshQueued { coalesced: true }),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(RefreshError::Unavailable),
        }
    }
}

pub struct HttpServer {
    local_addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl HttpServer {
    pub async fn bind(options: ServerOptions) -> Result<Self> {
        let bind_addr = resolve_bind_address(options.bind_host(), options.bind_port()).await?;
        let listener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("failed to bind HTTP server to {bind_addr}"))?;
        let local_addr = listener
            .local_addr()
            .context("failed to read bound HTTP server address")?;
        let app = api::router(AppState::new(&options));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let task = tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            });

            if let Err(error) = server.await {
                tracing::error!(%error, "HTTP server exited with an error");
            }
        });

        Ok(Self {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
            task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }

        let _ = self.task.await;
    }
}

pub struct RuntimeChannels {
    snapshot_tx: watch::Sender<SnapshotState>,
    refresh_rx: mpsc::Receiver<()>,
}

impl RuntimeChannels {
    pub fn snapshot_sender(&self) -> &watch::Sender<SnapshotState> {
        &self.snapshot_tx
    }

    pub fn refresh_receiver(&mut self) -> &mut mpsc::Receiver<()> {
        &mut self.refresh_rx
    }
}

pub fn runtime_channels(
    refresh_buffer: usize,
) -> (
    RuntimeChannels,
    watch::Receiver<SnapshotState>,
    mpsc::Sender<()>,
) {
    let (snapshot_tx, snapshot_rx) = watch::channel(SnapshotState::Pending);
    let (refresh_tx, refresh_rx) = mpsc::channel(refresh_buffer);

    (
        RuntimeChannels {
            snapshot_tx,
            refresh_rx,
        },
        snapshot_rx,
        refresh_tx,
    )
}

pub async fn bind_from_workflow(
    workflow_path: impl AsRef<Path>,
    cli_port: Option<u16>,
    snapshot_rx: watch::Receiver<SnapshotState>,
    refresh_tx: mpsc::Sender<()>,
) -> Result<Option<HttpServer>> {
    let workflow_path = workflow_path.as_ref();
    let workflow = WorkflowStore::load(workflow_path.to_path_buf())
        .with_context(|| format!("failed to load workflow from {}", workflow_path.display()))?;
    let config_port = workflow.current().config.server.port;

    if cli_port.or(config_port).is_none() {
        return Ok(None);
    }

    let server = HttpServer::bind(
        ServerOptions::new(snapshot_rx, refresh_tx)
            .with_cli_port(cli_port)
            .with_config_port(config_port),
    )
    .await?;

    Ok(Some(server))
}

async fn resolve_bind_address(host: &str, port: u16) -> Result<SocketAddr> {
    let mut addresses = lookup_host((host, port))
        .await
        .with_context(|| format!("failed to resolve HTTP bind host `{host}`"))?;

    addresses.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("no socket addresses resolved for HTTP bind host `{host}`"),
        )
        .into()
    })
}
