//! Typed asset handles + ref-counted streaming store (CHECKLIST T1 #34).
//!
//! A `Handle<T>` is a cheap, cloneable, ref-counted reference to an asset in an
//! `Assets<T>` store. Dropping the last handle marks the asset for release; a
//! per-frame **time budget** bounds how much loading work happens per frame (the
//! Unreal streaming pattern researched in `UNREAL_RESEARCH_2026-06-18.md`). The
//! store is generic + testable; real decode is plugged via a loader callback.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Load state of an asset slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadState {
    Queued,
    Loading,
    Loaded,
    Failed,
}

/// A typed, ref-counted handle. Cloning bumps the refcount; dropping decrements it.
pub struct Handle<T> {
    id: u32,
    rc: Arc<AtomicU32>,
    _t: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    pub fn id(&self) -> u32 {
        self.id
    }
    /// Strong reference count (this handle included).
    pub fn ref_count(&self) -> u32 {
        self.rc.load(Ordering::Relaxed)
    }
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        self.rc.fetch_add(1, Ordering::Relaxed);
        Self { id: self.id, rc: self.rc.clone(), _t: PhantomData }
    }
}

impl<T> Drop for Handle<T> {
    fn drop(&mut self) {
        self.rc.fetch_sub(1, Ordering::Relaxed);
    }
}

struct Slot<T> {
    path: String,
    state: LoadState,
    asset: Option<T>,
    rc: Arc<AtomicU32>,
}

/// A store of one asset type. `request` returns a handle immediately (state
/// `Queued`); `tick` does up to `budget` loads via the provided loader.
pub struct Assets<T> {
    slots: HashMap<u32, Slot<T>>,
    by_path: HashMap<String, u32>,
    next: u32,
}

impl<T> Default for Assets<T> {
    fn default() -> Self {
        Self { slots: HashMap::new(), by_path: HashMap::new(), next: 1 }
    }
}

impl<T> Assets<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or create) a handle for `path`. De-dupes: the same path returns a handle
    /// to the same slot (shared refcount).
    pub fn request(&mut self, path: &str) -> Handle<T> {
        if let Some(&id) = self.by_path.get(path) {
            let rc = self.slots[&id].rc.clone();
            rc.fetch_add(1, Ordering::Relaxed);
            return Handle { id, rc, _t: PhantomData };
        }
        let id = self.next;
        self.next += 1;
        let rc = Arc::new(AtomicU32::new(1));
        self.slots.insert(
            id,
            Slot { path: path.to_string(), state: LoadState::Queued, asset: None, rc: rc.clone() },
        );
        self.by_path.insert(path.to_string(), id);
        Handle { id, rc, _t: PhantomData }
    }

    pub fn state(&self, h: &Handle<T>) -> Option<LoadState> {
        self.slots.get(&h.id).map(|s| s.state)
    }

    pub fn get(&self, h: &Handle<T>) -> Option<&T> {
        self.slots.get(&h.id).and_then(|s| s.asset.as_ref())
    }

    /// Process up to `budget` queued loads this frame, calling `loader(path)`.
    /// Returns how many were processed. A `None` from the loader marks the slot
    /// `Failed`.
    pub fn tick(&mut self, budget: usize, mut loader: impl FnMut(&str) -> Option<T>) -> usize {
        let queued: Vec<u32> = self
            .slots
            .iter()
            .filter(|(_, s)| s.state == LoadState::Queued)
            .map(|(&id, _)| id)
            .take(budget)
            .collect();
        let mut done = 0;
        for id in queued {
            let path = self.slots[&id].path.clone();
            match loader(&path) {
                Some(a) => {
                    let s = self.slots.get_mut(&id).unwrap();
                    s.asset = Some(a);
                    s.state = LoadState::Loaded;
                }
                None => self.slots.get_mut(&id).unwrap().state = LoadState::Failed,
            }
            done += 1;
        }
        done
    }

    /// Free slots whose last handle has dropped (refcount 0). Returns released ids.
    pub fn collect_unused(&mut self) -> Vec<u32> {
        let dead: Vec<u32> = self
            .slots
            .iter()
            .filter(|(_, s)| s.rc.load(Ordering::Relaxed) == 0)
            .map(|(&id, _)| id)
            .collect();
        for id in &dead {
            if let Some(s) = self.slots.remove(id) {
                self.by_path.remove(&s.path);
            }
        }
        dead
    }

    pub fn loaded_count(&self) -> usize {
        self.slots.values().filter(|s| s.state == LoadState::Loaded).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_by_path_and_shares_refcount() {
        let mut assets: Assets<String> = Assets::new();
        let a = assets.request("tex/foo.png");
        let b = assets.request("tex/foo.png");
        assert_eq!(a.id(), b.id());
        assert_eq!(a.ref_count(), 2);
        drop(b);
        assert_eq!(a.ref_count(), 1);
    }

    #[test]
    fn budget_limits_loads_per_tick() {
        let mut assets: Assets<String> = Assets::new();
        let _h: Vec<_> = (0..5).map(|i| assets.request(&format!("a{i}"))).collect();
        let n = assets.tick(2, |p| Some(format!("loaded:{p}")));
        assert_eq!(n, 2);
        assert_eq!(assets.loaded_count(), 2);
        assets.tick(10, |p| Some(format!("loaded:{p}")));
        assert_eq!(assets.loaded_count(), 5);
    }

    #[test]
    fn loaded_asset_is_retrievable_and_failure_marked() {
        let mut assets: Assets<String> = Assets::new();
        let ok = assets.request("good");
        let bad = assets.request("bad");
        assets.tick(10, |p| if p == "bad" { None } else { Some(p.to_string()) });
        assert_eq!(assets.get(&ok), Some(&"good".to_string()));
        assert_eq!(assets.state(&bad), Some(LoadState::Failed));
    }

    #[test]
    fn dropped_handles_are_collected() {
        let mut assets: Assets<String> = Assets::new();
        let h = assets.request("temp");
        assets.tick(1, |p| Some(p.to_string()));
        let id = h.id();
        drop(h);
        let freed = assets.collect_unused();
        assert_eq!(freed, vec![id]);
        assert_eq!(assets.loaded_count(), 0);
    }
}
