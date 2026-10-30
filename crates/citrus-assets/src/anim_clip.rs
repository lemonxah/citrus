//! Authored animation clip asset (`.anim`): keyframed property tracks made in
//! the editor's Animation tab. A track targets a bone (skinned models) or an
//! object path relative to the Animator's object ("" = the object itself,
//! "Arm/Hand" = a descendant) and animates one property with per-key
//! interpolation (step, linear, or cubic with automatic tangents).

use std::path::Path;

use anyhow::{Context as _, Result};
use glam::Quat;
use serde::{Deserialize, Serialize};

pub const ANIM_CLIP_EXTENSION: &str = "anim";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TrackProperty {
    #[default]
    Translation,
    /// Quaternion `[x, y, z, w]`.
    Rotation,
    Scale,
    /// `Light` component intensity (first value lane).
    LightIntensity,
}

impl TrackProperty {
    pub const ALL: [TrackProperty; 4] = [Self::Translation, Self::Rotation, Self::Scale, Self::LightIntensity];
    pub fn label(self) -> &'static str {
        match self {
            Self::Translation => "Position",
            Self::Rotation => "Rotation",
            Self::Scale => "Scale",
            Self::LightIntensity => "Light Intensity",
        }
    }
    /// Value lanes used in `Keyframe::value`.
    pub fn lanes(self) -> usize {
        match self {
            Self::Translation | Self::Scale => 3,
            Self::Rotation => 4,
            Self::LightIntensity => 1,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KeyInterp {
    /// Hold this key's value until the next key.
    Step,
    Linear,
    /// Smooth Hermite curve with automatic (Catmull-Rom) tangents, flat at the ends.
    #[default]
    Cubic,
}

impl KeyInterp {
    pub const ALL: [KeyInterp; 3] = [Self::Step, Self::Linear, Self::Cubic];
    pub fn label(self) -> &'static str {
        match self {
            Self::Step => "Constant",
            Self::Linear => "Linear",
            Self::Cubic => "Auto (cubic)",
        }
    }
}

/// `interp` shapes the segment from this key to the next.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Keyframe {
    pub time: f32,
    pub value: [f32; 4],
    pub interp: KeyInterp,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct AnimTrack {
    pub target: String,
    pub property: TrackProperty,
    /// Sorted by time.
    pub keys: Vec<Keyframe>,
}

impl AnimTrack {
    pub fn sort(&mut self) {
        self.keys.sort_by(|a, b| a.time.total_cmp(&b.time));
    }

    /// Insert or replace the key at `time` (within half a millisecond).
    pub fn set_key(&mut self, time: f32, value: [f32; 4]) -> usize {
        if let Some(i) = self.keys.iter().position(|k| (k.time - time).abs() < 5e-4) {
            self.keys[i].value = value;
            return i;
        }
        let interp = self.keys.last().map(|k| k.interp).unwrap_or_default();
        self.keys.push(Keyframe { time, value, interp });
        self.sort();
        self.keys.iter().position(|k| (k.time - time).abs() < 5e-4).unwrap_or(0)
    }

    /// Value at `t` seconds (no wrapping; clamps to the first / last key).
    pub fn sample(&self, t: f32) -> [f32; 4] {
        let keys = &self.keys;
        let Some(first) = keys.first() else {
            return [0.0; 4];
        };
        if keys.len() == 1 || t <= first.time {
            return first.value;
        }
        let last = keys[keys.len() - 1];
        if t >= last.time {
            return last.value;
        }
        let i = keys.partition_point(|k| k.time <= t).saturating_sub(1).min(keys.len() - 2);
        let (k0, k1) = (keys[i], keys[i + 1]);
        let h = (k1.time - k0.time).max(1e-6);
        let s = ((t - k0.time) / h).clamp(0.0, 1.0);
        let rotation = self.property == TrackProperty::Rotation;
        match k0.interp {
            KeyInterp::Step => k0.value,
            KeyInterp::Linear => {
                if rotation {
                    let (a, b) = (Quat::from_array(k0.value), Quat::from_array(k1.value));
                    a.slerp(b, s).normalize().to_array()
                } else {
                    lerp4(k0.value, k1.value, s)
                }
            }
            KeyInterp::Cubic => {
                // Keep neighbouring quaternions in one hemisphere so the
                // component-wise curve doesn't take the long way round.
                let align = |reference: [f32; 4], v: [f32; 4]| {
                    if rotation && dot4(reference, v) < 0.0 { v.map(|x| -x) } else { v }
                };
                let p0 = k0.value;
                let p1 = align(p0, k1.value);
                let tangent = |j: usize, pj: [f32; 4]| -> [f32; 4] {
                    if j == 0 || j + 1 >= keys.len() {
                        return [0.0; 4];
                    }
                    let prev = align(pj, keys[j - 1].value);
                    let next = align(pj, keys[j + 1].value);
                    let dt = (keys[j + 1].time - keys[j - 1].time).max(1e-6);
                    std::array::from_fn(|c| (next[c] - prev[c]) / dt)
                };
                let m0 = tangent(i, p0);
                let m1 = tangent(i + 1, p1);
                let (s2, s3) = (s * s, s * s * s);
                let h00 = 2.0 * s3 - 3.0 * s2 + 1.0;
                let h10 = s3 - 2.0 * s2 + s;
                let h01 = -2.0 * s3 + 3.0 * s2;
                let h11 = s3 - s2;
                let v: [f32; 4] = std::array::from_fn(|c| h00 * p0[c] + h10 * h * m0[c] + h01 * p1[c] + h11 * h * m1[c]);
                if rotation { Quat::from_array(v).normalize().to_array() } else { v }
            }
        }
    }
}

fn lerp4(a: [f32; 4], b: [f32; 4], s: f32) -> [f32; 4] {
    std::array::from_fn(|c| a[c] + (b[c] - a[c]) * s)
}

fn dot4(a: [f32; 4], b: [f32; 4]) -> f32 {
    a.iter().zip(&b).map(|(x, y)| x * y).sum()
}

/// A named marker fired when playback passes `time`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct ClipEvent {
    pub time: f32,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AnimClip {
    /// Seconds.
    pub length: f32,
    pub looping: bool,
    /// Editor snapping only.
    pub frame_rate: f32,
    pub tracks: Vec<AnimTrack>,
    pub events: Vec<ClipEvent>,
}

impl Default for AnimClip {
    fn default() -> Self {
        Self { length: 1.0, looping: true, frame_rate: 30.0, tracks: Vec::new(), events: Vec::new() }
    }
}

impl AnimClip {
    pub fn track_index(&self, target: &str, property: TrackProperty) -> Option<usize> {
        self.tracks.iter().position(|t| t.target == target && t.property == property)
    }

    /// Wrap (looping) or clamp `t` into the clip.
    pub fn local_time(&self, t: f32, looping: bool) -> f32 {
        let len = self.length.max(1e-4);
        if looping { t.rem_euclid(len) } else { t.clamp(0.0, len) }
    }

    /// Sample track `i` at playback time `t`.
    pub fn sample_track(&self, i: usize, t: f32, looping: bool) -> [f32; 4] {
        self.tracks.get(i).map(|tr| tr.sample(self.local_time(t, looping))).unwrap_or([0.0; 4])
    }

    /// Latest key time over all tracks.
    pub fn last_key_time(&self) -> f32 {
        self.tracks.iter().filter_map(|t| t.keys.last()).map(|k| k.time).fold(0.0, f32::max)
    }
}

pub fn load_anim_clip(path: impl AsRef<Path>) -> Result<AnimClip> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut clip: AnimClip = ron::from_str(&text).with_context(|| format!("parsing animation clip {}", path.display()))?;
    for t in &mut clip.tracks {
        t.sort();
    }
    Ok(clip)
}

