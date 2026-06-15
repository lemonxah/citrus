//! Inverse kinematics for VR full-body tracking: solve a skeleton's joint
//! rotations so end effectors (head, hands, hips, feet) reach tracker targets.
//!
//! Two solvers, matching the VRChat/Unreal IK rig split:
//! - [`solve_two_bone`]: analytic two-bone IK for limbs (upper arm + forearm →
//!   hand, thigh + shin → foot), with a pole/hint for elbow/knee direction.
//!   Exact, single-pass, no iteration — the right tool for a 3-joint limb.
//! - [`solve_fabrik`]: FABRIK for longer chains (spine: hips → chest → neck →
//!   head), iterative, handles arbitrary length.
//!
//! Both work in world space on joint positions; the caller converts the
//! resulting positions back into the skeleton's local joint rotations.

use glam::{Quat, Vec3};

/// Full-body IK end-effector targets (world-space pose per limb). The source is
/// open: VR trackers, gameplay (look-at / hand reach / foot planting), a
/// cinematic, or a networked remote avatar — any humanoid avatar can be posed
/// from these. `None` for a limb leaves it at its animated/rest pose.
///
/// (Named `TrackerTargets` for the VR path; [`IkTargets`] is the general alias.)
#[derive(Clone, Copy, Debug, Default)]
pub struct TrackerTargets {
    pub head: Option<(Vec3, Quat)>,
    pub left_hand: Option<(Vec3, Quat)>,
    pub right_hand: Option<(Vec3, Quat)>,
    pub hips: Option<(Vec3, Quat)>,
    pub left_foot: Option<(Vec3, Quat)>,
    pub right_foot: Option<(Vec3, Quat)>,
}

/// General-purpose alias: full-body IK targets for any humanoid avatar, from any
/// source (not VR-specific).
pub type IkTargets = TrackerTargets;

/// Result of a two-bone solve: world-space mid (elbow/knee) and end positions,
/// plus the rotations to apply to the root and mid joints.
#[derive(Clone, Copy, Debug)]
pub struct TwoBoneSolution {
    pub root_rotation: Quat,
    pub mid_rotation: Quat,
    pub mid_pos: Vec3,
    pub end_pos: Vec3,
}

/// Analytic two-bone IK. `root`/`mid`/`end` are the current world joint
/// positions (e.g. shoulder/elbow/wrist); `target` is where the end should
/// reach; `pole` biases the bend plane (points roughly where the elbow/knee
/// should aim). Lengths are taken from the current pose so bone lengths are
/// preserved. Returns the rotations to apply to the root and mid joints.
pub fn solve_two_bone(root: Vec3, mid: Vec3, end: Vec3, target: Vec3, pole: Vec3) -> TwoBoneSolution {
    let l_upper = (mid - root).length().max(1e-5);
    let l_lower = (end - mid).length().max(1e-5);
    let to_target = target - root;
    let mut dist = to_target.length().max(1e-5);
    // Clamp the reach so the law-of-cosines stays valid (can't over-extend).
    dist = dist.clamp((l_upper - l_lower).abs() + 1e-4, l_upper + l_lower - 1e-4);
    let dir = to_target.normalize_or(Vec3::Y);

    // Interior angle at the root (between the upper bone and the root→target line).
    let cos_root = ((l_upper * l_upper + dist * dist - l_lower * l_lower)
        / (2.0 * l_upper * dist))
        .clamp(-1.0, 1.0);
    let root_angle = cos_root.acos();

    // Bend axis from the pole: the plane containing root→target and the pole hint.
    let pole_dir = (pole - root).normalize_or(Vec3::Z);
    let mut axis = dir.cross(pole_dir);
    if axis.length_squared() < 1e-8 {
        // Pole colinear with the limb; pick any perpendicular axis.
        axis = dir.cross(Vec3::Y);
        if axis.length_squared() < 1e-8 {
            axis = dir.cross(Vec3::X);
        }
    }
    axis = axis.normalize_or(Vec3::Z);

    // Upper bone direction: rotate the root→target dir by root_angle about axis.
    let upper_dir = Quat::from_axis_angle(axis, root_angle) * dir;
    let mid_pos = root + upper_dir * l_upper;
    let lower_dir = (target - mid_pos).normalize_or(upper_dir);
    let end_pos = mid_pos + lower_dir * l_lower;

    // Rotations that take the current bone directions to the solved ones.
    let cur_upper = (mid - root).normalize_or(Vec3::Y);
    let cur_lower = (end - mid).normalize_or(Vec3::Y);
    let root_rotation = Quat::from_rotation_arc(cur_upper, upper_dir);
    let mid_rotation = Quat::from_rotation_arc(cur_lower, lower_dir);

    TwoBoneSolution {
        root_rotation,
        mid_rotation,
        mid_pos,
        end_pos,
    }
}

/// FABRIK (Forward And Backward Reaching Inverse Kinematics) for a chain of
/// joint world positions. The first joint is treated as the anchored root; the
/// last reaches `target`. Bone lengths are preserved from the input. Runs up to
/// `iterations` passes (8–10 is plenty for a spine). Mutates `joints` in place.
pub fn solve_fabrik(joints: &mut [Vec3], target: Vec3, iterations: usize) {
    let n = joints.len();
    if n < 2 {
        return;
    }
    let lengths: Vec<f32> = (0..n - 1)
        .map(|i| (joints[i + 1] - joints[i]).length().max(1e-5))
        .collect();
    let total: f32 = lengths.iter().sum();
    let root = joints[0];

    // Target unreachable: stretch the chain straight toward it.
    if (target - root).length() >= total {
        let dir = (target - root).normalize_or(Vec3::Y);
        for i in 1..n {
            joints[i] = joints[i - 1] + dir * lengths[i - 1];
        }
        return;
    }

    let tol = 1e-3;
    for _ in 0..iterations {
        if (joints[n - 1] - target).length() < tol {
            break;
        }
        // Backward pass: end → root, pulling the tip to the target.
        joints[n - 1] = target;
        for i in (0..n - 1).rev() {
            let dir = (joints[i] - joints[i + 1]).normalize_or(Vec3::Y);
            joints[i] = joints[i + 1] + dir * lengths[i];
        }
        // Forward pass: root → end, re-anchoring the root.
        joints[0] = root;
        for i in 0..n - 1 {
            let dir = (joints[i + 1] - joints[i]).normalize_or(Vec3::Y);
            joints[i + 1] = joints[i] + dir * lengths[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_bone_reaches_target_in_range() {
        let root = Vec3::ZERO;
        let mid = Vec3::new(0.0, -1.0, 0.0);
        let end = Vec3::new(0.0, -2.0, 0.0);
        let target = Vec3::new(1.0, -1.0, 0.0);
        let s = solve_two_bone(root, mid, end, target, Vec3::new(0.0, -1.0, 1.0));
        // End effector should land very close to the (reachable) target.
        assert!((s.end_pos - target).length() < 1e-2, "end {:?}", s.end_pos);
    }

    #[test]
    fn fabrik_reaches_and_preserves_length() {
        let mut joints = vec![
            Vec3::ZERO,
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(0.0, 2.0, 0.0),
            Vec3::new(0.0, 3.0, 0.0),
        ];
        let target = Vec3::new(1.5, 1.5, 0.0);
        solve_fabrik(&mut joints, target, 16);
        assert!((joints[3] - target).length() < 1e-2);
        // Bone lengths preserved (~1.0 each).
        for i in 0..3 {
            assert!(((joints[i + 1] - joints[i]).length() - 1.0).abs() < 1e-2);
        }
    }
}
