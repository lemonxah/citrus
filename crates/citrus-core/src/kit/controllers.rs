//! Character and camera controllers: first person, third person (orbit /
//! over-the-shoulder boom with collision) and a smoothed follow camera.

use glam::{EulerRot, Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::{CharacterController, ComponentCtx, ObjectRef, TypedComponent};

/// Rotation whose forward (-Z) points along `dir`, keeping +Y up.
pub fn look_rotation(dir: Vec3) -> Quat {
    let f = dir.normalize_or(Vec3::NEG_Z);
    let up = if f.y.abs() > 0.999 { Vec3::Z } else { Vec3::Y };
    Quat::from_mat4(&Mat4::look_to_rh(Vec3::ZERO, f, up).inverse()).normalize()
}

/// Exponential smoothing factor for a rate (1/s) over `dt`.
fn smooth(rate: f32, dt: f32) -> f32 {
    1.0 - (-rate.max(0.0) * dt).exp()
}

/// Move `current` toward `target` (degrees) by at most `max_step`, the short way round.
fn approach_angle(current: f32, target: f32, max_step: f32) -> f32 {
    let diff = (target - current + 540.0).rem_euclid(360.0) - 180.0;
    current + diff.clamp(-max_step, max_step)
}

/// Shared walking state: horizontal acceleration, gravity, jumping, and either
/// the physics character controller or a flat floor at the start height.
#[derive(Default)]
#[doc(hidden)]
pub struct Walker {
    vel: Vec3,
    grounded: bool,
    floor_y: f32,
}

impl Walker {
    fn step(&mut self, ctx: &mut ComponentCtx, wish: Vec3, accel: f32, jump_speed: Option<f32>, gravity: f32, dt: f32) {
        let horizontal = Vec3::new(self.vel.x, 0.0, self.vel.z).lerp(wish, smooth(accel, dt));
        self.vel.x = horizontal.x;
        self.vel.z = horizontal.z;
        let physical = ctx.physics.is_some() && ctx.get_self::<CharacterController>().is_some();
        if physical {
            self.grounded = ctx.is_grounded();
            if self.grounded && self.vel.y < 0.0 {
                self.vel.y = -1.0;
            }
        }
        if let Some(v) = jump_speed
            && self.grounded
        {
            self.vel.y = v;
            self.grounded = false;
        }
        self.vel.y -= gravity * dt;
        if physical {
            ctx.move_character(self.vel * dt);
        } else {
            let mut pos = *ctx.translation + self.vel * dt;
            if pos.y <= self.floor_y {
                pos.y = self.floor_y;
                self.vel.y = 0.0;
                self.grounded = true;
            }
            *ctx.translation = pos;
        }
    }
}

/// First-person shooter movement: mouse look (body yaws, child camera
/// pitches), WASD, jump, sprint and crouch. Walks through the physics world
/// with a [`CharacterController`] on the same object.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct FirstPersonController {
    /// Child camera; the controller writes its local height and pitch.
    pub camera: ObjectRef,
    pub walk_speed: f32,
    pub sprint_speed: f32,
    pub crouch_speed: f32,
    pub acceleration: f32,
    /// Fraction of `acceleration` available in the air.
    pub air_control: f32,
    pub jump_height: f32,
    pub gravity: f32,
    pub look_sensitivity: f32,
    pub eye_height: f32,
    pub crouch_eye_height: f32,
    /// Character capsule height while crouched (feet stay put); 0 keeps the
    /// capsule unchanged. Standing back up waits for headroom.
    pub crouch_height: f32,
    /// Crouch toggles on press instead of while held.
    pub crouch_toggle: bool,
    pub min_pitch: f32,
    pub max_pitch: f32,
    pub lock_cursor: bool,
    pub move_action: String,
    pub look_action: String,
    pub jump_action: String,
    pub sprint_action: String,
    pub crouch_action: String,
    #[serde(skip)]
    pub yaw: f32,
    #[serde(skip)]
    pub pitch: f32,
    #[serde(skip)]
    pub crouched: bool,
    #[serde(skip)]
    pub sprinting: bool,
    #[serde(skip)]
    pub eye: f32,
    #[serde(skip)]
    pub walker: Walker,
    #[serde(skip)]
    pub started: bool,
    /// Standing capsule (height, center), captured on the first crouch.
    #[serde(skip)]
    pub stand_capsule: Option<(f32, [f32; 3])>,
    #[serde(skip)]
    pub capsule_crouched: bool,
}

