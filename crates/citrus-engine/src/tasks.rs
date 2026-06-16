//! Background-task system for the editor.
//!
//! Heavy CPU work (file parsing, FS scans) runs on worker threads and reports
//! results back over per-task channels; the main thread polls each frame and
//! applies the GPU side (see `EngineApp::apply_task_result`). GPU work that
//! can't move off the main thread (light baking) is driven separately as a
//! per-frame stepped job but still registers a task here for the shared UI.
//!
//! winit/egui/the renderer are pinned to the main thread, and the scene holds
//! non-`Send` `dyn Component`s — so worker closures may only capture `Send`
//! data (paths, asset-side `Scene`/`SceneFile`/`TextureData`), never `self`,
//! the renderer, or the `LoadedScene`. The compiler enforces this at `spawn`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

pub type TaskId = u64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    ImportModel,
    LoadScene,
    Bake,
    FocusReimport,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BakePhase {
    Lightmaps,
    Probes,
    FluxVr,
}

/// Live progress a worker (or the bake stepper) updates and the UI reads.
#[derive(Clone)]
pub enum TaskProgress {
    /// No countable units (parse/scan) — UI shows an indeterminate spinner.
    Indeterminate,
    /// Light-bake detail (counts known up front from the gather).
    Bake {
        lights: u32,
        bounces: u32,
        lightmap_done: u32,
        lightmap_total: u32,
        probe_volumes: u32,
        fluxvr_volumes: u32,
        phase: BakePhase,
    },
}

impl TaskProgress {
    /// 0..1 completion, or None when indeterminate.
    pub fn fraction(&self) -> Option<f32> {
        match self {
            TaskProgress::Indeterminate => None,
            TaskProgress::Bake {
                lightmap_done,
                lightmap_total,
                ..
            } => Some(*lightmap_done as f32 / (*lightmap_total).max(1) as f32),
        }
    }

    /// Short human-readable detail line for the task popup.
    pub fn detail(&self) -> String {
        match self {
            TaskProgress::Indeterminate => "working…".into(),
            TaskProgress::Bake {
                lights,
                bounces,
                lightmap_done,
                lightmap_total,
                probe_volumes,
                fluxvr_volumes,
                phase,
            } => {
                let p = match phase {
                    BakePhase::Lightmaps => "lightmaps",
                    BakePhase::Probes => "probes",
                    BakePhase::FluxVr => "FluxVoxel",
                };
                format!(
                    "{p}: lightmap {lightmap_done}/{lightmap_total} · {lights} lights · \
                     {bounces} bounces · {probe_volumes} probe vols · {fluxvr_volumes} FluxVoxel"
                )
            }
        }
    }
}

/// A finished worker's payload for the main thread to apply (GPU upload etc.).
pub enum TaskPayload {
    /// Nothing to apply (no change found, or a smoke-test task).
    None,
    /// Parsed model ready for `add_asset_scene` on the main thread. `source` is
    /// the project-relative path. Textures referenced by the model ride along in
    /// the `Scene` and are uploaded by `add_asset_scene`.
    Model {
        source: PathBuf,
        scene: Box<citrus_assets::Scene>,
    },
    /// A scene file + its models, parsed off-thread. `prepared` holds GPU
    /// resources already uploaded on the loader thread (when a transfer queue is
    /// available), to be installed on the main thread; None means upload on main.
    Scene {
        path: PathBuf,
        file: Box<citrus_assets::SceneFile>,
        models: std::collections::HashMap<String, citrus_assets::Scene>,
        prepared: Option<std::collections::HashMap<String, citrus_render::PreparedScene>>,
        /// Material textures decoded + uploaded on the loader thread, paired with
        /// the (abs path, srgb) keys to seed the file cache. None on the
        /// single-queue fallback (decoded on the main thread instead).
        material_textures: Option<(Vec<(PathBuf, bool)>, citrus_render::PreparedScene)>,
    },
    /// A focus-scan result: the changed asset (if any) to re-import.
    FocusChanges {
        scene_path: Option<PathBuf>,
        label: String,
    },
}

