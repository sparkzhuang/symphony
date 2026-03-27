use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, EnvFilter, Registry};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoggingConfig {
    pub logs_root: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoggingInit {
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SharedLogSink {
    state: Arc<Mutex<SharedLogSinkState>>,
}

#[derive(Debug)]
struct SharedLogSinkState {
    file: Option<File>,
    file_failed: bool,
}

pub fn init_logging(config: &LoggingConfig) -> anyhow::Result<LoggingInit> {
    let (sink, warnings) = SharedLogSink::from_config(config);
    let subscriber = subscriber_with_sink(EnvFilter::from_default_env(), sink);
    tracing::subscriber::set_global_default(subscriber)
        .context("failed to install tracing subscriber")?;
    for warning in &warnings {
        eprintln!("{warning}");
    }
    Ok(LoggingInit { warnings })
}

pub fn subscriber_with_sink(
    env_filter: EnvFilter,
    sink: SharedLogSink,
) -> impl tracing::Subscriber + Send + Sync {
    Registry::default().with(env_filter).with(
        fmt::layer()
            .json()
            .with_target(false)
            .flatten_event(true)
            .with_writer(sink),
    )
}

impl LoggingConfig {
    pub fn log_file_path(&self) -> Option<PathBuf> {
        self.logs_root
            .as_ref()
            .map(|root| root.join("log").join("symphony.log"))
    }
}

impl SharedLogSink {
    pub fn from_config(config: &LoggingConfig) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let file = match config.log_file_path() {
            Some(path) => match open_log_file(&path) {
                Ok(file) => Some(file),
                Err(error) => {
                    warnings.push(format!(
                        "structured log file sink disabled for {}: {error:#}",
                        path.display()
                    ));
                    None
                }
            },
            None => None,
        };

        (
            Self {
                state: Arc::new(Mutex::new(SharedLogSinkState {
                    file,
                    file_failed: false,
                })),
            },
            warnings,
        )
    }

    pub fn stdout_only() -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedLogSinkState {
                file: None,
                file_failed: false,
            })),
        }
    }
}

impl<'a> MakeWriter<'a> for SharedLogSink {
    type Writer = SharedLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedLogWriter {
            state: Arc::clone(&self.state),
        }
    }
}

pub struct SharedLogWriter {
    state: Arc<Mutex<SharedLogSinkState>>,
}

impl Write for SharedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(buf)?;

        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("log sink lock poisoned"))?;
        if let Some(file) = state.file.as_mut() {
            if let Err(error) = file.write_all(buf) {
                state.file = None;
                state.file_failed = true;
                let _ = writeln!(
                    stderr,
                    "structured log file sink disabled after write failure: {error}"
                );
            }
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().lock().flush()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("log sink lock poisoned"))?;
        if let Some(file) = state.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

fn open_log_file(path: &Path) -> anyhow::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("log file path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

#[cfg(test)]
mod tests {
    use super::{subscriber_with_sink, LoggingConfig, SharedLogSink};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tracing::info;
    use tracing_subscriber::EnvFilter;

    #[test]
    fn resolves_default_log_file_path_under_logs_root() {
        let config = LoggingConfig {
            logs_root: Some(PathBuf::from("/tmp/symphony")),
        };

        assert_eq!(
            config.log_file_path(),
            Some(PathBuf::from("/tmp/symphony/log/symphony.log"))
        );
    }

    #[test]
    fn creates_json_logs_with_issue_and_session_fields() {
        let sink = SharedLogSink::stdout_only();
        let subscriber = subscriber_with_sink(EnvFilter::new("info"), sink);
        let _guard = tracing::subscriber::set_default(subscriber);

        info!(
            issue_id = "issue-123",
            issue_identifier = "SPA-16",
            session_id = "thread-1-turn-2",
            action = "streaming",
            "agent event"
        );
    }

    #[test]
    fn sink_setup_fails_on_invalid_logs_root_today() {
        let root = temp_path("symphony-log-root");
        fs::write(&root, "not a directory").expect("should create sentinel file");

        let (_sink, warnings) = SharedLogSink::from_config(&LoggingConfig {
            logs_root: Some(root),
        });

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("structured log file sink disabled"));
    }

    fn temp_path(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{unique}"))
    }
}
