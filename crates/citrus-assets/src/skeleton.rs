//! Skeletal rigging + animation: imported armature (joint hierarchy + bind
//! pose) and animation clips. The mesh `Vertex` carries up to 4 skin-local
//! joint indices + weights; a `Skeleton`'s joints line up with those indices.
//!
//! Design follows the glTF model (which both Unity and Unreal interop with):
//! flat joint array, parent indices, per-joint inverse-bind matrix, and clips
//! as per-joint TRS channels with keyframe times. Sampling produces a joint
//! matrix palette (`joint_world * inverse_bind`) ready for linear-blend skinning.

use glam::{Mat4, Quat, Vec3};

/// One skeleton joint (bone). `parent` indexes back into `Skeleton::joints`
/// (`None` = root). `inverse_bind` maps a world-space vertex into this joint's
/// bind-pose local space; the rest TRS is the default pose used when no clip
/// drives the joint.
#[derive(Clone, Debug)]
pub struct Joint {
    pub name: String,
    pub parent: Option<usize>,
    pub inverse_bind: Mat4,
    pub rest_translation: Vec3,
    pub rest_rotation: Quat,
    pub rest_scale: Vec3,
}

/// An armature: joints in a fixed order matching the vertex joint indices.
/// Stored topologically (parents before children) so a single forward pass
/// computes world matrices.
#[derive(Clone, Debug, Default)]
pub struct Skeleton {
    pub joints: Vec<Joint>,
}

impl Skeleton {
    /// Default (rest-pose) joint local transforms.
    pub fn rest_locals(&self) -> Vec<Mat4> {
        self.joints
            .iter()
            .map(|j| Mat4::from_scale_rotation_translation(j.rest_scale, j.rest_rotation, j.rest_translation))
            .collect()
    }

    /// Compose per-joint local transforms into the skinning matrix palette
    /// (`joint_world * inverse_bind`). Handles arbitrary joint order (glTF skins
    /// aren't topologically sorted) via a fixpoint that resolves a joint once its
    /// parent's world matrix is known.
    pub fn palette(&self, locals: &[Mat4]) -> Vec<Mat4> {
        let n = self.joints.len();
        let mut world = vec![Mat4::IDENTITY; n];
        let mut done = vec![false; n];
        let mut remaining = n;
        while remaining > 0 {
            let mut progressed = false;
            for i in 0..n {
                if done[i] {
                    continue;
                }
                let ready = match self.joints[i].parent {
                    None => true,
                    Some(p) => done[p],
                };
                if ready {
                    let local = locals.get(i).copied().unwrap_or(Mat4::IDENTITY);
                    world[i] = match self.joints[i].parent {
                        Some(p) => world[p] * local,
                        None => local,
                    };
                    done[i] = true;
                    remaining -= 1;
                    progressed = true;
                }
            }
            if !progressed {
                break; // malformed (cycle / bad parent index); leave the rest identity
            }
        }
        world
            .iter()
            .zip(&self.joints)
            .map(|(w, j)| *w * j.inverse_bind)
            .collect()
    }
}

/// Which TRS component a channel animates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelPath {
    Translation,
    Rotation,
    Scale,
}

/// One animated property of one joint: keyframe times + values (linear interp;
/// rotations slerp). Only the field matching `path` is populated.
#[derive(Clone, Debug)]
pub struct AnimChannel {
    pub joint: usize,
    pub path: ChannelPath,
    pub times: Vec<f32>,
    pub vec_values: Vec<Vec3>,
    pub quat_values: Vec<Quat>,
}

/// A skeletal animation clip: per-joint TRS channels over a duration.
#[derive(Clone, Debug)]
pub struct AnimationClip {
    pub name: String,
    pub duration: f32,
    pub channels: Vec<AnimChannel>,
}

