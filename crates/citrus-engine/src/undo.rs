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
            _ => {}
        }
    }
}

#[derive(Default)]
pub struct UndoStack {
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    last_push: Option<Instant>,
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
            .is_some_and(|t| now.duration_since(t) < MERGE_WINDOW);
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
