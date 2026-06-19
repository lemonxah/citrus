//! Event / messaging bus (ENGINE_FEATURE_CHECKLIST T1 #30).
//!
//! A data-driven, double-buffered event queue (the Bevy-events pattern): gameplay
//! code `emit`s named events with a typed payload; readers `drain` the previous
//! frame's events. Double buffering means an event emitted in frame N is readable
//! through frame N+1 regardless of system order, then dropped — no unbounded growth,
//! no missed events from ordering.

use crate::savegame::SaveValue;

/// One queued message: a name + optional typed payload (reuses `SaveValue`).
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub name: String,
    pub data: Option<SaveValue>,
}

/// Double-buffered event bus. Call [`EventBus::swap`] once per frame (the runtime
/// does this) to advance buffers.
#[derive(Default)]
pub struct EventBus {
    /// Events emitted this frame.
    current: Vec<Event>,
    /// Events emitted last frame (still readable).
    previous: Vec<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Emit an event with no payload.
    pub fn signal(&mut self, name: impl Into<String>) {
        self.current.push(Event { name: name.into(), data: None });
    }

    /// Emit an event with a typed payload.
    pub fn emit(&mut self, name: impl Into<String>, data: impl Into<SaveValue>) {
        self.current.push(Event { name: name.into(), data: Some(data.into()) });
    }

    /// All currently-readable events (this frame + last frame).
    pub fn iter(&self) -> impl Iterator<Item = &Event> {
        self.previous.iter().chain(self.current.iter())
    }

    /// Readable events of a given name.
    pub fn read<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Event> {
        self.iter().filter(move |e| e.name == name)
    }

    /// True if any readable event has this name.
    pub fn any(&self, name: &str) -> bool {
        self.read(name).next().is_some()
    }

    /// Advance the frame: last frame's events drop, this frame's become readable
    /// next. Call exactly once per frame.
    pub fn swap(&mut self) {
        self.previous.clear();
        std::mem::swap(&mut self.previous, &mut self.current);
    }

    /// Drop everything (e.g. on scene change).
    pub fn clear(&mut self) {
        self.current.clear();
        self.previous.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_readable_for_one_frame_after_emit() {
        let mut bus = EventBus::new();
        bus.signal("player.died");
        bus.emit("score.add", 100i64);
        // Same frame: readable.
        assert!(bus.any("player.died"));
        assert_eq!(bus.read("score.add").count(), 1);

        bus.swap(); // frame N -> still readable (was "current", now "previous")
        assert!(bus.any("player.died"));
        assert_eq!(
            bus.read("score.add").next().unwrap().data,
            Some(SaveValue::Int(100))
        );

        bus.swap(); // frame N+1 -> dropped
        assert!(!bus.any("player.died"));
        assert_eq!(bus.iter().count(), 0);
    }

    #[test]
    fn filters_by_name() {
        let mut bus = EventBus::new();
        bus.signal("a");
        bus.signal("b");
        bus.signal("a");
        assert_eq!(bus.read("a").count(), 2);
        assert_eq!(bus.read("b").count(), 1);
        assert_eq!(bus.read("c").count(), 0);
    }
}
