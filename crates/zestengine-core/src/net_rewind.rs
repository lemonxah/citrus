//! Lag compensation: the host's short history of networked hitboxes, so a
//! shot can be tested against where targets were when the shooter saw them.
//!
//! The host records every networked object that has a hit shape (Character
//! Controller capsule, Sphere / Capsule / Box Collider) each frame, stamped
//! with the session clock. A client firing sends `ctx.net_view_time()` (or
//! the host derives it from the shooter's latency with
//! `ctx.ray_cast_rewound`), and the host tests the ray against the poses at
//! that time with `ctx.ray_cast_at`. Rewinds are capped at [`MAX_REWIND`].
//! Only the recorded hitboxes are rewound, not world geometry: compare the
//! hit distance with a normal `ctx.raycast` for walls in the way.

use std::collections::{HashMap, VecDeque};

use glam::{Quat, Vec3};

use crate::{ComponentCtx, ObjectId};

/// How far back a shot may be rewound (seconds).
pub const MAX_REWIND: f64 = 0.5;
/// Seconds of history kept per object.
const KEEP: f64 = 1.0;

/// A hit volume in the object's local space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HitShape {
    Sphere { center: Vec3, radius: f32 },
    /// Segment `a`..`b` swept by `radius`.
    Capsule { a: Vec3, b: Vec3, radius: f32 },
    Box { center: Vec3, half: Vec3 },
}

/// An object's world pose at one moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitPose {
    pub time: f64,
    pub pos: Vec3,
    pub rot: Quat,
    pub scale: Vec3,
}

/// A rewound hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RewindHit {
    pub object: ObjectId,
    pub point: Vec3,
    pub distance: f32,
    /// The time the target was tested at.
    pub time: f64,
}

#[derive(Clone, Debug, Default)]
struct Track {
    shapes: Vec<HitShape>,
    poses: VecDeque<HitPose>,
}

/// Recent poses of every hittable networked object (host side).
#[derive(Clone, Debug, Default)]
pub struct HitHistory {
    tracks: HashMap<ObjectId, Track>,
    /// Time of the newest record.
    pub now: f64,
}

impl HitHistory {
    /// Empty the history and restart the clock at `now` (session clock).
    /// A host taking over after a migration has none of the old host's
    /// recorded hitboxes, so its lag-compensation history starts fresh at
    /// the handover moment; any poses from an earlier stint as host are
    /// dropped so rewound ray casts never serve stale hitboxes.
    pub fn restart(&mut self, now: f64) {
        self.tracks.clear();
        self.now = now;
    }

    /// Record an object's pose (call once per frame per object).
    pub fn record(&mut self, id: ObjectId, time: f64, pos: Vec3, rot: Quat, scale: Vec3, shapes: &[HitShape]) {
        self.now = self.now.max(time);
        let t = self.tracks.entry(id).or_default();
        if t.shapes != shapes {
            t.shapes = shapes.to_vec();
        }
        if t.poses.back().is_some_and(|p| p.time >= time) {
            t.poses.pop_back();
        }
        t.poses.push_back(HitPose { time, pos, rot, scale });
        while t.poses.front().is_some_and(|p| p.time < time - KEEP) {
            t.poses.pop_front();
        }
    }

