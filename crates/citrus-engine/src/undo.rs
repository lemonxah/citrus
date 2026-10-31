//! Undo/redo history.
//!
//! Entries are recorded by diffing a pre-frame snapshot of the selection
//! against the post-frame state, so every edit path (gizmo drags, inspector
//! fields, material sliders, drag & drop assignment) is captured without
//! per-widget bookkeeping. Continuous gestures coalesce into single entries.
//!
//! Deliberately NOT undoable (user decision): object deletion (when it
//! exists).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use citrus_editor::MaterialModel;
use glam::{Quat, Vec3};

/// How long after the last merge a gesture stays "open" for coalescing.
const MERGE_WINDOW: Duration = Duration::from_millis(700);

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectState {
    pub name: String,
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
    pub cast_shadows: citrus_core::ShadowCasting,
    /// Components as (registry name, RON). Diffable and restorable.
    pub components: Vec<(String, String)>,
}

#[derive(Clone)]
pub enum UndoEntry {
    /// Transform / rename of a scene object.
    Object {
        index: usize,
        before: ObjectState,
        after: ObjectState,
    },
    /// Property edit of a scene material.
    Material {
        index: usize,
        before: Box<MaterialModel>,
        after: Box<MaterialModel>,
    },
    /// Material slot assignment on an object.
    Assign {
        object: usize,
        /// Which material slot (0 = primary, 1.. = extra slots).
        slot: usize,
        before: usize,
        after: usize,
    },
    /// Property edit of a `.material` file opened in the Inspector.
    FileMaterial {
        path: PathBuf,
        before: Box<MaterialModel>,
        after: Box<MaterialModel>,
    },
    /// One atomic multi-object edit (a multi-select gesture): all sub-entries are
    /// undone/redone together as a single history step, and coalesce as a unit so a
    /// continuous multi-object drag is ONE entry, not hundreds of interleaved ones.
    Group(Vec<UndoEntry>),
}

impl UndoEntry {
    /// Same gesture target? (used for coalescing)
    fn same_target(&self, other: &UndoEntry) -> bool {
        match (self, other) {
            (UndoEntry::Object { index: a, .. }, UndoEntry::Object { index: b, .. }) => a == b,
            (UndoEntry::Material { index: a, .. }, UndoEntry::Material { index: b, .. }) => a == b,
            (UndoEntry::FileMaterial { path: a, .. }, UndoEntry::FileMaterial { path: b, .. }) => {
                a == b
            }
            // Same gesture if the groups cover the same targets in the same order
            // (record_edits builds them stably: anchor first, then the others).
            (UndoEntry::Group(a), UndoEntry::Group(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same_target(y))
            }
            _ => false,
        }
    }

    /// Merge `newer` into self (keep self.before, take newer.after).
    fn absorb(&mut self, newer: UndoEntry) {
        match (self, newer) {
            (UndoEntry::Object { after, .. }, UndoEntry::Object { after: new, .. }) => {
                *after = new;
            }
            (UndoEntry::Material { after, .. }, UndoEntry::Material { after: new, .. }) => {
                *after = new;
            }
            (UndoEntry::FileMaterial { after, .. }, UndoEntry::FileMaterial { after: new, .. }) => {
                *after = new;
            }
            // Absorb sub-entry by sub-entry (same structure, guaranteed by
            // `same_target`): each keeps its own `before`, takes the newest `after`.
            (UndoEntry::Group(a), UndoEntry::Group(new)) => {
                for (x, y) in a.iter_mut().zip(new) {
                    x.absorb(y);
                }
            }
            _ => {}
        }
    }
}

#[derive(Default)]
pub struct UndoStack {
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    last_push: Option<Instant>,
    /// A paint stroke is open: its edits coalesce however long it pauses.
    stroke: bool,
}

impl UndoStack {
    /// Record an edit. Coalesces with the previous entry only while a continuous
    /// gesture is active (`gesture_active`, e.g. a pointer-held slider/handle drag)
    /// AND it targets the same thing within the merge window — so one drag is one
    /// entry, but two DISCRETE edits (typed value, separate clicks) each get their
    /// own entry instead of silently merging (which made them un-undoable). `Assign`
    /// entries never coalesce.
    pub fn record(&mut self, entry: UndoEntry, gesture_active: bool) {
        self.redo.clear();
        let now = Instant::now();
        let in_window = self
            .last_push
            .is_some_and(|t| self.stroke || now.duration_since(t) < MERGE_WINDOW);
        let coalesce = gesture_active
            && !matches!(entry, UndoEntry::Assign { .. })
            && in_window
            && self
                .undo
                .last()
                .is_some_and(|last| last.same_target(&entry));
        if coalesce {
            self.undo.last_mut().unwrap().absorb(entry);
        } else {
            self.undo.push(entry);
            if self.undo.len() > 256 {
                self.undo.remove(0);
            }
        }
        self.last_push = Some(now);
    }

    /// Open or close a paint stroke. Opening one never merges into the
    /// previous entry, and its edits then coalesce into one entry until it
    /// closes, so one stroke is one undo step.
    pub fn set_stroke(&mut self, stroking: bool) {
        if stroking && !self.stroke {
            self.last_push = None;
        }
        self.stroke = stroking;
    }

    pub fn pop_undo(&mut self) -> Option<UndoEntry> {
        let entry = self.undo.pop()?;
        self.redo.push(entry.clone());
        self.last_push = None; // never coalesce across an undo
        Some(entry)
    }

    pub fn pop_redo(&mut self) -> Option<UndoEntry> {
        let entry = self.redo.pop()?;
        self.undo.push(entry.clone());
        self.last_push = None;
        Some(entry)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Drop everything (scene reload invalidates indices).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.last_push = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(pins: &str) -> ObjectState {
        ObjectState {
            name: "Skirt".into(),
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            cast_shadows: Default::default(),
            components: vec![("Mesh Cloth".into(), format!("(paint: (pins: \"{pins}\"))"))],
        }
    }

    fn edit(before: &str, after: &str) -> UndoEntry {
        UndoEntry::Object { index: 3, before: state(before), after: state(after) }
    }

    fn after_of(e: &UndoEntry) -> &str {
        match e {
            UndoEntry::Object { after, .. } => &after.components[0].1,
            _ => panic!("object entry"),
        }
    }

    #[test]
    fn a_paint_stroke_is_one_undo_step() {
        let mut stack = UndoStack::default();
        stack.record(edit("a", "b"), false);
        stack.set_stroke(true);
        stack.record(edit("b", "c"), true);
        std::thread::sleep(MERGE_WINDOW + Duration::from_millis(50));
        stack.record(edit("c", "d"), true);
        stack.record(edit("d", "e"), true);
        stack.set_stroke(false);
        // A second stroke right after starts its own entry.
        stack.set_stroke(true);
        stack.record(edit("e", "f"), true);
        stack.set_stroke(false);

        let second = stack.pop_undo().unwrap();
        assert!(after_of(&second).contains("\"f\""));
        let stroke = stack.pop_undo().unwrap();
        match &stroke {
            UndoEntry::Object { before, after, .. } => {
                assert_eq!(before, &state("b"), "the stroke undoes to its start, despite the pause");
                assert_eq!(after, &state("e"));
            }
            _ => panic!(),
        }
        assert!(stack.can_undo(), "the edit before the stroke stays its own step");
        stack.pop_undo();
        assert!(!stack.can_undo());
        assert!(after_of(&stack.pop_redo().unwrap()).contains("\"b\""));
    }
}
