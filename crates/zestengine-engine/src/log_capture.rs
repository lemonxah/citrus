//! In-app log console: a `tracing` layer that mirrors every event into a
//! shared ring buffer so the editor's Log tab can show and filter them.
//!
//! [`init`] installs the global subscriber (stdout fmt, plus this capture
//! layer in the editor); [`store`] returns the ring the Log tab reads each
//! frame. A game build has no Log tab, so it installs no capture layer.

#[cfg(feature = "editor")]
use std::collections::VecDeque;
#[cfg(feature = "editor")]
use std::fmt::Write as _;
#[cfg(feature = "editor")]
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(feature = "editor")]
use tracing::Level;
#[cfg(feature = "editor")]
use tracing_subscriber::layer::Context;
#[cfg(feature = "editor")]
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::*;

/// One captured log record.
#[cfg(feature = "editor")]
pub struct LogEntry {
    pub level: Level,
    pub target: String,
    pub message: String,
    /// Local wall-clock time the event occurred, "HH:MM:SS.mmm".
    pub time: String,
}

/// Ring buffer of recent log entries; oldest dropped past the cap.
#[cfg(feature = "editor")]
pub struct LogRing {
    pub entries: VecDeque<LogEntry>,
    cap: usize,
}

#[cfg(feature = "editor")]
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

#[cfg(feature = "editor")]
pub type LogStore = Arc<Mutex<LogRing>>;

#[cfg(feature = "editor")]
static STORE: OnceLock<LogStore> = OnceLock::new();

/// The shared log ring (created on first use).
#[cfg(feature = "editor")]
pub fn store() -> &'static LogStore {
    STORE.get_or_init(|| Arc::new(Mutex::new(LogRing::new(5000))))
}

/// Install the global tracing subscriber: stdout formatting (honouring
/// `RUST_LOG`, default `info`) plus, in the editor, the in-app capture layer.
/// Call once at startup in place of the bare `tracing_subscriber::fmt().init()`.
pub fn init() {
    // `calloop` spams benign "event for non-existence source" WARNs during
    // cursor-grab churn on Wayland, and `sctk_adwaita` (the Wayland title bar)
    // warns about desktop button layouts it doesn't draw (e.g. `icon`); cap
    // both at error so they stay quiet even when RUST_LOG raises the level.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into())
        .add_directive("calloop=error".parse().expect("valid directive"))
        .add_directive("sctk_adwaita=error".parse().expect("valid directive"));
    // Android has no stdout: lines go to logcat (`adb logcat -s zestengine`).
    #[cfg(target_os = "android")]
    let fmt = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(crate::runtime::Logcat);
    #[cfg(not(target_os = "android"))]
    let fmt = tracing_subscriber::fmt::layer();
    let registry = tracing_subscriber::registry().with(filter).with(fmt);
    #[cfg(feature = "editor")]
    registry.with(CaptureLayer).init();
    #[cfg(not(feature = "editor"))]
    registry.init();
}

/// Tracing layer that appends each event to the shared ring.
#[cfg(feature = "editor")]
struct CaptureLayer;

#[cfg(feature = "editor")]
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
#[cfg(feature = "editor")]
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

#[cfg(feature = "editor")]
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

#[cfg(feature = "editor")]
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