impl Default for FirstPersonController {
    fn default() -> Self {
        Self {
            camera: ObjectRef::NONE,
            walk_speed: 4.5,
            sprint_speed: 7.5,
            crouch_speed: 2.2,
            acceleration: 12.0,
            air_control: 0.3,
            jump_height: 1.1,
            gravity: 20.0,
            look_sensitivity: 0.12,
            eye_height: 1.65,
            crouch_eye_height: 1.0,
            crouch_height: 1.2,
            crouch_toggle: false,
            min_pitch: -85.0,
            max_pitch: 85.0,
            lock_cursor: true,
            move_action: "Move".into(),
            look_action: "Look".into(),
            jump_action: "Jump".into(),
            sprint_action: "Sprint".into(),
            crouch_action: "Crouch".into(),
            yaw: 0.0,
            pitch: 0.0,
            crouched: false,
            sprinting: false,
            eye: 1.65,
            walker: Walker::default(),
            started: false,
            stand_capsule: None,
            capsule_crouched: false,
        }
    }
}

impl FirstPersonController {
    /// Whether something above a crouched capsule stops it from standing up.
    fn blocked_overhead(&self, ctx: &ComponentCtx) -> bool {
        let (Some((stand_h, _)), Some(cc)) = (self.stand_capsule, ctx.get_self::<CharacterController>()) else {
            return false;
        };
        let feet = ctx.self_transform().translation;
        let rise = stand_h - self.crouch_height.min(stand_h) + 0.05;
        let filter = crate::RayFilter { exclude: Some(ctx.self_id()), ..Default::default() };
        let r = cc.radius * 0.7;
        [Vec3::ZERO, Vec3::X * r, Vec3::NEG_X * r, Vec3::Z * r, Vec3::NEG_Z * r].iter().any(|o| {
            let from = feet + *o + Vec3::Y * (cc.height - 0.05).max(0.05);
            ctx.raycast_filtered(from, Vec3::Y, rise, &filter).is_some()
        })
    }

    /// Resize the character capsule to match the crouch state.
    fn apply_crouch_capsule(&mut self, ctx: &mut ComponentCtx) {
        if self.crouch_height <= 0.0 || self.crouched == self.capsule_crouched {
            return;
        }
        let Some(cc) = ctx.get_self::<CharacterController>() else {
            return;
        };
        let stand = *self.stand_capsule.get_or_insert((cc.height, cc.center));
        let (height, center) = if self.crouched {
            let h = self.crouch_height.min(stand.0);
            (h, [stand.1[0], stand.1[1] - (stand.0 - h) * 0.5, stand.1[2]])
        } else {
            stand
        };
        let me = ObjectRef::new(ctx.self_id());
        ctx.modify::<CharacterController>(me, move |c| {
            c.height = height;
            c.center = center;
        });
        self.capsule_crouched = self.crouched;
    }

    /// Current horizontal speed limit from the sprint / crouch state.
    pub fn target_speed(&self) -> f32 {
        if self.crouched {
            self.crouch_speed
        } else if self.sprinting {
            self.sprint_speed
        } else {
            self.walk_speed
        }
    }
}

impl TypedComponent for FirstPersonController {
    const NAME: &'static str = "First Person Controller";

    fn start(&mut self, ctx: &mut ComponentCtx) {
        self.yaw = ctx.rotation.to_euler(EulerRot::YXZ).0.to_degrees();
        self.walker = Walker { floor_y: ctx.translation.y, grounded: true, ..Default::default() };
        self.eye = self.eye_height;
        if self.lock_cursor {
            ctx.set_cursor_locked(true);
        }
        if self.camera.is_set() {
            ctx.set_active_camera(self.camera);
        }
        self.started = true;
    }