pub struct TaskResult {
    pub outcome: Result<TaskPayload, String>,
    pub cancelled: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NotifyLevel {
    Info,
    Warn,
}

pub struct Notification {
    pub text: String,
    pub at: Instant,
    pub level: NotifyLevel,
}

/// A live task: its UI-facing handle plus the channel its worker reports on.
/// `rx` is None for the stepped bake (driven on the main thread, completed via
/// `complete_stepped`).
pub struct TaskHandle {
    pub id: TaskId,
    pub label: String,
    /// Task category (reserved for per-type UI such as icons).
    #[allow(dead_code)]
    pub kind: TaskKind,
    pub blocking: bool,
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<Mutex<TaskProgress>>,
}

struct RunningTask {
    handle: TaskHandle,
    rx: Option<mpsc::Receiver<TaskResult>>,
}

pub struct TaskManager {
    next_id: TaskId,
    running: Vec<RunningTask>,
    pub notifications: Vec<Notification>,
}

/// How long a notification stays in the status bar.
const NOTIFY_TTL_SECS: f32 = 8.0;

impl Default for TaskManager {
    fn default() -> Self {
        Self {
            next_id: 1,
            running: Vec::new(),
            notifications: Vec::new(),
        }
    }
}

impl TaskManager {
    /// Spawn a CPU worker task. `f` runs on a std thread with the cancel flag and
    /// a progress writer, returning a payload (or an error string).
    pub fn spawn<F>(
        &mut self,
        label: impl Into<String>,
        kind: TaskKind,
        blocking: bool,
        f: F,
    ) -> TaskId
    where
        F: FnOnce(Arc<AtomicBool>, Arc<Mutex<TaskProgress>>) -> Result<TaskPayload, String>
            + Send
            + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(TaskProgress::Indeterminate));
        let (tx, rx) = mpsc::channel();
        let cancel_run = cancel.clone();
        let cancel_check = cancel.clone();
        let progress_run = progress.clone();
        std::thread::spawn(move || {
            let outcome = f(cancel_run, progress_run);
            let _ = tx.send(TaskResult {
                outcome,
                cancelled: cancel_check.load(Ordering::Relaxed),
            });
        });
        self.running.push(RunningTask {
            handle: TaskHandle {
                id,
                label: label.into(),
                kind,
                blocking,
                cancel,
                progress,
            },
            rx: Some(rx),
        });
        id
    }

    /// Register a main-thread stepped task (the bake). Returns its id; progress
    /// is updated via `progress_of`, completion via `complete_stepped`.
    pub fn register_stepped(
        &mut self,
        label: impl Into<String>,
        kind: TaskKind,
        progress: TaskProgress,
    ) -> (TaskId, Arc<AtomicBool>, Arc<Mutex<TaskProgress>>) {
        let id = self.next_id;
        self.next_id += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(progress));
        self.running.push(RunningTask {
            handle: TaskHandle {
                id,
                label: label.into(),
                kind,
                blocking: false,
                cancel: cancel.clone(),
                progress: progress.clone(),
            },
            rx: None,
        });
        (id, cancel, progress)
    }

    /// Remove a stepped task by id (on completion/cancel of the bake).
    pub fn complete_stepped(&mut self, id: TaskId) {
        self.running.retain(|t| t.handle.id != id);
    }

    /// Drain finished worker tasks (removing them from `running`), returning
    /// owned results so the caller can apply them against `&mut self` without
    /// holding a borrow on the manager.
    pub fn poll_workers(&mut self) -> Vec<TaskResult> {
        let mut done = Vec::new();
        self.running.retain_mut(|t| {
            let Some(rx) = t.rx.as_ref() else {
                return true; // stepped task; not channel-driven
            };
            match rx.try_recv() {
                Ok(r) => {
                    done.push(r);
                    false
                }
                Err(mpsc::TryRecvError::Empty) => true,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Worker died without sending — drop the task.
                    done.push(TaskResult {
                        outcome: Err("worker thread died".into()),
                        cancelled: t.handle.cancel.load(Ordering::Relaxed),
                    });
                    false
                }
            }
        });
        done
    }

    pub fn cancel(&mut self, id: TaskId) {
        if let Some(t) = self.running.iter().find(|t| t.handle.id == id) {
            t.handle.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Running tasks the UI lists (excludes blocking tasks, which show a modal).
    pub fn visible(&self) -> impl Iterator<Item = &TaskHandle> {
        self.running
            .iter()
            .map(|t| &t.handle)
            .filter(|h| !h.blocking)
    }

    /// First blocking task (drives the modal overlay), if any.
    pub fn blocking(&self) -> Option<&TaskHandle> {
        self.running.iter().map(|t| &t.handle).find(|h| h.blocking)
    }

    /// (count, overall 0..1) over visible tasks for the status-bar button.
    pub fn aggregate(&self) -> Option<(usize, f32)> {
        let visible: Vec<&TaskHandle> = self.visible().collect();
        if visible.is_empty() {
            return None;
        }
        let mut sum = 0.0;
        let mut n = 0;
        for h in &visible {
            if let Some(f) = h.progress.lock().unwrap().fraction() {
                sum += f;
                n += 1;
            }
        }
        let frac = if n > 0 { sum / n as f32 } else { 0.0 };
        Some((visible.len(), frac))
    }

    pub fn notify(&mut self, text: impl Into<String>, level: NotifyLevel) {
        self.notifications.push(Notification {
            text: text.into(),
            at: Instant::now(),
            level,
        });
    }

    pub fn prune_notifications(&mut self) {
        self.notifications
            .retain(|n| n.at.elapsed().as_secs_f32() < NOTIFY_TTL_SECS);
    }
}
