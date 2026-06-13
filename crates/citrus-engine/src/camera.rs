//! Free-fly editor camera.
//!
//! Controls: hold right mouse to look, WASD to fly (Q/E down/up, Shift =
//! fast) while looking, middle mouse drag to pan, scroll to dolly.

use glam::{Mat4, Vec3};

pub struct FlyCamera {
    pub position: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub fov_y: f32,
}

impl Default for FlyCamera {
    fn default() -> Self {
        Self::looking_at(Vec3::new(3.9, 2.2, 2.7), Vec3::new(0.0, 0.5, 0.0))
    }
}

impl FlyCamera {
    pub fn looking_at(position: Vec3, target: Vec3) -> Self {
        let dir = (target - position).normalize_or(Vec3::NEG_Z);
        Self {
            position,
            yaw: dir.z.atan2(dir.x),
            pitch: dir.y.asin(),
            fov_y: 60f32.to_radians(),
        }
    }

    pub fn forward(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(cy * cp, sp, sy * cp)
    }

    pub fn right(&self) -> Vec3 {
        self.forward().cross(Vec3::Y).normalize_or(Vec3::X)
    }

    pub fn look(&mut self, dx: f32, dy: f32) {
        self.yaw += dx * 0.0035;
        self.pitch = (self.pitch - dy * 0.0035).clamp(-1.55, 1.55);
    }

    /// Classic turntable orbit around `pivot` (left drag): the camera
    /// revolves around the pivot, looking at it. The pivot is locked for the
    /// whole drag (see engine), so the rotation is stable.
    pub fn orbit(&mut self, pivot: Vec3, dx: f32, dy: f32) {
        let offset = self.position - pivot;
        let distance = offset.length().max(0.05);
        let azimuth = offset.z.atan2(offset.x);
        let elevation = (offset.y / distance).clamp(-1.0, 1.0).asin();

        let new_azimuth = azimuth + dx * 0.008;
        let new_elevation = (elevation + dy * 0.008).clamp(-1.54, 1.54);

        let (sy, cy) = new_azimuth.sin_cos();
        let (sp, cp) = new_elevation.sin_cos();
        self.position = pivot + Vec3::new(cy * cp, sp, sy * cp) * distance;

        let dir = (pivot - self.position).normalize_or(Vec3::NEG_Z);
        self.yaw = dir.z.atan2(dir.x);
        self.pitch = dir.y.asin();
    }

    /// Screen-space pan (middle mouse drag), in pixels.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        let up = self.right().cross(self.forward()).normalize_or(Vec3::Y);
        self.position += self.right() * (-dx * 0.005) + up * (dy * 0.005);
    }

    /// Scroll dolly along the view direction.
    pub fn dolly(&mut self, scroll: f32) {
        self.position += self.forward() * scroll * 0.6;
    }

    /// Frame an object: keep the view direction, move so the object fills a
    /// comfortable portion of the screen (F-focus).
    pub fn focus(&mut self, center: Vec3, radius: f32) {
        self.position = center - self.forward() * (radius * 3.0).max(0.5);
    }

    /// `local` is (right, up, forward) movement intent, e.g. from WASD.
    pub fn fly(&mut self, local: Vec3, dt: f32, fast: bool) {
        let speed = if fast { 10.0 } else { 3.0 };
        let movement = self.right() * local.x + Vec3::Y * local.y + self.forward() * local.z;
        self.position += movement.normalize_or(Vec3::ZERO) * speed * dt;
    }

    pub fn view(&self) -> Mat4 {
        Mat4::look_to_rh(self.position, self.forward(), Vec3::Y)
    }

    pub fn proj(&self, aspect: f32) -> Mat4 {
        Mat4::perspective_rh(self.fov_y, aspect, 0.05, 500.0)
    }
}
