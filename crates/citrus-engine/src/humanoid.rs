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
use citrus_core::{solve_two_bone, TrackerTargets};
use glam::{Mat4, Vec3};

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

#[cfg(test)]
mod tests {
    use super::*;
    use citrus_assets::Joint;
    use glam::Quat;

    fn joint(name: &str, parent: Option<usize>) -> Joint {
        Joint {
            name: name.into(),
            parent,
            inverse_bind: Mat4::IDENTITY,
            rest_translation: Vec3::ZERO,
            rest_rotation: Quat::IDENTITY,
            rest_scale: Vec3::ONE,
        }
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
