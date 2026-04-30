//! In-process tracing layer that captures every event into a ring
//! buffer + broadcast channel.
//!
//! The layer is installed once at startup. The Tauri app subscribes to
//! the broadcast and emits `app:log` events for each line; the
//! frontend's Logs page tails those events and renders them.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::broadcast;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context as LayerContext;
use tracing_subscriber::Layer;

const RING_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    /// ISO-8601 UTC timestamp.
    pub ts: String,
    pub level: String,
    pub target: String,
    pub message: String,
}

/// Shared sink installed by the layer. Holds both a ring buffer (so
/// the Logs page has something to render on first load) and a tokio
/// broadcast channel that pushes new lines to subscribers.
pub struct LogSink {
    ring: Mutex<Vec<LogLine>>,
    tx: broadcast::Sender<LogLine>,
}

impl LogSink {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(RING_CAPACITY);
        Arc::new(Self {
            ring: Mutex::new(Vec::with_capacity(RING_CAPACITY)),
            tx,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.ring.lock().map(|r| r.clone()).unwrap_or_default()
    }

    pub fn push(&self, line: LogLine) {
        if let Ok(mut ring) = self.ring.lock() {
            if ring.len() >= RING_CAPACITY {
                ring.remove(0);
            }
            ring.push(line.clone());
        }
        // It's fine if no subscribers — `send` returns Err which we ignore.
        let _ = self.tx.send(line);
    }
}

/// Tracing layer that shovels each event into a `LogSink`.
pub struct ChannelLayer {
    sink: Arc<LogSink>,
}

impl ChannelLayer {
    pub fn new(sink: Arc<LogSink>) -> Self {
        Self { sink }
    }
}

impl<S: Subscriber> Layer<S> for ChannelLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let now: DateTime<Utc> = Utc::now();
        let line = LogLine {
            ts: now.to_rfc3339(),
            level: level_name(*event.metadata().level()).to_string(),
            target: event.metadata().target().to_string(),
            message: visitor.message,
        };
        self.sink.push(line);
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    }
}

/// Extract the rendered `message` field from a tracing event. Other
/// fields are appended as `key=value` pairs so structured logs still
/// surface meaningful content in the UI.
#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message.push_str(value);
        } else {
            self.append_kv(field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let formatted = format!("{value:?}");
        if field.name() == "message" {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message.push_str(&formatted);
        } else {
            self.append_kv(field.name(), &formatted);
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.append_kv(field.name(), &value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.append_kv(field.name(), &value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.append_kv(field.name(), &value.to_string());
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.append_kv(field.name(), &value.to_string());
    }
}

impl MessageVisitor {
    fn append_kv(&mut self, name: &str, value: &str) {
        if !self.message.is_empty() {
            self.message.push(' ');
        }
        self.message.push_str(name);
        self.message.push('=');
        self.message.push_str(value);
    }
}

/// Spawn a tokio task that listens on the sink's broadcast channel and
/// emits `app:log` events for each line. Called once at startup.
pub fn spawn_emitter(app: AppHandle, sink: Arc<LogSink>) {
    tauri::async_runtime::spawn(async move {
        let mut rx = sink.subscribe();
        loop {
            match rx.recv().await {
                Ok(line) => {
                    let _ = app.emit("app:log", &line);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Frontend is behind; drop the lag and keep going.
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[tauri::command]
pub async fn logs_buffer(
    state: tauri::State<'_, Arc<LogSink>>,
) -> Result<Vec<LogLine>, String> {
    Ok(state.snapshot())
}

