//! Project components, compiled and hot-loaded by the citrus editor
//! (Tools → Build & Reload Components). Add a struct, implement
//! [`TypedComponent`], and register it below.

use citrus_editor::{ComponentCtx, ComponentRegistry, TypedComponent};
use serde::{Deserialize, Serialize};

/// Called by the editor after this plugin is (re)loaded.
#[unsafe(no_mangle)]
pub fn citrus_register(registry: &mut ComponentRegistry) {
    registry.register::<Orbit>();
    // citrus: new components register here
}

/// Example: circle around the object's authored position.
#[derive(Serialize, Deserialize)]
pub struct Orbit {
    pub degrees_per_second: f32,
    pub radius: f32,
    #[serde(skip)]
    applied: glam::Vec3,
}

impl Default for Orbit {
    fn default() -> Self {
        Self {
            degrees_per_second: 90.0,
            radius: 1.5,
            applied: glam::Vec3::ZERO,
        }
    }
}

impl TypedComponent for Orbit {
    const NAME: &'static str = "Orbit";

    fn inspector_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        changed |= ui
            .add(
                egui::Slider::new(&mut self.degrees_per_second, -360.0..=360.0).text("Speed (°/s)"),
            )
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut self.radius, 0.0..=10.0).text("Radius"))
            .changed();
        changed
    }

    fn update(&mut self, ctx: &mut ComponentCtx) {
        let angle = ctx.time * self.degrees_per_second.to_radians();
        let offset = glam::Vec3::new(angle.cos(), 0.0, angle.sin()) * self.radius;
        *ctx.translation += offset - self.applied;
        self.applied = offset;
    }
}
