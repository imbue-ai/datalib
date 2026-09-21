//! A `tracing` layer that turns every event into a `log` row, so a
//! datalib process's own lines land in the store beside what its steps
//! said — in the shape the runner gives a step's structured stderr:
//! the message, level, target and thread as columns, everything else
//! the event carried as `fields`.

use std::sync::Weak;

use app_schema::runs::{LogLevel, LogRow};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::store::{now_split, LogSink};

pub use datalib_log_filter::DEFAULT_LOG_FILTER;

pub struct StoreLayer {
    /// Weak on purpose: the subscriber is global and lives until the
    /// process ends, and a strong handle from it would keep the writer
    /// from ever being dropped — which is what flushes its last lines.
    sink: Weak<dyn LogSink>,
}

impl StoreLayer {
    pub fn new<W: LogSink + 'static>(sink: Weak<W>) -> Self {
        Self { sink }
    }
}

impl<S: tracing::Subscriber> Layer<S> for StoreLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let Some(sink) = self.sink.upgrade() else {
            return;
        };
        sink.log(row_from(event));
    }
}

pub fn level_of(level: &tracing::Level) -> LogLevel {
    match *level {
        tracing::Level::TRACE => LogLevel::Trace,
        tracing::Level::DEBUG => LogLevel::Debug,
        tracing::Level::INFO => LogLevel::Info,
        tracing::Level::WARN => LogLevel::Warn,
        tracing::Level::ERROR => LogLevel::Error,
    }
}

fn row_from(event: &tracing::Event<'_>) -> LogRow {
    let meta = event.metadata();
    let mut fields = Fields::default();
    event.record(&mut fields);
    // The same two keys tracing-subscriber's JSON envelope carries, so
    // a line the server wrote reads like one a step wrote.
    if let Some(file) = meta.file() {
        fields.other.insert("filename".into(), file.into());
    }
    if let Some(line) = meta.line() {
        fields.other.insert("line_number".into(), line.into());
    }
    let (ts_utc, tz_offset) = now_split();
    let thread = std::thread::current();
    let thread = thread
        .name()
        .map(str::to_string)
        .unwrap_or_else(|| format!("{:?}", thread.id()));
    LogRow {
        ts_utc,
        tz_offset,
        level: level_of(meta.level()).as_str().into(),
        target: Some(meta.target().into()),
        thread: Some(thread),
        msg: fields.message.unwrap_or_default(),
        fields: (!fields.other.is_empty())
            .then(|| serde_json::Value::Object(fields.other).to_string()),
        ..Default::default()
    }
}

/// The event's fields, with `message` set apart from the rest.
#[derive(Default)]
struct Fields {
    message: Option<String>,
    other: serde_json::Map<String, serde_json::Value>,
}

impl Fields {
    fn put(&mut self, field: &Field, value: serde_json::Value) {
        self.other.insert(field.name().into(), value);
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(text);
        } else {
            self.put(field, text.into());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.into());
        } else {
            self.put(field, value.into());
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, value.into());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, value.into());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, value.into());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, value.to_string().into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Default)]
    struct Caught(Mutex<Vec<LogRow>>);

    impl LogSink for Caught {
        fn log(&self, row: LogRow) {
            self.0.lock().unwrap().push(row);
        }
    }

    fn catch(f: impl FnOnce()) -> Vec<LogRow> {
        let caught = Arc::new(Caught::default());
        let subscriber =
            tracing_subscriber::registry().with(StoreLayer::new(Arc::downgrade(&caught)));
        tracing::subscriber::with_default(subscriber, f);
        let rows = std::mem::take(&mut *caught.0.lock().unwrap());
        rows
    }

    #[test]
    fn an_event_becomes_one_row_in_the_envelope_shape() {
        let rows = catch(|| {
            tracing::warn!(target: "datalib_http::worker", job = "j1", n = 3u64, "claim {}", "failed");
        });
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.level, "warn");
        assert_eq!(r.msg, "claim failed");
        assert_eq!(r.target.as_deref(), Some("datalib_http::worker"));
        assert!(r.thread.is_some());
        assert!(r.ts_utc.ends_with("+00:00"), "{}", r.ts_utc);
        let fields: serde_json::Value = serde_json::from_str(r.fields.as_ref().unwrap()).unwrap();
        assert_eq!(fields["job"], "j1");
        assert_eq!(fields["n"], 3);
        assert!(fields["filename"]
            .as_str()
            .unwrap()
            .ends_with("tracing_layer.rs"));
        assert!(fields["line_number"].is_number());
        // The writer binds these; the layer leaves them alone.
        assert_eq!(r.run_id, None);
        assert_eq!(r.process_id, "");
        assert_eq!(r.step, None);
    }

    /// Dropping the writer is what flushes it, so the layer must not be
    /// what keeps it alive.
    #[test]
    fn a_gone_sink_drops_the_line() {
        let caught = Arc::new(Caught::default());
        let weak = Arc::downgrade(&caught);
        drop(caught);
        let subscriber = tracing_subscriber::registry().with(StoreLayer::new(weak));
        tracing::subscriber::with_default(subscriber, || tracing::info!("nobody home"));
    }

    #[test]
    fn every_tracing_level_has_a_word() {
        for (l, word) in [
            (tracing::Level::TRACE, "trace"),
            (tracing::Level::DEBUG, "debug"),
            (tracing::Level::INFO, "info"),
            (tracing::Level::WARN, "warn"),
            (tracing::Level::ERROR, "error"),
        ] {
            assert_eq!(level_of(&l).as_str(), word);
        }
    }
}