impl AnimationClip {
    /// Sample the clip at time `t` (seconds, looped), returning per-joint local
    /// transforms seeded from the skeleton's rest pose. Feed into
    /// [`Skeleton::palette`] for the skinning matrices.
    pub fn sample(&self, skeleton: &Skeleton, t: f32) -> Vec<Mat4> {
        let n = skeleton.joints.len();
        let mut trans: Vec<Vec3> = skeleton.joints.iter().map(|j| j.rest_translation).collect();
        let mut rot: Vec<Quat> = skeleton.joints.iter().map(|j| j.rest_rotation).collect();
        let mut scale: Vec<Vec3> = skeleton.joints.iter().map(|j| j.rest_scale).collect();
        let tt = if self.duration > 1e-6 { t.rem_euclid(self.duration) } else { 0.0 };
        for ch in &self.channels {
            if ch.joint >= n || ch.times.is_empty() {
                continue;
            }
            let (i0, i1, f) = key_indices(&ch.times, tt);
            match ch.path {
                ChannelPath::Translation => {
                    if let (Some(a), Some(b)) = (ch.vec_values.get(i0), ch.vec_values.get(i1)) {
                        trans[ch.joint] = a.lerp(*b, f);
                    }
                }
                ChannelPath::Scale => {
                    if let (Some(a), Some(b)) = (ch.vec_values.get(i0), ch.vec_values.get(i1)) {
                        scale[ch.joint] = a.lerp(*b, f);
                    }
                }
                ChannelPath::Rotation => {
                    if let (Some(a), Some(b)) = (ch.quat_values.get(i0), ch.quat_values.get(i1)) {
                        rot[ch.joint] = a.slerp(*b, f);
                    }
                }
            }
        }
        (0..n)
            .map(|i| Mat4::from_scale_rotation_translation(scale[i], rot[i], trans[i]))
            .collect()
    }
}

/// Linear-blend skin one vertex position by a joint matrix palette. `joints`
/// are skin-local indices into `palette`; `weights` are the matching weights.
/// Falls back to the input position when the vertex is unweighted (static).
pub fn skin_position(pos: Vec3, joints: [u32; 4], weights: [f32; 4], palette: &[Mat4]) -> Vec3 {
    let sum: f32 = weights.iter().sum();
    if sum <= 1e-5 {
        return pos;
    }
    let mut out = Vec3::ZERO;
    for k in 0..4 {
        let w = weights[k];
        if w <= 0.0 {
            continue;
        }
        if let Some(m) = palette.get(joints[k] as usize) {
            out += w * m.transform_point3(pos);
        }
    }
    out / sum
}

/// Linear-blend skin a direction (normal/tangent) — same as `skin_position` but
/// using the matrices' rotational part (no translation), then renormalized.
pub fn skin_direction(dir: Vec3, joints: [u32; 4], weights: [f32; 4], palette: &[Mat4]) -> Vec3 {
    let sum: f32 = weights.iter().sum();
    if sum <= 1e-5 {
        return dir;
    }
    let mut out = Vec3::ZERO;
    for k in 0..4 {
        let w = weights[k];
        if w <= 0.0 {
            continue;
        }
        if let Some(m) = palette.get(joints[k] as usize) {
            out += w * m.transform_vector3(dir);
        }
    }
    out.normalize_or(dir)
}

/// Find the surrounding keyframe indices + interpolation factor for time `t`.
fn key_indices(times: &[f32], t: f32) -> (usize, usize, f32) {
    if times.len() == 1 || t <= times[0] {
        return (0, 0, 0.0);
    }
    if t >= *times.last().unwrap() {
        let last = times.len() - 1;
        return (last, last, 0.0);
    }
    let mut i = 0;
    while i + 1 < times.len() && times[i + 1] < t {
        i += 1;
    }
    let (a, b) = (times[i], times[i + 1]);
    let f = if (b - a).abs() > 1e-6 { (t - a) / (b - a) } else { 0.0 };
    (i, i + 1, f.clamp(0.0, 1.0))
}