    /// Forget objects not recorded for a while (despawned, or no longer hittable).
    pub fn prune(&mut self, now: f64) {
        self.tracks.retain(|_, t| t.poses.back().is_some_and(|p| now - p.time < KEEP));
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// An object's pose at `time` (interpolated; clamped to the recorded span).
    pub fn pose_at(&self, id: ObjectId, time: f64) -> Option<HitPose> {
        sample(&self.tracks.get(&id)?.poses, time)
    }

    /// First recorded hitbox the ray hits, with every object posed as it was at `time`.
    pub fn ray_cast_at(&self, time: f64, origin: Vec3, dir: Vec3, max_dist: f32, exclude: &[ObjectId]) -> Option<RewindHit> {
        let dir = dir.normalize_or_zero();
        if dir == Vec3::ZERO {
            return None;
        }
        let time = time.clamp(self.now - MAX_REWIND, self.now);
        let mut best: Option<RewindHit> = None;
        for (id, track) in &self.tracks {
            if exclude.contains(id) {
                continue;
            }
            let Some(pose) = sample(&track.poses, time) else {
                continue;
            };
            for shape in &track.shapes {
                if let Some(d) = ray_shape(origin, dir, shape, &pose)
                    && d <= max_dist
                    && best.is_none_or(|b| d < b.distance)
                {
                    best = Some(RewindHit { object: *id, point: origin + dir * d, distance: d, time });
                }
            }
        }
        best
    }
}

fn sample(poses: &VecDeque<HitPose>, time: f64) -> Option<HitPose> {
    let newest = *poses.back()?;
    if time >= newest.time {
        return Some(newest);
    }
    let i = poses.iter().position(|p| p.time > time)?;
    if i == 0 {
        return Some(poses[0]);
    }
    let (a, b) = (poses[i - 1], poses[i]);
    let t = ((time - a.time) / (b.time - a.time).max(1e-9)) as f32;
    Some(HitPose { time, pos: a.pos.lerp(b.pos, t), rot: a.rot.slerp(b.rot, t), scale: a.scale.lerp(b.scale, t) })
}

fn ray_shape(o: Vec3, d: Vec3, shape: &HitShape, pose: &HitPose) -> Option<f32> {
    let world = |p: Vec3| pose.pos + pose.rot * (p * pose.scale);
    let s = pose.scale.abs().max_element();
    match *shape {
        HitShape::Sphere { center, radius } => ray_sphere(o, d, world(center), radius * s),
        HitShape::Capsule { a, b, radius } => ray_capsule(o, d, world(a), world(b), radius * s),
        HitShape::Box { center, half } => {
            let inv = pose.rot.inverse();
            ray_box(inv * (o - world(center)), inv * d, half * pose.scale.abs())
        }
    }
}

fn ray_sphere(o: Vec3, d: Vec3, c: Vec3, r: f32) -> Option<f32> {
    let oc = o - c;
    let b = oc.dot(d);
    let cc = oc.dot(oc) - r * r;
    if cc <= 0.0 {
        return Some(0.0);
    }
    let h = b * b - cc;
    if h < 0.0 {
        return None;
    }
    let t = -b - h.sqrt();
    (t >= 0.0).then_some(t)
}

fn ray_capsule(o: Vec3, d: Vec3, pa: Vec3, pb: Vec3, r: f32) -> Option<f32> {
    let ba = pb - pa;
    let oa = o - pa;
    let baba = ba.dot(ba);
    if baba < 1e-10 {
        return ray_sphere(o, d, pa, r);
    }
    let bard = ba.dot(d);
    let baoa = ba.dot(oa);
    let a = baba - bard * bard;
    let mut best = None;
    if a.abs() > 1e-8 {
        let b = baba * d.dot(oa) - baoa * bard;
        let c = baba * oa.dot(oa) - baoa * baoa - r * r * baba;
        let h = b * b - a * c;
        if h >= 0.0 {
            let t = (-b - h.sqrt()) / a;
            let y = baoa + t * bard;
            if y > 0.0 && y < baba && t >= 0.0 {
                best = Some(t);
            }
        }
    }
    // End caps (also covers rays parallel to the axis and origins inside).
    for cap in [ray_sphere(o, d, pa, r), ray_sphere(o, d, pb, r)].into_iter().flatten() {
        if best.is_none_or(|b| cap < b) {
            best = Some(cap);
        }
    }
    best
}

fn ray_box(o: Vec3, d: Vec3, half: Vec3) -> Option<f32> {
    let mut tmin = 0.0f32;
    let mut tmax = f32::INFINITY;
    for k in 0..3 {
        if d[k].abs() < 1e-9 {
            if o[k].abs() > half[k] {
                return None;
            }
            continue;
        }
        let inv = 1.0 / d[k];
        let (t1, t2) = ((-half[k] - o[k]) * inv, (half[k] - o[k]) * inv);
        tmin = tmin.max(t1.min(t2));
        tmax = tmax.min(t1.max(t2));
        if tmin > tmax {
            return None;
        }
    }
    Some(tmin)
}

impl ComponentCtx<'_> {
    /// The session clock (the host's time, as this peer estimates it; 0 offline).
    pub fn net_time(&self) -> f64 {
        self.net.clock
    }

    /// Session-clock time of what this peer currently shows of remote
    /// objects (they render behind by the transit time plus the
    /// interpolation delay). Send it with a shot so the host can rewind.
    pub fn net_view_time(&self) -> f64 {
        self.net.clock - self.net.rtt_ms as f64 / 2000.0 - self.net.view_delay as f64
    }

    /// Host: hit-test networked hitboxes posed as they were at `time`
    /// (session clock, clamped to the last [`MAX_REWIND`] seconds),
    /// ignoring this object. None offline or on clients (no history).
    pub fn ray_cast_at(&self, time: f64, origin: Vec3, dir: Vec3, max_dist: f32) -> Option<RewindHit> {
        self.ray_cast_at_filtered(time, origin, dir, max_dist, &[self.self_id()])
    }

