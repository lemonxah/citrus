//! Project components, compiled and hot-loaded by the citrus editor
//! (Tools → Build & Reload Components).
//!
//! A component's *runtime* behaviour is `citrus_core::TypedComponent` (no UI);
//! its *editor* inspector is `citrus_editor::Inspect` and its viewport gizmo is
//! `citrus_editor::Gizmo`. Register the runtime in `citrus_register` and the
//! editor traits in `citrus_register_editor`. A shipped game calls only the
//! former (no editor linkage).

use citrus_core::{ComponentCtx, ComponentRegistry, ObjectRef, TypedComponent};
#[cfg(feature = "editor")]
use citrus_editor::{EditorComponents, Gizmo, Inspect, InspectCtx};
use serde::{Deserialize, Serialize};

/// Register runtime behaviour (called by the editor and by a shipped game).
#[unsafe(no_mangle)]
pub fn citrus_register(registry: &mut ComponentRegistry) {
    registry.register::<Orbit>();
    // citrus: new components register here
}

/// Register editor-only traits (inspector + gizmo). Compiled only with the
/// `editor` feature (the editor builds plugins with it); absent in a game.
#[cfg(feature = "editor")]
#[unsafe(no_mangle)]
pub fn citrus_register_editor(editor: &mut EditorComponents) {
    editor.register::<Orbit>();
    // citrus: new components register their editor traits here
}

/// Orbits the owning object around a target object (or its own start point if
/// no target is set).
///
/// A component can't hold a `&Transform` (it owns/serializes its fields), so
/// the target is stored by name and resolved each frame through the in-game
/// API: `ctx.object_transform_named(name)`.
#[derive(Serialize, Deserialize)]
pub struct Orbit {
    pub degrees_per_second: f32,
    pub radius: f32,
    /// Object to orbit around (unset = orbit own start point). Set via the
    /// inspector's object picker; resolved by stable id each frame.
    pub target: ObjectRef,
    #[serde(skip)]
    applied: glam::Vec3,
}

impl Default for Orbit {
    fn default() -> Self {
        Self {
            degrees_per_second: 90.0,
            radius: 1.5,
            target: ObjectRef::NONE,
            applied: glam::Vec3::ZERO,
        }
    }
}

impl TypedComponent for Orbit {
    const NAME: &'static str = "Orbit";

    fn update(&mut self, ctx: &mut ComponentCtx) {
        let angle = ctx.time * self.degrees_per_second.to_radians();
        let offset = glam::Vec3::new(angle.cos(), 0.0, angle.sin()) * self.radius;
        // Orbit around the target object's world position if set and found;
        // otherwise orbit relative to the object's own start point.
        let center = ctx.transform_of(self.target).map(|t| t.translation);
        match center {
            // c + offset is a world coordinate; set_world_position converts it
            // through the parent so a parented orbiter still circles the target.
            Some(c) => ctx.set_world_position(c + offset),
            None => {
                *ctx.translation += offset - self.applied;
                self.applied = offset;
            }
        }
    }
}

#[cfg(feature = "editor")]
impl Inspect for Orbit {
    fn inspector_ui(&mut self, ui: &mut egui::Ui, ctx: &InspectCtx) -> bool {
        let mut changed = false;
        changed |= ui
            .add(egui::Slider::new(&mut self.degrees_per_second, -360.0..=360.0).text("Speed (°/s)"))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut self.radius, 0.0..=10.0).text("Radius"))
            .changed();
        // Object picker dropdown for the orbit target.
        changed |= ctx.object_ref(ui, "Target", &mut self.target);
        changed
    }
}

#[cfg(feature = "editor")]
impl Gizmo for Orbit {}
