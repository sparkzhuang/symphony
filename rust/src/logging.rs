use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{span, Event, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::{registry::LookupSpan, EnvFilter, Layer, Registry};

const DEFAULT_LOG_RELATIVE_PATH: &str = "log/symphony.log";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggingConfig {
    pub log_file_path: Option<PathBuf>,
}

impl LoggingConfig {
    pub fn from_logs_root(logs_root: Option<&Path>) -> Self {
        Self {
            log_file_path: logs_root.map(default_log_file),
        }
    }
}

pub fn default_log_file(logs_root: impl AsRef<Path>) -> PathBuf {
    logs_root.as_ref().join(DEFAULT_LOG_RELATIVE_PATH)
}

pub fn init_logging(config: &LoggingConfig) -> Result<()> {
    let subscriber = subscriber_with_writer(CombinedMakeWriter::new(
        StdoutMakeWriter,
        config.log_file_path.clone(),
    ));

    tracing::subscriber::set_global_default(subscriber)
        .context("failed to install structured logging subscriber")
}

pub fn subscriber_with_writer<W>(writer: W) -> impl Subscriber + Send + Sync
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::default().add_directive(LevelFilter::INFO.into()));

    Registry::default()
        .with(env_filter)
        .with(JsonLogLayer::new(writer))
}

#[derive(Debug, Clone, Default)]
struct JsonSpanFields(BTreeMap<String, Value>);

#[derive(Debug, Default)]
struct JsonFieldVisitor {
    fields: BTreeMap<String, Value>,
}

impl tracing::field::Visit for JsonFieldVisitor {
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), Value::Bool(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), Value::Number(value.into()));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), Value::Number(value.into()));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), Value::String(value.to_owned()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_owned(),
            Value::String(format!("{value:?}").trim_matches('"').to_owned()),
        );
    }
}

struct JsonLogLayer<W> {
    writer: W,
}

impl<W> JsonLogLayer<W> {
    fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<S, W> Layer<S> for JsonLogLayer<W>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: LayerContext<'_, S>) {
        let mut visitor = JsonFieldVisitor::default();
        attrs.record(&mut visitor);

        if let Some(span) = ctx.span(id) {
            span.extensions_mut()
                .insert(JsonSpanFields(visitor.fields.into_iter().collect()));
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: LayerContext<'_, S>) {
        let mut visitor = JsonFieldVisitor::default();
        values.record(&mut visitor);

        if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            let fields = extensions.get_mut::<JsonSpanFields>();

            match fields {
                Some(existing) => {
                    for (key, value) in visitor.fields {
                        existing.0.insert(key, value);
                    }
                }
                None => {
                    extensions.insert(JsonSpanFields(visitor.fields.into_iter().collect()));
                }
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: LayerContext<'_, S>) {
        let mut visitor = JsonFieldVisitor::default();
        event.record(&mut visitor);

        let metadata = event.metadata();
        let mut payload = Map::new();
        payload.insert(
            "timestamp".to_owned(),
            Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
        payload.insert(
            "level".to_owned(),
            Value::String(metadata.level().as_str().to_owned()),
        );
        payload.insert(
            "target".to_owned(),
            Value::String(metadata.target().to_owned()),
        );

        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(fields) = span.extensions().get::<JsonSpanFields>() {
                    for (key, value) in &fields.0 {
                        payload.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
            }
        }

        for (key, value) in visitor.fields {
            payload.insert(key, value);
        }

        if let Ok(line) = serde_json::to_vec(&payload) {
            let mut writer = self.writer.make_writer();
            let _ = writer.write_all(&line);
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
        }
    }
}

#[derive(Clone)]
struct CombinedMakeWriter<P> {
    primary: P,
    log_file_path: Option<PathBuf>,
    failure_reported: Arc<AtomicBool>,
}

impl<P> CombinedMakeWriter<P> {
    fn new(primary: P, log_file_path: Option<PathBuf>) -> Self {
        Self {
            primary,
            log_file_path,
            failure_reported: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<'writer, P> MakeWriter<'writer> for CombinedMakeWriter<P>
where
    P: MakeWriter<'writer> + Clone,
{
    type Writer = CombinedWriter<P::Writer>;

    fn make_writer(&'writer self) -> Self::Writer {
        let file = self
            .log_file_path
            .as_deref()
            .and_then(|path| open_log_file_resiliently(path, &self.failure_reported));

        CombinedWriter {
            primary: self.primary.make_writer(),
            file,
            failure_reported: Arc::clone(&self.failure_reported),
        }
    }
}

fn open_log_file_resiliently(path: &Path, failure_reported: &Arc<AtomicBool>) -> Option<File> {
    if let Some(parent) = path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            report_sink_failure(
                failure_reported,
                &format!(
                    "structured log directory unavailable at {}: {error}",
                    parent.display()
                ),
            );
            return None;
        }
    }

    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => Some(file),
        Err(error) => {
            report_sink_failure(
                failure_reported,
                &format!(
                    "structured log file unavailable at {}: {error}",
                    path.display()
                ),
            );
            None
        }
    }
}

struct CombinedWriter<W> {
    primary: W,
    file: Option<File>,
    failure_reported: Arc<AtomicBool>,
}

impl<W> Write for CombinedWriter<W>
where
    W: Write,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Err(error) = self.primary.write_all(buf) {
            report_sink_failure(
                &self.failure_reported,
                &format!("primary structured log sink failed: {error}"),
            );
        }
        if let Some(file) = &mut self.file {
            if let Err(error) = file.write_all(buf) {
                report_sink_failure(
                    &self.failure_reported,
                    &format!("file structured log sink failed: {error}"),
                );
                self.file = None;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Err(error) = self.primary.flush() {
            report_sink_failure(
                &self.failure_reported,
                &format!("primary structured log flush failed: {error}"),
            );
        }
        if let Some(file) = &mut self.file {
            if let Err(error) = file.flush() {
                report_sink_failure(
                    &self.failure_reported,
                    &format!("file structured log flush failed: {error}"),
                );
                self.file = None;
            }
        }
        Ok(())
    }
}

fn report_sink_failure(flag: &AtomicBool, message: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        let _ = writeln!(io::stderr(), "{message}");
    }
}

#[derive(Clone, Default)]
struct StdoutMakeWriter;

impl<'writer> MakeWriter<'writer> for StdoutMakeWriter {
    type Writer = Stdout;