    fn update(&mut self, ctx: &mut ComponentCtx) {
        if !self.started {
            TypedComponent::start(self, ctx);
        }
        let dt = ctx.dt.min(0.1);
        if dt <= 0.0 {
            return;
        }
        let typing = ctx.ui_text_editing();
        if !typing {
            let look = ctx.input.axis2(&self.look_action);
            self.yaw -= look.x * self.look_sensitivity;
            self.pitch = (self.pitch - look.y * self.look_sensitivity).clamp(self.min_pitch, self.max_pitch);
        }
        let mv = if typing { glam::Vec2::ZERO } else { ctx.input.axis2(&self.move_action) };
        if self.crouch_toggle {
            if ctx.input.pressed(&self.crouch_action) {
                self.crouched = !self.crouched;
            }
        } else {
            self.crouched = ctx.input.down(&self.crouch_action);
        }
        if !self.crouched && self.capsule_crouched && self.blocked_overhead(ctx) {
            self.crouched = true;
        }
        self.apply_crouch_capsule(ctx);
        self.sprinting = !self.crouched && mv.y > 0.1 && ctx.input.down(&self.sprint_action);

        let yaw_rot = Quat::from_rotation_y(self.yaw.to_radians());
        let dir = (yaw_rot * Vec3::X * mv.x + yaw_rot * Vec3::NEG_Z * mv.y).clamp_length_max(1.0);
        let accel = if self.walker.grounded { self.acceleration } else { self.acceleration * self.air_control };
        let jump = (!typing && !self.crouched && ctx.input.pressed(&self.jump_action))
            .then(|| (2.0 * self.gravity * self.jump_height.max(0.0)).sqrt());
        self.walker.step(ctx, dir * self.target_speed(), accel, jump, self.gravity, dt);

        *ctx.rotation = yaw_rot;
        let eye_goal = if self.crouched { self.crouch_eye_height } else { self.eye_height };
        self.eye += (eye_goal - self.eye) * smooth(12.0, dt);
        ctx.set_local_transform(
            self.camera,
            Some(Vec3::new(0.0, self.eye, 0.0)),
            Some(Quat::from_rotation_x(self.pitch.to_radians())),
            None,
        );
    }
}

/// Camera position on a boom: `distance` behind and `shoulder` to the right of
/// `pivot` along `rot`, pulled in to `hit_distance - radius` when the boom
/// would pass through geometry.
pub fn boom_position(pivot: Vec3, rot: Quat, shoulder: f32, distance: f32, hit_distance: Option<f32>, radius: f32) -> Vec3 {
    let offset = rot * Vec3::new(shoulder, 0.0, distance);
    let len = offset.length();
    if len < 1e-4 {
        return pivot;
    }
    let dir = offset / len;
    let reach = match hit_distance {
        Some(d) => (d - radius).clamp(0.1, len),
        None => len,
    };
    pivot + dir * reach
}

/// Third-person character: camera orbits on a collision-aware boom over the
/// shoulder, the body turns toward its movement, and holding Aim pulls the
/// camera in and makes the body face where the camera looks (strafing).
/// `camera` must be a root object (the boom places it in world space).
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct ThirdPersonController {
    pub camera: ObjectRef,
    pub walk_speed: f32,
    pub sprint_speed: f32,
    pub aim_speed: f32,
    pub acceleration: f32,
    /// Body turn rate, degrees per second.
    pub turn_speed: f32,
    pub jump_height: f32,
    pub gravity: f32,
    pub look_sensitivity: f32,
    pub min_pitch: f32,
    pub max_pitch: f32,
    /// Boom pivot above the character's feet.
    pub pivot_height: f32,
    pub distance: f32,
    pub aim_distance: f32,
    pub shoulder_offset: f32,
    pub aim_shoulder_offset: f32,
    /// Keep the camera this far from walls.
    pub camera_radius: f32,
    /// How fast the boom moves between hip and aim framing.
    pub zoom_rate: f32,
    pub lock_cursor: bool,
    pub move_action: String,
    pub look_action: String,
    pub jump_action: String,
    pub sprint_action: String,
    pub aim_action: String,
    #[serde(skip)]
    pub aiming: bool,
    #[serde(skip)]
    pub cam_yaw: f32,
    #[serde(skip)]
    pub cam_pitch: f32,
    #[serde(skip)]
    pub body_yaw: f32,
    #[serde(skip)]
    pub cur_distance: f32,
    #[serde(skip)]
    pub cur_shoulder: f32,
    #[serde(skip)]
    pub walker: Walker,
    #[serde(skip)]
    pub started: bool,
}

