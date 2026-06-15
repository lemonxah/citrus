//! In-app log console: a `tracing` layer that mirrors every event into a
//! shared ring buffer so the editor's Log tab can show and filter them.
//!
//! [`init`] installs the global subscriber (stdout fmt + this capture layer);
//! [`store`] returns the ring the Log tab reads each frame.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::Level;
use tracing_subscriber::layer::Context;
use tracing_subscriber::{Layer, prelude::*};

/// One captured log record.
pub struct LogEntry {
    pub level: Level,
    pub target: String,
    pub message: String,
    /// Local wall-clock time the event occurred, "HH:MM:SS.mmm".
    pub time: String,
}

/// Ring buffer of recent log entries; oldest dropped past the cap.
pub struct LogRing {
    pub entries: VecDeque<LogEntry>,
    cap: usize,
}

impl LogRing {
    fn new(cap: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(cap.min(1024)),
            cap,
        }
    }

    fn push(&mut self, entry: LogEntry) {
        if self.entries.len() == self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }
}

pub type LogStore = Arc<Mutex<LogRing>>;

static STORE: OnceLock<LogStore> = OnceLock::new();

/// The shared log ring (created on first use).
pub fn store() -> &'static LogStore {
    STORE.get_or_init(|| Arc::new(Mutex::new(LogRing::new(5000))))
}

/// Install the global tracing subscriber: stdout formatting (honouring
/// `RUST_LOG`, default `info`) plus the in-app capture layer. Call once at
/// startup in place of the bare `tracing_subscriber::fmt().init()`.
pub fn init() {
    // `calloop` spams benign "event for non-existence source" WARNs during
    // cursor-grab churn on Wayland; cap it at error so it stays quiet even when
    // RUST_LOG raises the global level.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into())
        .add_directive("calloop=error".parse().expect("valid directive"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(CaptureLayer)
        .init();
}

/// Tracing layer that appends each event to the shared ring.
struct CaptureLayer;

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        store().lock().unwrap().push(LogEntry {
            level: *meta.level(),
            target: meta.target().to_owned(),
            message: visitor.finish(),
            time: chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
        });
    }
}

/// Pulls the `message` field out of an event and appends any other fields as
/// `key=value`, so a structured `tracing::info!(gpu = ?name, "selected")` reads
/// as `selected gpu=...`.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn finish(mut self) -> String {
        if !self.fields.is_empty() {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message.push_str(&self.fields);
        }
        self.message
    }
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }
}