pub fn save_anim_clip(path: impl AsRef<Path>, clip: &AnimClip) -> Result<()> {
    let path = path.as_ref();
    let text = ron::ser::to_string_pretty(clip, ron::ser::PrettyConfig::default())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(interp: KeyInterp, keys: &[(f32, f32)]) -> AnimTrack {
        AnimTrack {
            target: String::new(),
            property: TrackProperty::Translation,
            keys: keys.iter().map(|&(time, v)| Keyframe { time, value: [v, 0.0, 0.0, 0.0], interp }).collect(),
        }
    }

    #[test]
    fn step_holds_until_next_key() {
        let t = track(KeyInterp::Step, &[(0.0, 1.0), (1.0, 3.0)]);
        assert_eq!(t.sample(0.99)[0], 1.0);
        assert_eq!(t.sample(1.0)[0], 3.0);
    }

    #[test]
    fn linear_interpolates_and_clamps() {
        let t = track(KeyInterp::Linear, &[(0.0, 0.0), (2.0, 4.0)]);
        assert!((t.sample(0.5)[0] - 1.0).abs() < 1e-5);
        assert_eq!(t.sample(-1.0)[0], 0.0);
        assert_eq!(t.sample(5.0)[0], 4.0);
    }

    #[test]
    fn cubic_is_smooth_through_keys() {
        let t = track(KeyInterp::Cubic, &[(0.0, 0.0), (1.0, 1.0), (2.0, 0.0)]);
        assert!((t.sample(1.0)[0] - 1.0).abs() < 1e-5);
        // Flat end tangent, Catmull-Rom middle tangent of 0: symmetric hump.
        let a = t.sample(0.5)[0];
        let b = t.sample(1.5)[0];
        assert!((a - b).abs() < 1e-5 && a > 0.4 && a < 0.6, "{a} {b}");
        // Ease in from the flat start.
        assert!(t.sample(0.1)[0] < 0.1 * 1.0);
    }

    #[test]
    fn rotation_linear_slerps() {
        let q0 = Quat::IDENTITY;
        let q1 = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        let t = AnimTrack {
            target: String::new(),
            property: TrackProperty::Rotation,
            keys: vec![
                Keyframe { time: 0.0, value: q0.to_array(), interp: KeyInterp::Linear },
                Keyframe { time: 1.0, value: q1.to_array(), interp: KeyInterp::Linear },
            ],
        };
        let mid = Quat::from_array(t.sample(0.5));
        assert!(mid.angle_between(Quat::from_rotation_y(std::f32::consts::FRAC_PI_4)) < 1e-4);
    }

    #[test]
    fn looping_wraps_and_one_shot_clamps() {
        let clip = AnimClip {
            length: 1.0,
            tracks: vec![track(KeyInterp::Linear, &[(0.0, 0.0), (1.0, 1.0)])],
            ..Default::default()
        };
        assert!((clip.sample_track(0, 1.25, true)[0] - 0.25).abs() < 1e-5);
        assert_eq!(clip.sample_track(0, 1.25, false)[0], 1.0);
    }

    #[test]
    fn set_key_replaces_and_sorts() {
        let mut t = track(KeyInterp::Linear, &[(1.0, 1.0)]);
        t.set_key(0.5, [2.0; 4]);
        t.set_key(1.0, [3.0; 4]);
        assert_eq!(t.keys.len(), 2);
        assert_eq!(t.keys[0].time, 0.5);
        assert_eq!(t.keys[1].value[0], 3.0);
    }
}