impl Default for ThirdPersonController {
    fn default() -> Self {
        Self {
            camera: ObjectRef::NONE,
            walk_speed: 4.0,
            sprint_speed: 7.0,
            aim_speed: 2.5,
            acceleration: 10.0,
            turn_speed: 720.0,
            jump_height: 1.1,
            gravity: 20.0,
            look_sensitivity: 0.12,
            min_pitch: -60.0,
            max_pitch: 70.0,
            pivot_height: 1.55,
            distance: 3.6,
            aim_distance: 1.5,
            shoulder_offset: 0.55,
            aim_shoulder_offset: 0.7,
            camera_radius: 0.25,
            zoom_rate: 10.0,
            lock_cursor: true,
            move_action: "Move".into(),
            look_action: "Look".into(),
            jump_action: "Jump".into(),
            sprint_action: "Sprint".into(),
            aim_action: "Aim".into(),
            aiming: false,
            cam_yaw: 0.0,
            cam_pitch: -10.0,
            body_yaw: 0.0,
            cur_distance: 3.6,
            cur_shoulder: 0.55,
            walker: Walker::default(),
            started: false,
        }
    }
}

impl TypedComponent for ThirdPersonController {
    const NAME: &'static str = "Third Person Controller";

    fn start(&mut self, ctx: &mut ComponentCtx) {
        self.body_yaw = ctx.rotation.to_euler(EulerRot::YXZ).0.to_degrees();
        self.cam_yaw = self.body_yaw;
        self.cur_distance = self.distance;
        self.cur_shoulder = self.shoulder_offset;
        self.walker = Walker { floor_y: ctx.translation.y, grounded: true, ..Default::default() };
        if self.lock_cursor {
            ctx.set_cursor_locked(true);
        }
        if self.camera.is_set() {
            ctx.set_active_camera(self.camera);
        }
        self.started = true;
    }

    fn update(&mut self, ctx: &mut ComponentCtx) {
        if !self.started {
            TypedComponent::start(self, ctx);
        }
        let dt = ctx.dt.min(0.1);
        if dt <= 0.0 {
            return;
        }
        let typing = ctx.ui_text_editing();
        if !typing {
            let look = ctx.input.axis2(&self.look_action);
            self.cam_yaw -= look.x * self.look_sensitivity;
            self.cam_pitch = (self.cam_pitch - look.y * self.look_sensitivity).clamp(self.min_pitch, self.max_pitch);
        }
        self.aiming = !typing && ctx.input.down(&self.aim_action);
        let mv = if typing { glam::Vec2::ZERO } else { ctx.input.axis2(&self.move_action) };
        let cam_rot = Quat::from_rotation_y(self.cam_yaw.to_radians());
        let dir = (cam_rot * Vec3::X * mv.x + cam_rot * Vec3::NEG_Z * mv.y).clamp_length_max(1.0);
        let sprint = !self.aiming && mv.length() > 0.1 && ctx.input.down(&self.sprint_action);
        let speed = if self.aiming {
            self.aim_speed
        } else if sprint {
            self.sprint_speed
        } else {
            self.walk_speed
        };

        let step = self.turn_speed * dt;
        if self.aiming {
            self.body_yaw = approach_angle(self.body_yaw, self.cam_yaw, step * 2.0);
        } else if dir.length_squared() > 1e-3 {
            let goal = (-dir.x).atan2(-dir.z).to_degrees();
            self.body_yaw = approach_angle(self.body_yaw, goal, step);
        }
        let jump = (!typing && ctx.input.pressed(&self.jump_action))
            .then(|| (2.0 * self.gravity * self.jump_height.max(0.0)).sqrt());
        let accel = if self.walker.grounded { self.acceleration } else { self.acceleration * 0.3 };
        self.walker.step(ctx, dir * speed, accel, jump, self.gravity, dt);
        *ctx.rotation = Quat::from_rotation_y(self.body_yaw.to_radians());
    }

    fn late_update(&mut self, ctx: &mut ComponentCtx) {
        if !self.started || !self.camera.is_set() {
            return;
        }
        let dt = ctx.unscaled_dt.min(0.1);
        let (dist_goal, shoulder_goal) = if self.aiming {
            (self.aim_distance, self.aim_shoulder_offset)
        } else {
            (self.distance, self.shoulder_offset)
        };
        let k = smooth(self.zoom_rate, dt);
        self.cur_distance += (dist_goal - self.cur_distance) * k;
        self.cur_shoulder += (shoulder_goal - self.cur_shoulder) * k;

        let pivot = ctx.self_transform().translation + Vec3::Y * self.pivot_height;
        let rot = Quat::from_euler(EulerRot::YXZ, self.cam_yaw.to_radians(), self.cam_pitch.to_radians(), 0.0);
        let offset = rot * Vec3::new(self.cur_shoulder, 0.0, self.cur_distance);
        let hit = offset
            .try_normalize()
            .and_then(|d| ctx.raycast(pivot, d, offset.length() + self.camera_radius))
            .map(|h| h.distance);
        let pos = boom_position(pivot, rot, self.cur_shoulder, self.cur_distance, hit, self.camera_radius);
        ctx.set_local_transform(self.camera, Some(pos), Some(rot), None);
    }
}