    fn make_writer(&'writer self) -> Self::Writer {
        io::stdout()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use tracing::{info, info_span};

    #[derive(Clone, Default)]
    struct SharedBufferMakeWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl SharedBufferMakeWriter {
        fn contents(&self) -> String {
            let bytes = self.buffer.lock().expect("buffer lock should succeed");
            String::from_utf8(bytes.clone()).expect("buffer should contain utf-8 logs")
        }
    }

    struct SharedBufferWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for SharedBufferWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer
                .lock()
                .expect("buffer lock should succeed")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for SharedBufferMakeWriter {
        type Writer = SharedBufferWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            SharedBufferWriter {
                buffer: Arc::clone(&self.buffer),
            }
        }
    }

    #[test]
    fn default_log_file_matches_elixir_layout() {
        assert_eq!(
            default_log_file("/tmp/symphony"),
            PathBuf::from("/tmp/symphony/log/symphony.log")
        );
    }

    #[test]
    fn structured_logs_include_issue_and_session_context() {
        let writer = SharedBufferMakeWriter::default();
        let subscriber = subscriber_with_writer(writer.clone());
        let dispatch = tracing::Dispatch::new(subscriber);

        tracing::dispatcher::with_default(&dispatch, || {
            let span = info_span!(
                "issue_run",
                issue_id = "f212628c-11f2-4047-893d-ded2fef164d4",
                issue_identifier = "SPA-16",
                session_id = "thread-1-turn-1"
            );
            let _entered = span.enter();
            info!(message = "dispatching issue");
        });

        let line = writer.contents();
        let record: Value =
            serde_json::from_str(line.trim()).expect("log line should be valid json");

        assert_eq!(
            record["issue_id"],
            Value::String("f212628c-11f2-4047-893d-ded2fef164d4".to_owned())
        );
        assert_eq!(
            record["issue_identifier"],
            Value::String("SPA-16".to_owned())
        );
        assert_eq!(
            record["session_id"],
            Value::String("thread-1-turn-1".to_owned())
        );
    }

    #[test]
    fn file_sink_failures_do_not_drop_primary_logs() {
        let primary = SharedBufferMakeWriter::default();
        let subscriber = subscriber_with_writer(CombinedMakeWriter::new(
            primary.clone(),
            Some(PathBuf::from("/dev/null/blocked")),
        ));
        let dispatch = tracing::Dispatch::new(subscriber);

        tracing::dispatcher::with_default(&dispatch, || {
            info!(message = "still emitted");
        });

        assert!(primary.contents().contains("still emitted"));
    }

    #[test]
    fn file_sink_writes_logs_when_directory_is_available() {
        let logs_root = std::env::temp_dir().join(format!("symphony-logs-{}", std::process::id()));
        let log_file = default_log_file(&logs_root);
        let subscriber = subscriber_with_writer(CombinedMakeWriter::new(
            SharedBufferMakeWriter::default(),
            Some(log_file.clone()),
        ));
        let dispatch = tracing::Dispatch::new(subscriber);

        tracing::dispatcher::with_default(&dispatch, || {
            info!(message = "persisted log line");
        });

        let written = std::fs::read_to_string(&log_file).expect("log file should be written");
        let _ = std::fs::remove_dir_all(&logs_root);

        assert!(written.contains("persisted log line"));
    }
}
