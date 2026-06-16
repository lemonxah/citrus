//! Humanoid full-body IK: map an imported skeleton's bones to humanoid roles by
//! name, then pose them from VR `TrackerTargets` (head/hands/waist/feet) using
//! the two-bone (limbs) + direct (hips/head) solvers. Produces per-joint local
//! transforms ready for `Skeleton::palette` → linear-blend skinning.
//!
//! Bone matching covers the common naming schemes (Mixamo `LeftArm`/`LeftForeArm`,
//! VRM/Unity `leftUpperArm`/`leftLowerArm`). The retarget math is a first cut: it
//! aligns each limb's bones to the solved world directions; exact wrist/ankle
//! roll wants per-rig tuning against a real avatar.

use citrus_assets::Skeleton;
use citrus_core::{solve_two_bone, TrackerCalibration, TrackerTargets};
use glam::{Mat4, Quat, Vec3};

/// Humanoid bone indices into a `Skeleton::joints`, by role. `None` when the
/// rig doesn't expose that bone under a recognized name.
#[derive(Clone, Copy, Debug, Default)]
pub struct HumanoidRig {
    pub hips: Option<usize>,
    pub head: Option<usize>,
    pub l_upper_arm: Option<usize>,
    pub l_lower_arm: Option<usize>,
    pub l_hand: Option<usize>,
    pub r_upper_arm: Option<usize>,
    pub r_lower_arm: Option<usize>,
    pub r_hand: Option<usize>,
    pub l_upper_leg: Option<usize>,
    pub l_lower_leg: Option<usize>,
    pub l_foot: Option<usize>,
    pub r_upper_leg: Option<usize>,
    pub r_lower_leg: Option<usize>,
    pub r_foot: Option<usize>,
}

impl HumanoidRig {
    /// Best-effort map of joint names to humanoid roles (case-insensitive
    /// substring matching across Mixamo / VRM / Unity conventions).
    pub fn map(skel: &Skeleton) -> Self {
        let names: Vec<String> = skel.joints.iter().map(|j| j.name.to_lowercase()).collect();
        // First joint whose name contains all `needs` and none of `nots`.
        let find = |needs: &[&str], nots: &[&str]| -> Option<usize> {
            names.iter().position(|n| {
                needs.iter().all(|k| n.contains(k)) && nots.iter().all(|k| !n.contains(k))
            })
        };
        // Upper arm: "upperarm" or a plain "arm" that isn't fore/lower/hand/shoulder.
        let upper_arm = |side: &str| {
            find(&[side, "upperarm"], &[])
                .or_else(|| find(&[side, "arm"], &["fore", "lower", "hand", "shoulder"]))
        };
        let lower_arm = |side: &str| {
            find(&[side, "lowerarm"], &[])
                .or_else(|| find(&[side, "forearm"], &[]))
                .or_else(|| find(&[side, "elbow"], &[]))
        };
        let upper_leg = |side: &str| {
            find(&[side, "upperleg"], &[])
                .or_else(|| find(&[side, "thigh"], &[]))
                .or_else(|| find(&[side, "upleg"], &[]))
        };
        let lower_leg = |side: &str| {
            find(&[side, "lowerleg"], &[])
                .or_else(|| find(&[side, "calf"], &[]))
                .or_else(|| find(&[side, "shin"], &[]))
                .or_else(|| find(&[side, "knee"], &[]))
                .or_else(|| find(&[side, "leg"], &["up", "thigh"]))
        };
        Self {
            hips: find(&["hip"], &[]).or_else(|| find(&["pelvis"], &[])),
            head: find(&["head"], &["headtop", "end"]),
            l_upper_arm: upper_arm("left"),
            l_lower_arm: lower_arm("left"),
            l_hand: find(&["left", "hand"], &["thumb", "index", "middle", "ring", "pinky", "little"]),
            r_upper_arm: upper_arm("right"),
            r_lower_arm: lower_arm("right"),
            r_hand: find(&["right", "hand"], &["thumb", "index", "middle", "ring", "pinky", "little"]),
            l_upper_leg: upper_leg("left"),
            l_lower_leg: lower_leg("left"),
            l_foot: find(&["left", "foot"], &["toe"]),
            r_upper_leg: upper_leg("right"),
            r_lower_leg: lower_leg("right"),
            r_foot: find(&["right", "foot"], &["toe"]),
        }
    }