    pub fn ray_cast_at_filtered(&self, time: f64, origin: Vec3, dir: Vec3, max_dist: f32, exclude: &[ObjectId]) -> Option<RewindHit> {
        self.net.rewind.as_ref()?.lock().ok()?.ray_cast_at(time, origin, dir, max_dist, exclude)
    }

    /// Host: hit-test as `shooter` saw the world (rewound by its round trip
    /// and its reported interpolation delay), ignoring this object and
    /// everything the shooter owns.
    pub fn ray_cast_rewound(&self, shooter: u64, origin: Vec3, dir: Vec3, max_dist: f32) -> Option<RewindHit> {
        let peer = self.net.peer(shooter)?;
        let time = self.net.clock - peer.rtt_ms as f64 / 1000.0 - peer.view_delay as f64;
        let mut exclude: Vec<ObjectId> = self.net.owners.iter().filter(|(_, o)| **o == shooter).map(|(id, _)| *id).collect();
        exclude.push(self.self_id());
        self.ray_cast_at_filtered(time, origin, dir, max_dist, &exclude)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewound_ray_hits_where_the_target_was() {
        let mut h = HitHistory::default();
        let id = ObjectId::new();
        let capsule = [HitShape::Capsule { a: Vec3::new(0.0, 0.35, 0.0), b: Vec3::new(0.0, 1.45, 0.0), radius: 0.35 }];
        // The target runs along +X at 5 m/s, recorded at 60 Hz for a second.
        for k in 0..=60 {
            let t = k as f64 / 60.0;
            h.record(id, t, Vec3::new(5.0 * t as f32, 0.0, 0.0), Quat::IDENTITY, Vec3::ONE, &capsule);
        }
        // A shooter at z = 10 aiming at x = 3 (where the target was at t = 0.6).
        let origin = Vec3::new(3.0, 1.0, 10.0);
        let hit = h.ray_cast_at(0.6, origin, Vec3::NEG_Z, 100.0, &[]).expect("hit in the past");
        assert_eq!(hit.object, id);
        assert!((hit.distance - (10.0 - 0.35)).abs() < 1e-3, "{hit:?}");
        assert!(h.ray_cast_at(1.0, origin, Vec3::NEG_Z, 100.0, &[]).is_none(), "the target has moved on by now");
        assert!(h.ray_cast_at(0.6, origin, Vec3::NEG_Z, 100.0, &[id]).is_none(), "excluded");
        // Rewinds are clamped to MAX_REWIND: t = 0.1 is tested at 0.5.
        let at = h.ray_cast_at(0.1, Vec3::new(2.5, 1.0, 10.0), Vec3::NEG_Z, 100.0, &[]).expect("clamped time");
        assert!((at.time - 0.5).abs() < 1e-9);
        assert!(h.pose_at(id, 0.25).is_some_and(|p| (p.pos.x - 1.25).abs() < 1e-4), "interpolated pose");
        h.prune(5.0);
        assert!(h.is_empty());
    }

    #[test]
    fn shapes() {
        let pose = HitPose { time: 0.0, pos: Vec3::new(0.0, 0.0, -5.0), rot: Quat::from_rotation_y(0.7), scale: Vec3::ONE };
        let b = HitShape::Box { center: Vec3::ZERO, half: Vec3::splat(0.5) };
        assert!(ray_shape(Vec3::ZERO, Vec3::NEG_Z, &b, &pose).is_some_and(|d| d > 4.2 && d < 4.6));
        assert!(ray_shape(Vec3::new(2.0, 0.0, 0.0), Vec3::NEG_Z, &b, &pose).is_none());
        let s = HitShape::Sphere { center: Vec3::ZERO, radius: 1.0 };
        assert!(ray_shape(Vec3::ZERO, Vec3::NEG_Z, &s, &pose).is_some_and(|d| (d - 4.0).abs() < 1e-4));
        // A capsule seen end-on (ray along its axis) is hit at its cap.
        let c = HitShape::Capsule { a: Vec3::new(0.0, 0.0, -1.0), b: Vec3::new(0.0, 0.0, 1.0), radius: 0.5 };
        let p0 = HitPose { rot: Quat::IDENTITY, ..pose };
        assert!(ray_shape(Vec3::ZERO, Vec3::NEG_Z, &c, &p0).is_some_and(|d| (d - 3.5).abs() < 1e-4));
    }
}