/// Smoothed follow camera for top-down / isometric views: keeps a world
/// offset from `target`, looks at it, and zooms the offset with an axis action.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct FollowCamera {
    pub target: ObjectRef,
    /// World-space offset from the target (before zoom).
    pub offset: [f32; 3],
    /// Look at this point relative to the target.
    pub look_offset: [f32; 3],
    /// Follow rate (1/s); 0 snaps.
    pub smoothing: f32,
    pub zoom_action: String,
    pub zoom_step: f32,
    pub min_zoom: f32,
    pub max_zoom: f32,
    #[serde(skip)]
    pub zoom: f32,
    #[serde(skip)]
    pub pos: Option<Vec3>,
}

impl Default for FollowCamera {
    fn default() -> Self {
        Self {
            target: ObjectRef::NONE,
            offset: [0.0, 12.0, 9.0],
            look_offset: [0.0, 0.5, 0.0],
            smoothing: 6.0,
            zoom_action: "Zoom".into(),
            zoom_step: 0.1,
            min_zoom: 0.4,
            max_zoom: 2.0,
            zoom: 1.0,
            pos: None,
        }
    }
}

impl TypedComponent for FollowCamera {
    const NAME: &'static str = "Follow Camera";

    fn start(&mut self, ctx: &mut ComponentCtx) {
        self.zoom = 1.0;
        self.pos = None;
        ctx.set_active_camera(ctx.self_id().into());
    }

    fn late_update(&mut self, ctx: &mut ComponentCtx) {
        let Some(target) = ctx.position_of(self.target) else {
            return;
        };
        if !self.zoom_action.is_empty() && !ctx.pointer_over_ui() {
            let wheel = ctx.input.axis(&self.zoom_action);
            self.zoom = (self.zoom * (1.0 - wheel * self.zoom_step)).clamp(self.min_zoom, self.max_zoom);
        }
        let desired = target + Vec3::from(self.offset) * self.zoom;
        let pos = match self.pos {
            Some(p) if self.smoothing > 0.0 => p.lerp(desired, smooth(self.smoothing, ctx.unscaled_dt.min(0.1))),
            _ => desired,
        };
        self.pos = Some(pos);
        ctx.set_world_position(pos);
        *ctx.rotation = look_rotation(target + Vec3::from(self.look_offset) - pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn look_rotation_points_forward() {
        for dir in [Vec3::X, Vec3::NEG_Z, Vec3::new(1.0, -1.0, 0.5), Vec3::NEG_Y] {
            let q = look_rotation(dir);
            assert!((q * Vec3::NEG_Z - dir.normalize()).length() < 1e-4, "{dir:?}");
        }
    }

    #[test]
    fn boom_pulls_in_on_hit() {
        let pivot = Vec3::new(0.0, 1.5, 0.0);
        let free = boom_position(pivot, Quat::IDENTITY, 0.0, 4.0, None, 0.2);
        assert!((free - Vec3::new(0.0, 1.5, 4.0)).length() < 1e-5);
        let blocked = boom_position(pivot, Quat::IDENTITY, 0.0, 4.0, Some(2.0), 0.2);
        assert!((blocked - Vec3::new(0.0, 1.5, 1.8)).length() < 1e-5);
        let shoulder = boom_position(pivot, Quat::IDENTITY, 0.5, 2.0, None, 0.2);
        assert!(shoulder.x > 0.0 && shoulder.z > 0.0);
    }

    #[test]
    fn angles_approach_the_short_way() {
        assert_eq!(approach_angle(170.0, -170.0, 5.0), 175.0);
        assert_eq!(approach_angle(0.0, 90.0, 30.0), 30.0);
        assert_eq!(approach_angle(10.0, 12.0, 30.0), 12.0);
    }
}