    /// Enough of a humanoid to drive full-body IK (hips + head + at least one arm).
    pub fn is_humanoid(&self) -> bool {
        self.hips.is_some() && self.head.is_some()
    }

    fn arms(&self) -> [(Option<usize>, Option<usize>, Option<usize>); 2] {
        [
            (self.l_upper_arm, self.l_lower_arm, self.l_hand),
            (self.r_upper_arm, self.r_lower_arm, self.r_hand),
        ]
    }
    fn legs(&self) -> [(Option<usize>, Option<usize>, Option<usize>); 2] {
        [
            (self.l_upper_leg, self.l_lower_leg, self.l_foot),
            (self.r_upper_leg, self.r_lower_leg, self.r_foot),
        ]
    }
}

/// Forward-resolve world matrices from per-joint local transforms (order-robust).
fn world_matrices(skel: &Skeleton, locals: &[Mat4]) -> Vec<Mat4> {
    let n = skel.joints.len();
    let mut world = vec![Mat4::IDENTITY; n];
    let mut done = vec![false; n];
    let mut remaining = n;
    while remaining > 0 {
        let mut progressed = false;
        for i in 0..n {
            if done[i] {
                continue;
            }
            let ready = match skel.joints[i].parent {
                None => true,
                Some(p) => done[p],
            };
            if ready {
                let l = locals.get(i).copied().unwrap_or(Mat4::IDENTITY);
                world[i] = match skel.joints[i].parent {
                    Some(p) => world[p] * l,
                    None => l,
                };
                done[i] = true;
                remaining -= 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    world
}

/// Pose the skeleton from VR tracker targets, returning per-joint local
/// transforms (feed into `Skeleton::palette`). Falls back to the rest pose for
/// any chain whose tracker or bones are missing.
pub fn pose_from_trackers(skel: &Skeleton, targets: &TrackerTargets) -> Vec<Mat4> {
    let rig = HumanoidRig::map(skel);
    let mut locals = skel.rest_locals();

    // Hips: drive the root directly from the waist tracker (fall back to head).
    if let (Some(hips), Some((pos, rot))) = (rig.hips, targets.hips.or(targets.head)) {
        locals[hips] = Mat4::from_rotation_translation(rot, pos);
    }
    // Head: orient the head bone toward the head tracker's rotation.
    if let (Some(head), Some((_, rot))) = (rig.head, targets.head) {
        let (s, _, t) = locals[head].to_scale_rotation_translation();
        locals[head] = Mat4::from_scale_rotation_translation(s, rot, t);
    }

    // Limbs: two-bone IK toward each end-effector tracker.
    let world = world_matrices(skel, &locals);
    let apply_limb = |a: usize, b: usize, c: usize, target: Vec3, locals: &mut [Mat4]| {
        let root = world[a].w_axis.truncate();
        let mid = world[b].w_axis.truncate();
        let end = world[c].w_axis.truncate();
        // Pole hint: bend roughly away from the root→end midline, toward -Z.
        let pole = mid + Vec3::new(0.0, 0.0, -1.0);
        let sol = solve_two_bone(root, mid, end, target, pole);
        // Compose the solved world deltas into the bones' local rotations.
        for (joint, delta) in [(a, sol.root_rotation), (b, sol.mid_rotation)] {
            let parent_w = match skel.joints[joint].parent {
                Some(p) => world[p],
                None => Mat4::IDENTITY,
            };
            let (s, r, t) = locals[joint].to_scale_rotation_translation();
            let (_, pr, _) = parent_w.to_scale_rotation_translation();
            // delta is in world space; bring it into the parent's frame.
            let local_delta = pr.inverse() * delta * pr;
            locals[joint] = Mat4::from_scale_rotation_translation(s, local_delta * r, t);
        }
    };

    let arm_targets = [targets.left_hand, targets.right_hand];
    for (i, (ua, la, ha)) in rig.arms().iter().enumerate() {
        if let (Some(a), Some(b), Some(c), Some((tp, _))) = (*ua, *la, *ha, arm_targets[i]) {
            apply_limb(a, b, c, tp, &mut locals);
        }
    }
    let leg_targets = [targets.left_foot, targets.right_foot];
    for (i, (ul, ll, fl)) in rig.legs().iter().enumerate() {
        if let (Some(a), Some(b), Some(c), Some((tp, _))) = (*ul, *ll, *fl, leg_targets[i]) {
            apply_limb(a, b, c, tp, &mut locals);
        }
    }
    locals
}

/// Tuning for terrain foot IK ([`apply_foot_ik`]).
#[derive(Clone, Copy, Debug)]
pub struct FootIkParams {
    /// Height of the ankle joint above the sole (so the sole, not the ankle,
    /// lands on the ground).
    pub foot_height: f32,
    /// Max distance a foot may be raised / lowered from its animated position
    /// (keeps the IK from snapping a foot through a wall on bad geometry).
    pub max_lift: f32,
    pub max_drop: f32,
    /// 0..1: how far to lower the hips toward the lowest foot that can't reach
    /// the ground from the animated pose (Unity-style pelvis offset). Keeps both
    /// feet planted on steps / steep slopes instead of one floating.
    pub hips_follow: f32,
    /// 0..1: how strongly the foot rotates to match the ground normal.
    pub align_to_ground: f32,
    /// Knee pole hint (which way the knee bends); rig-forward, usually -Z.
    pub forward: Vec3,
}

impl Default for FootIkParams {
    fn default() -> Self {
        Self {
            foot_height: 0.12,
            max_lift: 0.5,
            max_drop: 0.5,
            hips_follow: 1.0,
            align_to_ground: 1.0,
            forward: Vec3::NEG_Z,
        }
    }
}

/// Compose a world-space rotation `delta` into joint `j`'s local transform,
/// bringing it through the parent's frame (shared by the limb solvers).
fn apply_world_delta(skel: &Skeleton, world: &[Mat4], locals: &mut [Mat4], j: usize, delta: Quat) {
    let parent_w = match skel.joints[j].parent {
        Some(p) => world[p],
        None => Mat4::IDENTITY,
    };
    let (s, r, t) = locals[j].to_scale_rotation_translation();
    let (_, pr, _) = parent_w.to_scale_rotation_translation();
    let local_delta = pr.inverse() * delta * pr;
    locals[j] = Mat4::from_scale_rotation_translation(s, local_delta * r, t);
}

/// World-space rotation of a joint from its world matrix.
fn world_rot(world: &[Mat4], j: usize) -> Quat {
    world[j].to_scale_rotation_translation().1
}

/// Replace joint `j`'s local rotation so its world rotation becomes `new_world`,
/// given the parent's (possibly already-updated) world rotation. Keeps scale +
/// translation.
fn set_world_rot(locals: &mut [Mat4], j: usize, parent_world_rot: Quat, new_world: Quat) {
    let (s, _, t) = locals[j].to_scale_rotation_translation();
    let local = parent_world_rot.inverse() * new_world;
    locals[j] = Mat4::from_scale_rotation_translation(s, local, t);
}

/// Apply an analytic two-bone solution to a limb (root `a` → mid `b` → end `c`),
/// chain-correctly: the mid bone's local rotation is derived from the root's NEW
/// world rotation, not the pre-solve snapshot (so FK reproduces the solved pose).
/// Assumes `b`'s parent is `a` (true for a limb's upper→lower bone).
fn apply_two_bone(
    skel: &Skeleton,
    world: &[Mat4],
    locals: &mut [Mat4],
    a: usize,
    b: usize,
    sol: &citrus_core::TwoBoneSolution,
) {
    let new_wr_a = sol.root_rotation * world_rot(world, a);
    let new_wr_b = sol.mid_rotation * world_rot(world, b);
    let parent_a = match skel.joints[a].parent {
        Some(p) => world_rot(world, p),
        None => Quat::IDENTITY,
    };
    set_world_rot(locals, a, parent_a, new_wr_a);
    // b's parent is a, now at new_wr_a.
    set_world_rot(locals, b, new_wr_a, new_wr_b);
}

/// Plant the feet of an animated humanoid on terrain so it walks naturally over
/// slopes and uneven ground. `locals` is the current animated pose (in/out);
/// `ground(p)` returns `(height, normal)` of the terrain under a world point (the
/// engine supplies this — a raycast or heightfield lookup), or `None` if there's
/// no ground there. Lowers the hips toward the lowest reachable foot, two-bone
/// solves each leg onto the surface, then rolls the foot to the ground normal.
pub fn apply_foot_ik(
    skel: &Skeleton,
    locals: &mut [Mat4],
    params: &FootIkParams,
    mut ground: impl FnMut(Vec3) -> Option<(f32, Vec3)>,
) {
    let rig = HumanoidRig::map(skel);
    let legs = [
        (rig.l_upper_leg, rig.l_lower_leg, rig.l_foot),
        (rig.r_upper_leg, rig.r_lower_leg, rig.r_foot),
    ];

    // 1) Pelvis drop: how far must the lowest foot descend to reach the ground?
    let world = world_matrices(skel, locals);
    let mut min_off = 0.0f32;
    let mut any = false;
    for (_, _, foot) in legs {
        if let Some(f) = foot {
            let ankle = world[f].w_axis.truncate();
            if let Some((gh, _)) = ground(ankle) {
                min_off = min_off.min(gh + params.foot_height - ankle.y);
                any = true;
            }
        }
    }
    if any && let Some(hips) = rig.hips {
        let drop = min_off.clamp(-params.max_drop, 0.0) * params.hips_follow.clamp(0.0, 1.0);
        if drop != 0.0 {
            let (s, r, t) = locals[hips].to_scale_rotation_translation();
            locals[hips] =
                Mat4::from_scale_rotation_translation(s, r, t + Vec3::new(0.0, drop, 0.0));
        }
    }

    // 2) Two-bone solve each leg onto the surface, then align the foot.
    let world = world_matrices(skel, locals);
    for (ul, ll, fl) in legs {
        let (Some(a), Some(b), Some(c)) = (ul, ll, fl) else {
            continue;
        };
        let ankle = world[c].w_axis.truncate();
        let Some((gh, n)) = ground(ankle) else {
            continue;
        };
        let mut target = Vec3::new(ankle.x, gh + params.foot_height, ankle.z);
        target.y = target.y.clamp(ankle.y - params.max_drop, ankle.y + params.max_lift);
        let root = world[a].w_axis.truncate();
        let mid = world[b].w_axis.truncate();
        let pole = mid + params.forward;
        let sol = solve_two_bone(root, mid, ankle, target, pole);
        apply_two_bone(skel, &world, locals, a, b, &sol);

        if params.align_to_ground > 0.0 {
            let w2 = world_matrices(skel, locals);
            let foot_up = w2[c].y_axis.truncate().normalize_or(Vec3::Y);
            let align = Quat::from_rotation_arc(foot_up, n.normalize_or(Vec3::Y));
            let blended = Quat::IDENTITY.slerp(align, params.align_to_ground.clamp(0.0, 1.0));
            apply_world_delta(skel, &w2, locals, c, blended);
        }
    }
}

/// Raw world poses of whichever full-body trackers are present this frame (head,
/// hands, hips, feet). Source-agnostic: SteamVR/SlimeVR, a mocap stream, etc.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrackerPoses {
    pub head: Option<Mat4>,
    pub left_hand: Option<Mat4>,
    pub right_hand: Option<Mat4>,
    pub hips: Option<Mat4>,
    pub left_foot: Option<Mat4>,
    pub right_foot: Option<Mat4>,
}

/// Per-role calibration offsets captured at the T-pose alignment step.
#[derive(Clone, Copy, Debug, Default)]
pub struct BodyCalibration {
    pub head: Option<TrackerCalibration>,
    pub left_hand: Option<TrackerCalibration>,
    pub right_hand: Option<TrackerCalibration>,
    pub hips: Option<TrackerCalibration>,
    pub left_foot: Option<TrackerCalibration>,
    pub right_foot: Option<TrackerCalibration>,
}

/// Capture full-body calibration: the avatar is shown in its rest/T-pose and the
/// player stands in the matching T-pose; for each assigned tracker we store the
/// rigid offset from the raw tracker pose to its bone's world pose. Confirm this
/// once, then drive the avatar with [`targets_from_calibrated`] every frame.
///
/// Assumes the skeleton's rest pose is (close to) a T-pose, which is the norm for
/// imported humanoid rigs; if not, pose the avatar into a T-pose first and pass
/// those locals via a skeleton whose rest matches.
pub fn calibrate_tpose(skel: &Skeleton, raw: &TrackerPoses) -> BodyCalibration {
    let locals = skel.rest_locals();
    let world = world_matrices(skel, &locals);
    let rig = HumanoidRig::map(skel);
    let cap = |bone: Option<usize>, t: Option<Mat4>| match (bone, t) {
        (Some(b), Some(tp)) => Some(TrackerCalibration::capture(tp, world[b])),
        _ => None,
    };
    BodyCalibration {
        head: cap(rig.head, raw.head),
        left_hand: cap(rig.l_hand, raw.left_hand),
        right_hand: cap(rig.r_hand, raw.right_hand),
        hips: cap(rig.hips, raw.hips),
        left_foot: cap(rig.l_foot, raw.left_foot),
        right_foot: cap(rig.r_foot, raw.right_foot),
    }
}

/// Turn live tracker poses into IK end-effector targets using a captured
/// [`BodyCalibration`]; feed the result straight into [`pose_from_trackers`].
pub fn targets_from_calibrated(cal: &BodyCalibration, raw: &TrackerPoses) -> TrackerTargets {
    let conv = |c: Option<TrackerCalibration>, t: Option<Mat4>| -> Option<(Vec3, Quat)> {
        match (c, t) {
            (Some(c), Some(tp)) => {
                let m = c.apply(tp);
                let (_, r, p) = m.to_scale_rotation_translation();
                Some((p, r))
            }
            _ => None,
        }
    };
    TrackerTargets {
        head: conv(cal.head, raw.head),
        left_hand: conv(cal.left_hand, raw.left_hand),
        right_hand: conv(cal.right_hand, raw.right_hand),
        hips: conv(cal.hips, raw.hips),
        left_foot: conv(cal.left_foot, raw.left_foot),
        right_foot: conv(cal.right_foot, raw.right_foot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use citrus_assets::Joint;
    use glam::Quat;

    fn joint(name: &str, parent: Option<usize>) -> Joint {
        joint_at(name, parent, Vec3::ZERO)
    }

    fn joint_at(name: &str, parent: Option<usize>, rest_translation: Vec3) -> Joint {
        Joint {
            name: name.into(),
            parent,
            inverse_bind: Mat4::IDENTITY,
            rest_translation,
            rest_rotation: Quat::IDENTITY,
            rest_scale: Vec3::ONE,
        }
    }

    #[test]
    fn tpose_calibration_drives_bones_from_trackers() {
        let skel = Skeleton {
            joints: vec![
                joint_at("Hips", None, Vec3::new(0.0, 1.0, 0.0)),
                joint_at("Head", Some(0), Vec3::new(0.0, 0.5, 0.0)),
                joint_at("LeftHand", Some(0), Vec3::new(0.5, 0.0, 0.0)),
            ],
        };
        // Trackers sit slightly off the bones at the calibration (T-)pose.
        let raw = TrackerPoses {
            hips: Some(Mat4::from_translation(Vec3::new(0.02, 1.0, 0.0))),
            left_hand: Some(Mat4::from_translation(Vec3::new(0.5, 0.03, 0.0))),
            ..Default::default()
        };
        let cal = calibrate_tpose(&skel, &raw);
        // At the calibration pose the targets recover the bone world positions.
        let t = targets_from_calibrated(&cal, &raw);
        assert!((t.hips.unwrap().0 - Vec3::new(0.0, 1.0, 0.0)).length() < 1e-3);
        assert!((t.left_hand.unwrap().0 - Vec3::new(0.5, 1.0, 0.0)).length() < 1e-3);
        // Moving the hand tracker moves its target by the same world delta.
        let moved = TrackerPoses {
            left_hand: Some(
                Mat4::from_translation(Vec3::new(0.0, 0.0, 0.2)) * raw.left_hand.unwrap(),
            ),
            ..raw
        };
        let t2 = targets_from_calibrated(&cal, &moved);
        assert!((t2.left_hand.unwrap().0 - Vec3::new(0.5, 1.0, 0.2)).length() < 1e-3);
    }

    #[test]
    fn foot_ik_plants_foot_on_lower_ground() {
        // Straight leg: thigh (0,0.6,0) -> shin (0,0.2,0) -> ankle (0,0,0).
        let skel = Skeleton {
            joints: vec![
                joint_at("Hips", None, Vec3::new(0.0, 1.0, 0.0)),
                joint_at("LeftUpperLeg", Some(0), Vec3::new(0.0, -0.4, 0.0)),
                joint_at("LeftLowerLeg", Some(1), Vec3::new(0.0, -0.4, 0.0)),
                joint_at("LeftFoot", Some(2), Vec3::new(0.0, -0.2, 0.0)),
            ],
        };
        let mut locals = skel.rest_locals();
        let params = FootIkParams {
            foot_height: 0.0,
            align_to_ground: 0.0,
            hips_follow: 0.0,
            ..Default::default()
        };
        // The animated foot sits at y=0 (leg straight). Ground is a 0.2 step UP,
        // which the leg can reach by bending the knee; the foot should rise to it.
        apply_foot_ik(&skel, &mut locals, &params, |_| Some((0.2, Vec3::Y)));
        let world = world_matrices(&skel, &locals);
        let foot_y = world[3].w_axis.y;
        assert!(foot_y > 0.1, "foot not lifted: {foot_y}");
        assert!((foot_y - 0.2).abs() < 0.06, "foot off ground: {foot_y}");
    }

    #[test]
    fn maps_mixamo_and_vrm_names() {
        let skel = Skeleton {
            joints: vec![
                joint("Hips", None),
                joint("Head", Some(0)),
                joint("LeftArm", Some(0)),
                joint("LeftForeArm", Some(2)),
                joint("LeftHand", Some(3)),
                joint("leftUpperLeg", Some(0)),
                joint("leftLowerLeg", Some(5)),
                joint("leftFoot", Some(6)),
            ],
        };
        let rig = HumanoidRig::map(&skel);
        assert!(rig.is_humanoid());
        assert_eq!(rig.l_upper_arm, Some(2));
        assert_eq!(rig.l_lower_arm, Some(3));
        assert_eq!(rig.l_hand, Some(4));
        assert_eq!(rig.l_upper_leg, Some(5));
        assert_eq!(rig.l_lower_leg, Some(6));
        assert_eq!(rig.l_foot, Some(7));
    }
}
