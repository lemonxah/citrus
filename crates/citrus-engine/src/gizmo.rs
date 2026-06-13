//! Transform gizmo driver.
//!
//! Drives `transform_gizmo::Gizmo` directly instead of via the crate's egui
//! wrapper: the wrapper registers an interact widget under the cursor every
//! frame, which would make egui claim the pointer over the whole viewport
//! and break click-to-pick. Here the gizmo hit-tests itself
//! (`pick_preview`) and the viewport decides who gets the click.

use egui::{Pos2, Ui};
use glam::{DQuat, DVec3, Mat4, Quat, Vec3};
use transform_gizmo_egui::math::Transform;
use transform_gizmo_egui::{
    EnumSet, Gizmo, GizmoConfig, GizmoInteraction, GizmoMode, GizmoOrientation, GizmoVisuals,
    enum_set,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GizmoTool {
    Move,
    Rotate,
    Scale,
}

/// Where the gizmo sits and what transformations pivot around.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PivotMode {
    /// The object's own origin (pivot point).
    Origin,
    /// Center of the mesh bounds (Unity's "Center").
    Center,
}

impl GizmoTool {
    fn modes(self) -> EnumSet<GizmoMode> {
        match self {
            Self::Move => enum_set!(
                GizmoMode::TranslateX
                    | GizmoMode::TranslateY
                    | GizmoMode::TranslateZ
                    | GizmoMode::TranslateXY
                    | GizmoMode::TranslateXZ
                    | GizmoMode::TranslateYZ
            ),
            Self::Rotate => enum_set!(GizmoMode::RotateX | GizmoMode::RotateY | GizmoMode::RotateZ),
            Self::Scale => enum_set!(
                GizmoMode::ScaleX | GizmoMode::ScaleY | GizmoMode::ScaleZ | GizmoMode::ScaleUniform
            ),
        }
    }
}

pub struct GizmoState {
    gizmo: Gizmo,
    pub tool: GizmoTool,
    pub pivot: PivotMode,
    /// Gizmo axes follow the object (local) or the world (global).
    pub local_orientation: bool,
    /// Snap translations to the grid (also active while Ctrl is held).
    pub snap: bool,
    /// Grid cell size in meters (translation snap increment).
    pub grid_size: f32,
}

impl GizmoState {
    pub fn new() -> Self {
        Self {
            gizmo: Gizmo::default(),
            tool: GizmoTool::Move,
            pivot: PivotMode::Origin,
            local_orientation: false,
            snap: false,
            grid_size: 0.5,
        }
    }

    pub fn is_focused(&self) -> bool {
        self.gizmo.is_focused()
    }

    pub fn pick_preview(&self, pos: Pos2) -> bool {
        self.gizmo.pick_preview((pos.x, pos.y))
    }

    /// Update + draw the gizmo for one target. `pivot_local` is the pivot
    /// point in object space (zero = object origin). Returns true when the
    /// transform was modified this frame.
    #[allow(clippy::too_many_arguments)]
    pub fn interact(
        &mut self,
        ui: &Ui,
        viewport: egui::Rect,
        view: Mat4,
        proj: Mat4,
        trs: (&mut Vec3, &mut Quat, &mut Vec3),
        pivot_local: Vec3,
        cursor: Option<Pos2>,
        drag_started: bool,
        dragging: bool,
    ) -> bool {
        let (translation, rotation, scale) = trs;
        let ctrl = ui.input(|i| i.modifiers.ctrl);
        self.gizmo.update_config(GizmoConfig {
            view_matrix: view.as_dmat4().into(),
            projection_matrix: proj.as_dmat4().into(),
            viewport,
            modes: self.tool.modes(),
            orientation: if self.local_orientation {
                GizmoOrientation::Local
            } else {
                GizmoOrientation::Global
            },
            snapping: self.snap || ctrl,
            snap_distance: self.grid_size.max(0.001),
            snap_angle: 15f32.to_radians(),
            snap_scale: 0.1,
            pixels_per_point: ui.ctx().pixels_per_point(),
            visuals: GizmoVisuals {
                // Thicker strokes so the uniform-scale center circle and
                // plane handles read clearly.
                stroke_width: 5.0,
                ..Default::default()
            },
            ..Default::default()
        });

        // The gizmo edits a frame placed at the pivot; the object's origin
        // is carried along as an offset within that frame.
        let pivot_world = *translation + *rotation * (*scale * pivot_local);
        let target = Transform::from_scale_rotation_translation(
            scale.as_dvec3(),
            rotation.as_dquat(),
            pivot_world.as_dvec3(),
        );
        let cursor_pos = cursor.map(|p| (p.x, p.y)).unwrap_or_default();
        let result = self.gizmo.update(
            GizmoInteraction {
                cursor_pos,
                hovered: cursor.is_some(),
                drag_started,
                dragging,
            },
            &[target],
        );

        // Draw through the viewport tab's painter (clipped to the tab).
        let draw_data = self.gizmo.draw();
        if !draw_data.indices.is_empty() {
            let mesh = egui::Mesh {
                indices: draw_data.indices,
                vertices: draw_data
                    .vertices
                    .into_iter()
                    .zip(draw_data.colors)
                    .map(|(pos, [r, g, b, a])| egui::epaint::Vertex {
                        pos: pos.into(),
                        uv: Pos2::default(),
                        color: egui::Rgba::from_rgba_premultiplied(r, g, b, a).into(),
                    })
                    .collect(),
                ..Default::default()
            };
            ui.painter().add(mesh);
        }

        if let Some((_, transforms)) = result
            && let Some(t) = transforms.first()
        {
            let new_pivot = DVec3::from(t.translation).as_vec3();
            let new_rotation = DQuat::from(t.rotation).as_quat();
            let new_scale = DVec3::from(t.scale).as_vec3();

            // Re-derive the object origin from the edited pivot frame:
            // origin = pivot + R_new * (ratio * (R_old⁻¹ * offset)).
            let offset = *translation - pivot_world;
            let ratio = Vec3::select(scale.cmpne(Vec3::ZERO), new_scale / *scale, Vec3::ONE);
            let local_offset = rotation.inverse() * offset;
            *translation = new_pivot + new_rotation * (local_offset * ratio);
            *rotation = new_rotation;
            *scale = new_scale;
            return true;
        }
        false
    }
}
