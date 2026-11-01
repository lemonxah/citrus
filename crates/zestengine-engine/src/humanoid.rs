//! Humanoid full-body IK: map an imported skeleton's bones to humanoid roles by
//! name, then pose them from `TrackerTargets` (head, hands, hips, feet, chest,
//! elbows, knees). The solve follows the usual VR full-body layout: the pelvis
//! comes from the hip tracker or is estimated under the head, the spine bends
//! and twists toward the head (and chest), shoulders turn toward reaching
//! hands, arms and legs are two-bone chains with elbow / knee bend goals, end
//! bones take the tracker rotation, and without foot trackers the feet stay
//! planted and step on their own.
//!
//! Bone matching covers the common naming schemes (Mixamo `LeftArm`/`LeftForeArm`,
//! VRM/Unity `leftUpperArm`/`leftLowerArm`); an Avatar Descriptor bone map
//! overrides it.

use zestengine_assets::{AnimationClip, Skeleton};
use zestengine_core::{TRACKER_SLOTS, TrackerCalibration, TrackerTargets, solve_two_bone};
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
    pub chest: Option<usize>,
    pub neck: Option<usize>,
    pub l_eye: Option<usize>,
    pub r_eye: Option<usize>,
    pub l_shoulder: Option<usize>,
    pub r_shoulder: Option<usize>,
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
            chest: find(&["upperchest"], &[]).or_else(|| find(&["chest"], &[])).or_else(|| find(&["spine2"], &[])),
            neck: find(&["neck"], &[]),
            l_eye: find(&["left", "eye"], &["brow", "lid", "lash"])
                .or_else(|| find(&["eye_l"], &["brow", "lid", "lash"]))
                .or_else(|| find(&["eye.l"], &["brow", "lid", "lash"])),
            r_eye: find(&["right", "eye"], &["brow", "lid", "lash"])
                .or_else(|| find(&["eye_r"], &["brow", "lid", "lash"]))
                .or_else(|| find(&["eye.r"], &["brow", "lid", "lash"])),
            l_shoulder: find(&["left", "shoulder"], &[]).or_else(|| find(&["left", "clavicle"], &[])),
            r_shoulder: find(&["right", "shoulder"], &[]).or_else(|| find(&["right", "clavicle"], &[])),
        }
        .with_shoulder_parents(skel)
    }

    /// Unnamed shoulders: an upper arm's parent that isn't part of the spine.
    fn with_shoulder_parents(mut self, skel: &Skeleton) -> Self {
        let spine = spine_chain(skel, &self);
        for (shoulder, upper) in [(&mut self.l_shoulder, self.l_upper_arm), (&mut self.r_shoulder, self.r_upper_arm)] {
            if shoulder.is_none()
                && let Some(p) = upper.and_then(|u| skel.joints[u].parent)
                && !spine.contains(&p)
                && Some(p) != self.hips
            {
                *shoulder = Some(p);
            }
        }
        self
    }

    /// [`Self::map`] with explicit role -> joint-name overrides (an avatar
    /// descriptor's `bone_map`). Unknown roles / joints are returned as errors.
    pub fn map_with_overrides(skel: &Skeleton, overrides: &[(String, String)]) -> (Self, Vec<String>) {
        let mut rig = Self::map(skel);
        let mut problems = Vec::new();
        for (role, joint) in overrides {
            let Some(index) = skel.joints.iter().position(|j| j.name == *joint) else {
                problems.push(format!("bone map: no joint named {joint:?} (role {role})"));
                continue;
            };
            let slot = match role.as_str() {
                "hips" => &mut rig.hips,
                "head" => &mut rig.head,
                "left_upper_arm" => &mut rig.l_upper_arm,
                "left_lower_arm" => &mut rig.l_lower_arm,
                "left_hand" => &mut rig.l_hand,
                "right_upper_arm" => &mut rig.r_upper_arm,
                "right_lower_arm" => &mut rig.r_lower_arm,
                "right_hand" => &mut rig.r_hand,
                "left_upper_leg" => &mut rig.l_upper_leg,
                "left_lower_leg" => &mut rig.l_lower_leg,
                "left_foot" => &mut rig.l_foot,
                "right_upper_leg" => &mut rig.r_upper_leg,
                "right_lower_leg" => &mut rig.r_lower_leg,
                "right_foot" => &mut rig.r_foot,
                "chest" => &mut rig.chest,
                "neck" => &mut rig.neck,
                "left_eye" => &mut rig.l_eye,
                "right_eye" => &mut rig.r_eye,
                "left_shoulder" => &mut rig.l_shoulder,
                "right_shoulder" => &mut rig.r_shoulder,
                _ => {
                    problems.push(format!("bone map: unknown humanoid role {role:?}"));
                    continue;
                }
            };
            *slot = Some(index);
        }
        (rig, problems)
    }

    /// Enough of a humanoid to drive full-body IK (hips + head + at least one arm).
    pub fn is_humanoid(&self) -> bool {
        self.hips.is_some() && self.head.is_some()
    }
}

/// The 20 humanoid roles as aligned (source, target) joint indices, `None`
/// where a skeleton lacks that bone under a recognized name.
fn role_pairs(src: &HumanoidRig, tgt: &HumanoidRig) -> [(Option<usize>, Option<usize>); 20] {
    [
        (src.hips, tgt.hips),
        (src.head, tgt.head),
        (src.l_upper_arm, tgt.l_upper_arm),
        (src.l_lower_arm, tgt.l_lower_arm),
        (src.l_hand, tgt.l_hand),
        (src.r_upper_arm, tgt.r_upper_arm),
        (src.r_lower_arm, tgt.r_lower_arm),
        (src.r_hand, tgt.r_hand),
        (src.l_upper_leg, tgt.l_upper_leg),
        (src.l_lower_leg, tgt.l_lower_leg),
        (src.l_foot, tgt.l_foot),
        (src.r_upper_leg, tgt.r_upper_leg),
        (src.r_lower_leg, tgt.r_lower_leg),
        (src.r_foot, tgt.r_foot),
        (src.chest, tgt.chest),
        (src.neck, tgt.neck),
        (src.l_eye, tgt.l_eye),
        (src.r_eye, tgt.r_eye),
        (src.l_shoulder, tgt.l_shoulder),
        (src.r_shoulder, tgt.r_shoulder),
    ]
}

/// A precomputed role-to-role mapping that retargets animated local poses
/// between two humanoid skeletons of different bone names and proportions.
///
/// Build one [`HumanoidRetarget`] per (source, target) skeleton pair — from the
/// rigs produced by [`HumanoidRig::map`] / [`HumanoidRig::map_with_overrides`] —
/// and reuse it every frame. Each mapped bone carries the source's global
/// rotation offset from its rest pose into the target's rest pose, so a clip
/// authored for the source plays on the target with the target's own rest
/// proportions. The hips (root) translation is scaled by the target/source
/// hip-to-head rest length; every other bone keeps its target rest translation
/// and scale, and bones without a role stay at the target rest pose.
#[derive(Clone, Debug)]
pub struct HumanoidRetarget {
    src_skel: Skeleton,
    tgt_skel: Skeleton,
    pairs: Vec<(usize, usize)>,
    src_rest: Vec<Mat4>,
    tgt_rest: Vec<Mat4>,
    src_hips: Option<usize>,
    tgt_hips: Option<usize>,
    /// Target/source hip-to-head rest length.
    scale: f32,
}

impl HumanoidRetarget {
    /// Build the mapping between `src_rig` (on `src_skel`) and `tgt_rig` (on
    /// `tgt_skel`). Roles missing on either side simply drop that pair.
    pub fn new(src_skel: &Skeleton, src_rig: &HumanoidRig, tgt_skel: &Skeleton, tgt_rig: &HumanoidRig) -> Self {
        let src_rest = src_skel.world_matrices(&src_skel.rest_locals());
        let tgt_rest = tgt_skel.world_matrices(&tgt_skel.rest_locals());
        let pairs = role_pairs(src_rig, tgt_rig).into_iter().filter_map(|(s, t)| s.zip(t)).collect();
        let height = |skel: &Skeleton, rig: &HumanoidRig| -> f32 {
            let rest = skel.world_matrices(&skel.rest_locals());
            match (rig.hips, rig.head) {
                (Some(h), Some(d)) => (rest[d].w_axis.truncate() - rest[h].w_axis.truncate()).length(),
                _ => 1.0,
            }
        };
        let scale = height(tgt_skel, tgt_rig).max(1e-4) / height(src_skel, src_rig).max(1e-4);
        Self {
            src_skel: src_skel.clone(),
            tgt_skel: tgt_skel.clone(),
            pairs,
            src_rest,
            tgt_rest,
            src_hips: src_rig.hips,
            tgt_hips: tgt_rig.hips,
            scale,
        }
    }

    /// The aligned role pairs this mapping carries: `(source joint, target
    /// joint)` indices, in humanoid-role order.
    pub fn pairs(&self) -> &[(usize, usize)] {
        &self.pairs
    }

    /// How many roles the two skeletons share (0 = nothing to retarget).
    pub fn mapped_count(&self) -> usize {
        self.pairs.len()
    }

    /// Retarget a source local pose onto the target skeleton. `src_locals` is
    /// the source skeleton's animated local transforms (e.g. from
    /// [`AnimationClip::sample`]); the returned target locals are ready for
    /// [`Skeleton::palette`].
    pub fn apply(&self, src_locals: &[Mat4]) -> Vec<Mat4> {
        let src_anim = self.src_skel.world_matrices(src_locals);
        let mut tgt_glob_rot: Vec<Quat> = self.tgt_rest.iter().map(|m| m.to_scale_rotation_translation().1).collect();
        for &(s, t) in &self.pairs {
            let src_rest_rot = self.src_rest[s].to_scale_rotation_translation().1;
            let src_anim_rot = src_anim[s].to_scale_rotation_translation().1;
            let delta = (src_anim_rot * src_rest_rot.inverse()).normalize();
            let tgt_rest_rot = self.tgt_rest[t].to_scale_rotation_translation().1;
            tgt_glob_rot[t] = (tgt_rest_rot * delta).normalize();
        }
        (0..self.tgt_skel.joints.len())
            .map(|t| {
                let rot = match self.tgt_skel.joints[t].parent {
                    Some(p) => (tgt_glob_rot[p].inverse() * tgt_glob_rot[t]).normalize(),
                    None => tgt_glob_rot[t],
                };
                let mut trans = self.tgt_skel.joints[t].rest_translation;
                // The hips (root) carries the locomotion translation, scaled to
                // the target's proportions.
                if Some(t) == self.tgt_hips
                    && let Some(h) = self.src_hips
                {
                    trans += (src_anim[h].w_axis.truncate() - self.src_rest[h].w_axis.truncate()) * self.scale;
                }
                Mat4::from_scale_rotation_translation(self.tgt_skel.joints[t].rest_scale, rot, trans)
            })
            .collect()
    }
}

/// Retarget a source humanoid's animated local pose onto a target humanoid by
/// role: each mapped bone carries the source motion (its rotation relative to
/// rest, in the source parent's rest frame), re-applied in the target parent's
/// rest frame, so a clip authored for one skeleton plays on another with
/// different names and proportions. One-shot convenience over
/// [`HumanoidRetarget`]; build the struct directly to reuse the mapping across
/// frames.
pub fn retarget_locals(
    src_skel: &Skeleton,
    src_rig: &HumanoidRig,
    src_locals: &[Mat4],
    tgt_skel: &Skeleton,
    tgt_rig: &HumanoidRig,
) -> Vec<Mat4> {
    HumanoidRetarget::new(src_skel, src_rig, tgt_skel, tgt_rig).apply(src_locals)
}

/// Sample a source clip at `time` and retarget it onto `tgt_skel` in one call.
pub fn retarget_clip(
    src_skel: &Skeleton,
    src_rig: &HumanoidRig,
    clip: &AnimationClip,
    time: f32,
    tgt_skel: &Skeleton,
    tgt_rig: &HumanoidRig,
) -> Vec<Mat4> {
    retarget_locals(src_skel, src_rig, &clip.sample(src_skel, time), tgt_skel, tgt_rig)
}

fn world_matrices(skel: &Skeleton, locals: &[Mat4]) -> Vec<Mat4> {
    skel.world_matrices(locals)
}

/// Joints strictly between the hips and the head, hips side first.
pub fn spine_chain(skel: &Skeleton, rig: &HumanoidRig) -> Vec<usize> {
    let (Some(hips), Some(head)) = (rig.hips, rig.head) else { return Vec::new() };
    let mut chain = Vec::new();
    let mut j = skel.joints[head].parent;
    while let Some(p) = j {
        if p == hips {
            chain.reverse();
            return chain;
        }
        chain.push(p);
        j = skel.joints[p].parent;
    }
    Vec::new()
}

fn pos(m: &Mat4) -> Vec3 {
    m.w_axis.truncate()
}

/// The avatar's facing in its own space (horizontal), from the rest pose: the
/// left-to-right hip (or shoulder) line crossed with up.
pub fn rest_forward(skel: &Skeleton, rig: &HumanoidRig) -> Vec3 {
    let rest = world_matrices(skel, &skel.rest_locals());
    let pairs = [(rig.l_upper_leg, rig.r_upper_leg), (rig.l_upper_arm, rig.r_upper_arm), (rig.l_hand, rig.r_hand)];
    for (l, r) in pairs {
        if let (Some(l), Some(r)) = (l, r) {
            let f = (pos(&rest[l]) - pos(&rest[r])).cross(Vec3::Y);
            let f = Vec3::new(f.x, 0.0, f.z);
            if f.length_squared() > 1e-8 {
                return f.normalize();
            }
        }
    }
    Vec3::Z
}

/// The rotation about +Y that `q` applies to the horizontal direction `fwd`.
fn yaw_of(q: Quat, fwd: Vec3) -> Quat {
    let f = q * fwd;
    let f = Vec3::new(f.x, 0.0, f.z);
    if f.length_squared() < 1e-6 {
        return Quat::IDENTITY;
    }
    let a = fwd.z.atan2(fwd.x) - f.z.atan2(f.x);
    Quat::from_rotation_y(a)
}

/// Full-body IK tuning.
#[derive(Clone, Copy, Debug)]
pub struct BodyIkSettings {
    /// 0..1 mix of the IK result over the incoming pose.
    pub weight: f32,
    /// Share of the head's yaw (relative to the hips) the spine takes.
    pub spine_twist: f32,
    /// 0..1: how far shoulders turn toward reaching hands.
    pub shoulder_rotation: f32,
    /// Keep feet planted and step procedurally when there are no foot trackers.
    pub plant_feet: bool,
    /// A planted foot steps once it is this far from where it belongs.
    pub step_distance: f32,
    /// Or once the body has turned this much away from it (radians).
    pub step_angle: f32,
    pub step_time: f32,
    pub step_height: f32,
    /// Hand targets move this much further from the shoulders (arm span
    /// matching: the avatar's arms over the player's).
    pub arm_reach: f32,
}

impl Default for BodyIkSettings {
    fn default() -> Self {
        Self {
            weight: 1.0,
            spine_twist: 0.5,
            shoulder_rotation: 0.5,
            plant_feet: true,
            step_distance: 0.25,
            step_angle: 0.8,
            step_time: 0.22,
            step_height: 0.08,
            arm_reach: 1.0,
        }
    }
}

/// A procedurally planted foot (world space).
#[derive(Clone, Copy, Debug)]
struct Footing {
    pos: Vec3,
    rot: Quat,
    from: Vec3,
    from_rot: Quat,
    /// Step progress 0..1; 1 = planted.
    t: f32,
}

/// Per-avatar state the solver carries between frames (planted feet).
#[derive(Clone, Debug, Default)]
pub struct BodyIkState {
    feet: [Option<Footing>; 2],
}

/// Rotate joint `j` by the world-space `delta` about its own position.
fn rotate_joint(skel: &Skeleton, locals: &mut [Mat4], j: usize, delta: Quat) {
    let world = world_matrices(skel, locals);
    apply_world_delta(skel, &world, locals, j, delta);
}

/// Give joint `j` the world rotation `rot`, keeping its position.
fn set_joint_rotation(skel: &Skeleton, locals: &mut [Mat4], j: usize, rot: Quat) {
    let world = world_matrices(skel, locals);
    let parent = skel.joints[j].parent.map(|p| world_rot(&world, p)).unwrap_or(Quat::IDENTITY);
    set_world_rot(locals, j, parent, rot.normalize());
}

/// Rotate `chain` (root first) so `end` moves toward `target`, spreading the
/// turn evenly over the joints.
fn bend_chain(skel: &Skeleton, locals: &mut [Mat4], chain: &[usize], end: usize, target: Vec3) {
    let Some(&first) = chain.first() else { return };
    let world = world_matrices(skel, locals);
    let base = pos(&world[first]);
    let (a, b) = (pos(&world[end]) - base, target - base);
    if a.length_squared() < 1e-8 || b.length_squared() < 1e-8 {
        return;
    }
    let step = Quat::IDENTITY.slerp(Quat::from_rotation_arc(a.normalize(), b.normalize()), 1.0 / chain.len() as f32);
    for &j in chain {
        rotate_joint(skel, locals, j, step);
    }
}

/// Two-bone solve of `a -> b -> c` toward `target`, bending toward `pole`.
fn solve_limb(skel: &Skeleton, locals: &mut [Mat4], (a, b, c): (usize, usize, usize), target: Vec3, pole: Vec3) {
    let world = world_matrices(skel, locals);
    let sol = solve_two_bone(pos(&world[a]), pos(&world[b]), pos(&world[c]), target, pole);
    apply_two_bone(skel, &world, locals, a, b, &sol);
}

/// Pose an arm (`upper` → `lower` → `hand`) so the hand joint lands on
/// `target` (skeleton space: position and rotation), the elbow bending toward
/// `pole`.
pub fn reach_hand(skel: &Skeleton, locals: &mut [Mat4], arm: (usize, usize, usize), target: Mat4, pole: Vec3) {
    let (_, rotation, position) = target.to_scale_rotation_translation();
    solve_limb(skel, locals, arm, position, pole);
    set_joint_rotation(skel, locals, arm.2, rotation);
}

/// Pose `locals` (the incoming animated or rest pose, in/out) from `targets`,
/// given in the skeleton's own space. `world` is the avatar's world matrix
/// (planted feet are held in world space) and `dt` the frame time.
#[allow(clippy::too_many_arguments)]
pub fn solve_body(
    skel: &Skeleton,
    rig: &HumanoidRig,
    locals: &mut [Mat4],
    targets: &TrackerTargets,
    settings: &BodyIkSettings,
    state: &mut BodyIkState,
    world: Mat4,
    dt: f32,
) {
    let n = skel.joints.len();
    if locals.len() < n || !rig.is_humanoid() || !targets.any() {
        return;
    }
    let base_locals: Vec<Mat4> = locals.to_vec();
    let base = world_matrices(skel, locals);
    let rest = world_matrices(skel, &skel.rest_locals());
    let fwd_rest = rest_forward(skel, rig);
    let (hips, head) = (rig.hips.unwrap_or(0), rig.head.unwrap_or(0));
    let spine = spine_chain(skel, rig);

    // Pelvis: the hip tracker, else under the head turned by the head's yaw.
    let hips_pose = targets.hips.or_else(|| {
        let (hp, hr) = targets.head?;
        let yaw = yaw_of(hr * world_rot(&base, head).inverse(), fwd_rest);
        let offset = pos(&base[hips]) - pos(&base[head]);
        Some((hp + yaw * offset, yaw * world_rot(&base, hips)))
    });
    if let Some((p, r)) = hips_pose {
        let parent = skel.joints[hips].parent.map(|pj| base[pj]).unwrap_or(Mat4::IDENTITY);
        let (scale, _, _) = base[hips].to_scale_rotation_translation();
        locals[hips] = parent.inverse() * Mat4::from_scale_rotation_translation(scale, r.normalize(), p);
    }
    let hips_yaw = yaw_of(world_rot(&world_matrices(skel, locals), hips) * world_rot(&rest, hips).inverse(), fwd_rest);

    // Spine: bend toward the chest and head, twist with the head's yaw.
    if !spine.is_empty() {
        let chest_at = rig.chest.and_then(|c| spine.iter().position(|&j| j == c));
        if let (Some((cp, cr)), Some(k)) = (targets.chest, chest_at) {
            bend_chain(skel, locals, &spine[..k], spine[k], cp);
            set_joint_rotation(skel, locals, spine[k], cr);
            if let Some((hp, _)) = targets.head {
                bend_chain(skel, locals, &spine[k + 1..], head, hp);
            }
        } else if let Some((hp, _)) = targets.head {
            bend_chain(skel, locals, &spine, head, hp);
        }
        if let Some((_, hr)) = targets.head
            && settings.spine_twist > 0.0
        {
            let rel = yaw_of(hr * world_rot(&base, head).inverse(), fwd_rest) * hips_yaw.inverse();
            let step = Quat::IDENTITY.slerp(rel, settings.spine_twist.clamp(0.0, 1.0) / spine.len() as f32);
            for &j in &spine {
                if targets.chest.is_some() && Some(j) == rig.chest {
                    continue;
                }
                rotate_joint(skel, locals, j, step);
            }
        }
    }
    if let Some((_, hr)) = targets.head {
        set_joint_rotation(skel, locals, head, hr);
    }

    // Body frame for bend goals: the chest's turn from rest, else the hips'.
    let w = world_matrices(skel, locals);
    let torso = rig.chest.or(spine.last().copied()).unwrap_or(hips);
    let body = world_rot(&w, torso) * world_rot(&rest, torso).inverse();
    let fwd = (body * fwd_rest).normalize_or(fwd_rest);
    let up = (body * Vec3::Y).normalize_or(Vec3::Y);
    let left = up.cross(fwd).normalize_or(Vec3::X);

    // Arms.
    let arms = [
        (rig.l_shoulder, rig.l_upper_arm, rig.l_lower_arm, rig.l_hand, targets.left_hand, targets.left_elbow, left),
        (rig.r_shoulder, rig.r_upper_arm, rig.r_lower_arm, rig.r_hand, targets.right_hand, targets.right_elbow, -left),
    ];
    for (shoulder, ua, la, ha, hand, elbow, out) in arms {
        let (Some(a), Some(b), Some(c), Some((tp, tr))) = (ua, la, ha, hand) else { continue };
        if let Some(sh) = shoulder
            && settings.shoulder_rotation > 0.0
        {
            let w = world_matrices(skel, locals);
            let origin = pos(&w[sh]);
            let (cur, want) = (pos(&w[a]) - origin, tp - origin);
            if cur.length_squared() > 1e-8 && want.length_squared() > 1e-8 {
                let full = Quat::from_rotation_arc(cur.normalize(), want.normalize());
                let (axis, angle) = full.to_axis_angle();
                let angle = (angle * settings.shoulder_rotation.clamp(0.0, 1.0) * 0.35).min(0.5);
                rotate_joint(skel, locals, sh, Quat::from_axis_angle(axis, angle));
            }
        }
        let w = world_matrices(skel, locals);
        let tp = match settings.arm_reach {
            r if (r - 1.0).abs() > 1e-3 => pos(&w[a]) + (tp - pos(&w[a])) * r,
            _ => tp,
        };
        let pole = match elbow {
            Some((ep, _)) => ep,
            None => pos(&w[b]) + (-fwd * 0.4 - up * 0.6 + out * 0.3),
        };
        solve_limb(skel, locals, (a, b, c), tp, pole);
        set_joint_rotation(skel, locals, c, tr);
    }

    // Legs: foot trackers, else planted feet that step.
    let legs = [
        (rig.l_upper_leg, rig.l_lower_leg, rig.l_foot, targets.left_foot, targets.left_knee, left),
        (rig.r_upper_leg, rig.r_lower_leg, rig.r_foot, targets.right_foot, targets.right_knee, -left),
    ];
    let inv_world = world.inverse();
    let (_, world_r, _) = world.to_scale_rotation_translation();
    let w = world_matrices(skel, locals);
    let hips_now = pos(&w[hips]);
    let stepping = state.feet.iter().flatten().any(|f| f.t < 1.0);
    let mut started = stepping;
    for (side, (ul, ll, fl, foot, knee, out)) in legs.into_iter().enumerate() {
        let (Some(a), Some(b), Some(c)) = (ul, ll, fl) else { continue };
        let target = match foot {
            Some(t) => {
                state.feet[side] = None;
                Some(t)
            }
            None if settings.plant_feet && (targets.head.is_some() || targets.hips.is_some()) => {
                let offset = pos(&base[c]) - pos(&base[hips]);
                let ideal = hips_now + hips_yaw * Vec3::new(offset.x, 0.0, offset.z);
                let ideal = Vec3::new(ideal.x, pos(&base[c]).y, ideal.z);
                let ideal_rot = hips_yaw * world_rot(&base, c);
                let (ideal_w, ideal_wr) = (world.transform_point3(ideal), (world_r * ideal_rot).normalize());
                let f = state.feet[side].get_or_insert(Footing {
                    pos: ideal_w,
                    rot: ideal_wr,
                    from: ideal_w,
                    from_rot: ideal_wr,
                    t: 1.0,
                });
                if f.t >= 1.0 {
                    let far = (f.pos - ideal_w).length() > settings.step_distance;
                    let turned = f.rot.angle_between(ideal_wr) > settings.step_angle;
                    if (far || turned) && !started {
                        started = true;
                        *f = Footing { pos: ideal_w, rot: ideal_wr, from: f.pos, from_rot: f.rot, t: 0.0 };
                    }
                } else {
                    f.pos = ideal_w;
                    f.rot = ideal_wr;
                }
                let (p, r) = if f.t < 1.0 {
                    f.t = (f.t + dt / settings.step_time.max(0.01)).min(1.0);
                    let e = f.t * f.t * (3.0 - 2.0 * f.t);
                    let lift = (f.t * std::f32::consts::PI).sin() * settings.step_height;
                    (f.from.lerp(f.pos, e) + Vec3::Y * lift, f.from_rot.slerp(f.rot, e))
                } else {
                    (f.pos, f.rot)
                };
                Some((inv_world.transform_point3(p), (world_r.inverse() * r).normalize()))
            }
            None => None,
        };
        let Some((tp, tr)) = target else { continue };
        let w = world_matrices(skel, locals);
        let pole = match knee {
            Some((kp, _)) => kp,
            None => pos(&w[b]) + fwd * 0.5 + out * 0.1,
        };
        solve_limb(skel, locals, (a, b, c), tp, pole);
        set_joint_rotation(skel, locals, c, tr);
    }

    let weight = settings.weight.clamp(0.0, 1.0);
    if weight < 1.0 {
        for (l, b) in locals.iter_mut().zip(&base_locals) {
            let (s0, r0, t0) = b.to_scale_rotation_translation();
            let (s1, r1, t1) = l.to_scale_rotation_translation();
            *l = Mat4::from_scale_rotation_translation(s0.lerp(s1, weight), r0.slerp(r1, weight), t0.lerp(t1, weight));
        }
    }
}

/// Turn the eye bones by a gaze (x right, y up, -1..1) of up to `max` (yaw,
/// pitch) radians, in the head's frame.
pub fn apply_eye_gaze(skel: &Skeleton, rig: &HumanoidRig, locals: &mut [Mat4], left: glam::Vec2, right: glam::Vec2, max: glam::Vec2) {
    let Some(head) = rig.head else { return };
    let rest = world_matrices(skel, &skel.rest_locals());
    let w = world_matrices(skel, locals);
    let turn = world_rot(&w, head) * world_rot(&rest, head).inverse();
    let fwd = turn * rest_forward(skel, rig);
    let up = turn * Vec3::Y;
    let side = fwd.cross(up).normalize_or(Vec3::NEG_X);
    for (eye, g) in [(rig.l_eye, left), (rig.r_eye, right)] {
        let Some(e) = eye else { continue };
        let g = g.clamp(glam::Vec2::splat(-1.0), glam::Vec2::ONE);
        let delta = Quat::from_axis_angle(side, g.y * max.y) * Quat::from_axis_angle(up, -g.x * max.x);
        rotate_joint(skel, locals, e, delta);
    }
}

/// Pose the skeleton from tracker targets in its own space, starting from the
/// rest pose, without foot planting. Returns per-joint locals.
pub fn pose_from_trackers(skel: &Skeleton, targets: &TrackerTargets) -> Vec<Mat4> {
    let rig = HumanoidRig::map(skel);
    let mut locals = skel.rest_locals();
    let settings = BodyIkSettings { plant_feet: false, ..Default::default() };
    solve_body(skel, &rig, &mut locals, targets, &settings, &mut BodyIkState::default(), Mat4::IDENTITY, 0.0);
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
    sol: &zestengine_core::TwoBoneSolution,
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

/// Raw tracker poses in slot order (see `TrackerTargets::slots`), any source.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrackerPoses(pub [Option<Mat4>; TRACKER_SLOTS]);

impl TrackerPoses {
    pub fn from_targets(t: &TrackerTargets) -> Self {
        Self(t.slots().map(|p| p.map(|(pos, rot)| Mat4::from_rotation_translation(rot, pos))))
    }
}

/// Per-slot calibration offsets from a tracker to the bone it drives.
#[derive(Clone, Copy, Debug, Default)]
pub struct BodyCalibration(pub [Option<TrackerCalibration>; TRACKER_SLOTS]);

/// The bone each tracker slot drives (elbow and knee trackers drive the lower
/// arm / leg joint, i.e. the elbow and knee).
fn slot_bones(rig: &HumanoidRig) -> [Option<usize>; TRACKER_SLOTS] {
    [
        rig.head,
        rig.l_hand,
        rig.r_hand,
        rig.hips,
        rig.l_foot,
        rig.r_foot,
        rig.chest,
        rig.l_lower_arm,
        rig.r_lower_arm,
        rig.l_lower_leg,
        rig.r_lower_leg,
    ]
}

/// Bone world poses of the rest pose with positions scaled by `scale`
/// (model units to tracking-space meters).
fn scaled_rest(skel: &Skeleton, scale: f32) -> Vec<Mat4> {
    world_matrices(skel, &skel.rest_locals())
        .iter()
        .map(|m| {
            let (_, r, t) = m.to_scale_rotation_translation();
            Mat4::from_rotation_translation(r, t * scale)
        })
        .collect()
}

/// Capture full-body calibration while the player holds the avatar's rest
/// pose (a T-pose for imported humanoids). The avatar, at `scale` (model
/// units to tracking-space meters, see `zestengine_core::BodyFit`), is lined up
/// under the player's head, turned to face where the head faces, then each
/// tracker's offset to its bone is stored. That offset carries the hip and
/// foot height differences between the player and the avatar, so standing
/// up straight puts the avatar's hips and feet at its own rest heights.
/// After this, [`targets_from_calibrated`] turns live tracker poses into bone
/// targets in the trackers' space.
pub fn calibrate_tpose(skel: &Skeleton, rig: &HumanoidRig, raw: &TrackerPoses, scale: f32) -> BodyCalibration {
    let world = scaled_rest(skel, scale);
    let align = match (raw.0[0], rig.head) {
        (Some(hmd), Some(head)) => {
            let (_, hr, hp) = hmd.to_scale_rotation_translation();
            let look = hr * Vec3::NEG_Z;
            let yaw = yaw_of(Quat::from_rotation_arc(rest_forward(skel, rig), Vec3::new(look.x, 0.0, look.z).normalize_or(Vec3::NEG_Z)), rest_forward(skel, rig));
            let head_at = yaw * pos(&world[head]);
            Mat4::from_rotation_translation(yaw, Vec3::new(hp.x - head_at.x, 0.0, hp.z - head_at.z))
        }
        _ => Mat4::IDENTITY,
    };
    let bones = slot_bones(rig);
    let mut out = BodyCalibration::default();
    for ((slot, bone), t) in out.0.iter_mut().zip(bones).zip(raw.0) {
        if let (Some(b), Some(t)) = (bone, t) {
            *slot = Some(TrackerCalibration::capture(t, align * world[b]));
        }
    }
    out
}

/// Calibration used before the player calibrates: the offsets an ideal player
/// would produce in the rest pose (headset at the eyes looking forward,
/// controllers and trackers aligned with the body), so head and hand rotations
/// already map sensibly onto the bones. `scale` as in [`calibrate_tpose`].
pub fn default_calibration(skel: &Skeleton, rig: &HumanoidRig, scale: f32) -> BodyCalibration {
    let world = scaled_rest(skel, scale);
    let fwd = rest_forward(skel, rig);
    let facing = Quat::from_rotation_y((-fwd.x).atan2(-fwd.z));
    let bones = slot_bones(rig);
    let mut raw = TrackerPoses::default();
    for (k, (slot, bone)) in raw.0.iter_mut().zip(bones).enumerate() {
        if let Some(b) = bone {
            let mut p = pos(&world[b]);
            if k == 0 {
                p += (fwd * 0.08 + Vec3::Y * 0.07) * scale;
            }
            *slot = Some(Mat4::from_rotation_translation(facing, p));
        }
    }
    let mut out = BodyCalibration::default();
    for ((slot, bone), t) in out.0.iter_mut().zip(bones).zip(raw.0) {
        if let (Some(b), Some(t)) = (bone, t) {
            *slot = Some(TrackerCalibration::capture(t, world[b]));
        }
    }
    out
}

/// An avatar's body in its model units, from the rest pose.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AvatarMeasure {
    /// Eye height above the floor (the eye bones, else above the head bone).
    pub eye_height: f32,
    /// Wrist-to-wrist distance with the arms straight out.
    pub arm_span: Option<f32>,
}

/// Measure a humanoid avatar (`None` without a head bone).
pub fn measure_avatar(skel: &Skeleton, rig: &HumanoidRig) -> Option<AvatarMeasure> {
    let rest = world_matrices(skel, &skel.rest_locals());
    let at = |j: Option<usize>| j.and_then(|j| rest.get(j)).map(pos);
    let head = at(rig.head)?;
    let feet: Vec<f32> = [at(rig.l_foot), at(rig.r_foot)].into_iter().flatten().map(|p| p.y).collect();
    let lowest = feet.iter().copied().reduce(f32::min);
    // Avatars normally stand on their origin; one rooted at the hips stands on its soles.
    let floor = match lowest {
        Some(f) => (f - 0.045 * (head.y - f).max(0.0)).min(0.0),
        None => 0.0,
    };
    let eyes: Vec<Vec3> = [at(rig.l_eye), at(rig.r_eye)].into_iter().flatten().collect();
    let eye_y = if eyes.is_empty() {
        head.y + 0.07 * ((head.y - floor) / 1.6)
    } else {
        eyes.iter().map(|e| e.y).sum::<f32>() / eyes.len() as f32
    };
    let arm = |ua: Option<usize>, la: Option<usize>, ha: Option<usize>| -> Option<(Vec3, f32)> {
        let (u, l, h) = (at(ua)?, at(la)?, at(ha)?);
        Some((u, u.distance(l) + l.distance(h)))
    };
    let arm_span = match (arm(rig.l_upper_arm, rig.l_lower_arm, rig.l_hand), arm(rig.r_upper_arm, rig.r_lower_arm, rig.r_hand)) {
        (Some((lu, ll)), Some((ru, rl))) => Some(lu.distance(ru) + ll + rl),
        _ => None,
    };
    let eye_height = eye_y - floor;
    (eye_height > 1e-4).then_some(AvatarMeasure { eye_height, arm_span })
}

/// Turn live tracker poses into bone targets using a [`BodyCalibration`].
/// Slots without a calibration pass through unchanged.
pub fn targets_from_calibrated(cal: &BodyCalibration, raw: &TrackerPoses) -> TrackerTargets {
    let mut out = [None; TRACKER_SLOTS];
    for ((slot, t), c) in out.iter_mut().zip(raw.0).zip(cal.0) {
        *slot = t.map(|t| {
            let m = c.map(|c| c.apply(t)).unwrap_or(t);
            let (_, r, p) = m.to_scale_rotation_translation();
            (p, r.normalize())
        });
    }
    TrackerTargets::from_slots(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zestengine_assets::Joint;
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
        let mut raw = TrackerPoses::default();
        raw.0[3] = Some(Mat4::from_translation(Vec3::new(0.02, 1.0, 0.0)));
        raw.0[1] = Some(Mat4::from_translation(Vec3::new(0.5, 0.03, 0.0)));
        let cal = calibrate_tpose(&skel, &HumanoidRig::map(&skel), &raw, 1.0);
        // At the calibration pose the targets recover the bone world positions.
        let t = targets_from_calibrated(&cal, &raw);
        assert!((t.hips.unwrap().0 - Vec3::new(0.0, 1.0, 0.0)).length() < 1e-3);
        assert!((t.left_hand.unwrap().0 - Vec3::new(0.5, 1.0, 0.0)).length() < 1e-3);
        // Moving the hand tracker moves its target by the same world delta.
        let mut moved = raw;
        moved.0[1] = Some(Mat4::from_translation(Vec3::new(0.0, 0.0, 0.2)) * raw.0[1].unwrap());
        let t2 = targets_from_calibrated(&cal, &moved);
        assert!((t2.left_hand.unwrap().0 - Vec3::new(0.5, 1.0, 0.2)).length() < 1e-3);
    }

    /// A T-pose humanoid facing +Z, hips at 1 m, head at 1.65 m.
    fn tpose() -> Skeleton {
        let mut joints = vec![
            joint_at("Hips", None, Vec3::new(0.0, 1.0, 0.0)),
            joint_at("Spine", Some(0), Vec3::new(0.0, 0.15, 0.0)),
            joint_at("Chest", Some(1), Vec3::new(0.0, 0.2, 0.0)),
            joint_at("Neck", Some(2), Vec3::new(0.0, 0.2, 0.0)),
            joint_at("Head", Some(3), Vec3::new(0.0, 0.1, 0.0)),
        ];
        for (side, x) in [("Left", 1.0f32), ("Right", -1.0)] {
            let base = joints.len();
            joints.push(joint_at(&format!("{side}Shoulder"), Some(2), Vec3::new(0.05 * x, 0.15, 0.0)));
            joints.push(joint_at(&format!("{side}UpperArm"), Some(base), Vec3::new(0.1 * x, 0.0, 0.0)));
            joints.push(joint_at(&format!("{side}LowerArm"), Some(base + 1), Vec3::new(0.25 * x, 0.0, 0.0)));
            joints.push(joint_at(&format!("{side}Hand"), Some(base + 2), Vec3::new(0.25 * x, 0.0, 0.0)));
            joints.push(joint_at(&format!("{side}UpperLeg"), Some(0), Vec3::new(0.1 * x, -0.05, 0.0)));
            joints.push(joint_at(&format!("{side}LowerLeg"), Some(base + 4), Vec3::new(0.0, -0.45, 0.0)));
            joints.push(joint_at(&format!("{side}Foot"), Some(base + 5), Vec3::new(0.0, -0.42, 0.0)));
        }
        Skeleton { joints }
    }

    /// A taller humanoid with Mixamo-style plain names (`Arm`/`ForeArm`/`Thigh`/
    /// `Shin` instead of `UpperArm`/`LowerArm`/`UpperLeg`/`LowerLeg`), so
    /// retargeting exercises the cross-convention bone matching.
    fn alt_tpose() -> Skeleton {
        let mut joints = vec![
            joint_at("Hips", None, Vec3::new(0.0, 1.0, 0.0)),
            joint_at("Spine", Some(0), Vec3::new(0.0, 0.18, 0.0)),
            joint_at("Chest", Some(1), Vec3::new(0.0, 0.25, 0.0)),
            joint_at("Neck", Some(2), Vec3::new(0.0, 0.2, 0.0)),
            joint_at("Head", Some(3), Vec3::new(0.0, 0.12, 0.0)),
        ];
        for (side, x) in [("Left", 1.0f32), ("Right", -1.0)] {
            let base = joints.len();
            joints.push(joint_at(&format!("{side}Shoulder"), Some(2), Vec3::new(0.06 * x, 0.18, 0.0)));
            joints.push(joint_at(&format!("{side}Arm"), Some(base), Vec3::new(0.15 * x, 0.0, 0.0)));
            joints.push(joint_at(&format!("{side}ForeArm"), Some(base + 1), Vec3::new(0.35 * x, 0.0, 0.0)));
            joints.push(joint_at(&format!("{side}Hand"), Some(base + 2), Vec3::new(0.35 * x, 0.0, 0.0)));
            joints.push(joint_at(&format!("{side}Thigh"), Some(0), Vec3::new(0.12 * x, -0.06, 0.0)));
            joints.push(joint_at(&format!("{side}Shin"), Some(base + 4), Vec3::new(0.0, -0.5, 0.0)));
            joints.push(joint_at(&format!("{side}Foot"), Some(base + 5), Vec3::new(0.0, -0.45, 0.0)));
        }
        Skeleton { joints }
    }

    fn at(skel: &Skeleton, locals: &[Mat4], name: &str) -> Vec3 {
        let i = skel.joints.iter().position(|j| j.name == name).unwrap();
        world_matrices(skel, locals)[i].w_axis.truncate()
    }

    #[test]
    fn maps_the_full_rig() {
        let skel = tpose();
        let rig = HumanoidRig::map(&skel);
        assert_eq!((rig.chest, rig.neck, rig.l_shoulder, rig.r_shoulder), (Some(2), Some(3), Some(5), Some(12)));
        assert_eq!(spine_chain(&skel, &rig), vec![1, 2, 3]);
        assert!((rest_forward(&skel, &rig) - Vec3::Z).length() < 1e-5);
    }

    #[test]
    fn retarget_rest_pose_matches_target_rest() {
        let src = tpose();
        let tgt = alt_tpose();
        let out = retarget_locals(&src, &HumanoidRig::map(&src), &src.rest_locals(), &tgt, &HumanoidRig::map(&tgt));
        let tgt_rest = tgt.rest_locals();
        for (a, b) in out.iter().zip(&tgt_rest) {
            let (ta, ra, sa) = a.to_scale_rotation_translation();
            let (tb, rb, sb) = b.to_scale_rotation_translation();
            assert!((ta - tb).length() < 1e-4, "translation mismatch");
            assert!((ra - rb).length() < 1e-4, "rotation mismatch");
            assert!((sa - sb).length() < 1e-4, "scale mismatch");
        }
    }

    #[test]
    fn retarget_transfers_arm_rotation_across_naming() {
        let src = tpose();
        let mut src_locals = src.rest_locals();
        let upper = src.joints.iter().position(|j| j.name == "LeftUpperArm").unwrap();
        src_locals[upper] = Mat4::from_rotation_translation(
            Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            src.joints[upper].rest_translation,
        );
        let tgt = alt_tpose();
        let out = retarget_locals(&src, &HumanoidRig::map(&src), &src_locals, &tgt, &HumanoidRig::map(&tgt));

        let upper = tgt.joints.iter().position(|j| j.name == "LeftArm").unwrap();
        let hand = tgt.joints.iter().position(|j| j.name == "LeftHand").unwrap();
        let rest = tgt.world_matrices(&tgt.rest_locals());
        let anim = tgt.world_matrices(&out);
        let upper_rest = rest[upper].w_axis.truncate();
        let hand_rest = rest[hand].w_axis.truncate();
        let upper_anim = anim[upper].w_axis.truncate();
        let hand_anim = anim[hand].w_axis.truncate();
        let rest_off = hand_rest - upper_rest;
        let want = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2) * rest_off;
        let got = hand_anim - upper_anim;
        assert!((got - want).length() < 1e-3, "got {got}, want {want}");
    }

    #[test]
    fn retarget_map_reuses_and_scales_locomotion() {
        let src = tpose();
        let tgt = alt_tpose();
        let src_rig = HumanoidRig::map(&src);
        let tgt_rig = HumanoidRig::map(&tgt);
        let map = HumanoidRetarget::new(&src, &src_rig, &tgt, &tgt_rig);

        // Both rigs expose every role except the (absent) eye bones.
        assert_eq!(map.mapped_count(), 18);
        assert_eq!(map.pairs().len(), 18);

        // Moving the source hips root is carried over, scaled to the target's
        // proportions, and the precomputed map matches the one-shot helper.
        let mut src_locals = src.rest_locals();
        let hips = src.joints.iter().position(|j| j.name == "Hips").unwrap();
        src_locals[hips] = Mat4::from_translation(Vec3::new(0.4, 0.0, 0.0)) * src_locals[hips];
        let out = map.apply(&src_locals);
        let one_shot = retarget_locals(&src, &src_rig, &src_locals, &tgt, &tgt_rig);
        for (a, b) in out.iter().zip(&one_shot) {
            let (ta, ra, sa) = a.to_scale_rotation_translation();
            let (tb, rb, sb) = b.to_scale_rotation_translation();
            assert!((ta - tb).length() < 1e-4, "translation mismatch");
            assert!((ra - rb).length() < 1e-4, "rotation mismatch");
            assert!((sa - sb).length() < 1e-4, "scale mismatch");
        }

        let src_world = src.world_matrices(&src_locals);
        let src_rest = src.world_matrices(&src.rest_locals());
        let tgt_world = tgt.world_matrices(&out);
        let tgt_rest = tgt.world_matrices(&tgt.rest_locals());
        let tgt_hips = tgt.joints.iter().position(|j| j.name == "Hips").unwrap();
        let head = tgt.joints.iter().position(|j| j.name == "Head").unwrap();
        let src_head = src.joints.iter().position(|j| j.name == "Head").unwrap();
        let scale = (tgt_rest[head].w_axis.truncate() - tgt_rest[tgt_hips].w_axis.truncate()).length()
            / (src_rest[src_head].w_axis.truncate() - src_rest[hips].w_axis.truncate()).length();
        let want = tgt_rest[tgt_hips].w_axis.truncate()
            + (src_world[hips].w_axis.truncate() - src_rest[hips].w_axis.truncate()) * scale;
        assert!((tgt_world[tgt_hips].w_axis.truncate() - want).length() < 1e-3);
    }

    #[test]
    fn retarget_clip_samples_then_retargets() {
        let src = tpose();
        let tgt = alt_tpose();
        let clip = AnimationClip { name: "idle".into(), duration: 0.0, channels: vec![] };
        let out = retarget_clip(&src, &HumanoidRig::map(&src), &clip, 0.0, &tgt, &HumanoidRig::map(&tgt));
        let tgt_rest = tgt.rest_locals();
        for (a, b) in out.iter().zip(&tgt_rest) {
            let (ta, ra, sa) = a.to_scale_rotation_translation();
            let (tb, rb, sb) = b.to_scale_rotation_translation();
            assert!((ta - tb).length() < 1e-4, "translation mismatch");
            assert!((ra - rb).length() < 1e-4, "rotation mismatch");
            assert!((sa - sb).length() < 1e-4, "scale mismatch");
        }
    }

    #[test]
    fn three_point_tracking_reaches_head_and_hands_and_crouches_on_planted_feet() {
        let skel = tpose();
        let rig = HumanoidRig::map(&skel);
        let targets = TrackerTargets {
            head: Some((Vec3::new(0.0, 1.55, 0.05), Quat::IDENTITY)),
            left_hand: Some((Vec3::new(0.3, 1.2, 0.3), Quat::IDENTITY)),
            right_hand: Some((Vec3::new(-0.3, 1.2, 0.3), Quat::IDENTITY)),
            ..Default::default()
        };
        let mut locals = skel.rest_locals();
        let mut state = BodyIkState::default();
        solve_body(&skel, &rig, &mut locals, &targets, &BodyIkSettings::default(), &mut state, Mat4::IDENTITY, 1.0 / 60.0);
        assert!((at(&skel, &locals, "Head") - Vec3::new(0.0, 1.55, 0.05)).length() < 0.02);
        assert!((at(&skel, &locals, "LeftHand") - Vec3::new(0.3, 1.2, 0.3)).length() < 0.02);
        assert!((at(&skel, &locals, "RightHand") - Vec3::new(-0.3, 1.2, 0.3)).length() < 0.02);
        // Elbows hang below the shoulder-hand line.
        let (sh, el, ha) = (at(&skel, &locals, "LeftUpperArm"), at(&skel, &locals, "LeftLowerArm"), at(&skel, &locals, "LeftHand"));
        assert!(el.y < (sh.y + ha.y) * 0.5, "elbow {el}");
        // The hips dropped 10 cm with the head; feet stay on the floor and knees bend forward.
        let foot = at(&skel, &locals, "LeftFoot");
        assert!((foot.y - 0.08).abs() < 0.02, "foot {foot}");
        let knee = at(&skel, &locals, "LeftLowerLeg");
        assert!(knee.z > foot.z + 0.02, "knee {knee} foot {foot}");
    }

    #[test]
    fn foot_and_hip_trackers_drive_the_legs_and_knee_trackers_aim_them() {
        let skel = tpose();
        let rig = HumanoidRig::map(&skel);
        let targets = TrackerTargets {
            head: Some((Vec3::new(0.0, 1.62, 0.0), Quat::IDENTITY)),
            hips: Some((Vec3::new(0.0, 0.95, 0.0), Quat::IDENTITY)),
            left_foot: Some((Vec3::new(0.15, 0.1, 0.25), Quat::IDENTITY)),
            right_foot: Some((Vec3::new(-0.15, 0.1, -0.1), Quat::IDENTITY)),
            left_knee: Some((Vec3::new(0.5, 0.5, 0.2), Quat::IDENTITY)),
            ..Default::default()
        };
        let mut locals = skel.rest_locals();
        solve_body(&skel, &rig, &mut locals, &targets, &BodyIkSettings::default(), &mut BodyIkState::default(), Mat4::IDENTITY, 0.0);
        assert!((at(&skel, &locals, "Hips") - Vec3::new(0.0, 0.95, 0.0)).length() < 1e-3);
        assert!((at(&skel, &locals, "LeftFoot") - Vec3::new(0.15, 0.1, 0.25)).length() < 0.02);
        assert!((at(&skel, &locals, "RightFoot") - Vec3::new(-0.15, 0.1, -0.1)).length() < 0.02);
        assert!(at(&skel, &locals, "LeftLowerLeg").x > 0.2, "left knee splays toward its tracker");
    }

    #[test]
    fn planted_feet_hold_then_step_after_the_body() {
        let skel = tpose();
        let rig = HumanoidRig::map(&skel);
        let settings = BodyIkSettings::default();
        let mut state = BodyIkState::default();
        let head = |x: f32| TrackerTargets { head: Some((Vec3::new(x, 1.62, 0.0), Quat::IDENTITY)), ..Default::default() };
        let mut locals = skel.rest_locals();
        solve_body(&skel, &rig, &mut locals, &head(0.0), &settings, &mut state, Mat4::IDENTITY, 1.0 / 60.0);
        let start = at(&skel, &locals, "LeftFoot");
        let mut locals = skel.rest_locals();
        solve_body(&skel, &rig, &mut locals, &head(0.1), &settings, &mut state, Mat4::IDENTITY, 1.0 / 60.0);
        assert!((at(&skel, &locals, "LeftFoot") - start).length() < 0.02, "a small lean keeps the foot planted");
        for _ in 0..90 {
            locals = skel.rest_locals();
            solve_body(&skel, &rig, &mut locals, &head(0.6), &settings, &mut state, Mat4::IDENTITY, 1.0 / 60.0);
        }
        let foot = at(&skel, &locals, "LeftFoot");
        assert!((foot.x - 0.7).abs() < 0.05 && (foot.y - 0.08).abs() < 0.02, "the foot stepped under the body: {foot}");
    }

    #[test]
    fn default_calibration_maps_an_ideal_headset_onto_the_head_bone() {
        let skel = tpose();
        let cal = default_calibration(&skel, &HumanoidRig::map(&skel), 1.0);
        let facing = Quat::from_rotation_y(std::f32::consts::PI);
        let mut raw = TrackerPoses::default();
        raw.0[0] = Some(Mat4::from_rotation_translation(facing, Vec3::new(0.0, 1.72, 0.08)));
        let t = targets_from_calibrated(&cal, &raw);
        let (p, r) = t.head.unwrap();
        assert!((p - Vec3::new(0.0, 1.65, 0.0)).length() < 1e-3, "{p}");
        assert!(r.angle_between(Quat::IDENTITY) < 1e-3);
    }

    #[test]
    fn measures_eye_height_and_arm_span_from_the_rest_pose() {
        let skel = tpose();
        let m = measure_avatar(&skel, &HumanoidRig::map(&skel)).unwrap();
        assert!((m.eye_height - (1.65 + 0.07 * 1.65 / 1.6)).abs() < 1e-4, "{}", m.eye_height);
        assert!((m.arm_span.unwrap() - 1.3).abs() < 1e-4, "wrist to wrist, arms straight");
        // Rooted at the hips (feet below the origin) it still stands on its soles.
        let mut hip_rooted = tpose();
        hip_rooted.joints[0].rest_translation = Vec3::ZERO;
        let h = measure_avatar(&hip_rooted, &HumanoidRig::map(&hip_rooted)).unwrap();
        assert!((h.eye_height - m.eye_height).abs() < 0.02, "{} vs {}", h.eye_height, m.eye_height);
        // Eye bones win over the head estimate.
        let mut eyed = tpose();
        let head = eyed.joints.iter().position(|j| j.name == "Head").unwrap();
        eyed.joints.push(joint_at("LeftEye", Some(head), Vec3::new(0.03, 0.09, 0.08)));
        eyed.joints.push(joint_at("RightEye", Some(head), Vec3::new(-0.03, 0.09, 0.08)));
        let e = measure_avatar(&eyed, &HumanoidRig::map(&eyed)).unwrap();
        assert!((e.eye_height - 1.74).abs() < 1e-4);
    }

    #[test]
    fn scaled_calibration_fits_a_bigger_player() {
        let skel = tpose();
        let rig = HumanoidRig::map(&skel);
        let s = 1.2;
        let mut raw = TrackerPoses::default();
        raw.0[0] = Some(Mat4::from_rotation_translation(Quat::from_rotation_y(std::f32::consts::PI), Vec3::new(0.3, 1.72 * s, 0.1)));
        raw.0[3] = Some(Mat4::from_translation(Vec3::new(0.32, 1.02 * s, 0.05)));
        raw.0[4] = Some(Mat4::from_translation(Vec3::new(0.3 + 0.1 * s, 0.1 * s, 0.1)));
        let cal = calibrate_tpose(&skel, &rig, &raw, s);
        let t = targets_from_calibrated(&cal, &raw);
        // At the T-pose the hips and foot targets are the scaled rest bones under the headset.
        assert!((t.hips.unwrap().0 - Vec3::new(0.3, 1.0 * s, 0.1)).length() < 1e-3, "{}", t.hips.unwrap().0);
        assert!((t.left_foot.unwrap().0.y - 0.08 * s).abs() < 1e-3);
        // Standing up on tiptoes lifts them with the trackers.
        let mut up = raw;
        for p in up.0.iter_mut().flatten() {
            *p = Mat4::from_translation(Vec3::Y * 0.05) * *p;
        }
        assert!((targets_from_calibrated(&cal, &up).hips.unwrap().0.y - (1.0 * s + 0.05)).abs() < 1e-3);
        // The default offsets at a scale put an ideal headset on the scaled head bone.
        let d = default_calibration(&skel, &rig, s);
        let mut hmd = TrackerPoses::default();
        hmd.0[0] = Some(Mat4::from_rotation_translation(Quat::from_rotation_y(std::f32::consts::PI), Vec3::new(0.0, (1.65 + 0.07) * s, 0.08 * s)));
        assert!((targets_from_calibrated(&d, &hmd).head.unwrap().0 - Vec3::new(0.0, 1.65 * s, 0.0)).length() < 1e-3);
    }

    #[test]
    fn arm_reach_scales_the_hand_target_from_the_shoulder() {
        let skel = tpose();
        let rig = HumanoidRig::map(&skel);
        let target = Vec3::new(0.75, 1.5, 0.3);
        let targets = TrackerTargets {
            head: Some((Vec3::new(0.0, 1.65, 0.0), Quat::IDENTITY)),
            left_hand: Some((target, Quat::IDENTITY)),
            ..Default::default()
        };
        let settings = BodyIkSettings { arm_reach: 0.7, shoulder_rotation: 0.0, plant_feet: false, ..Default::default() };
        let mut locals = skel.rest_locals();
        solve_body(&skel, &rig, &mut locals, &targets, &settings, &mut BodyIkState::default(), Mat4::IDENTITY, 0.0);
        let (shoulder, hand) = (at(&skel, &locals, "LeftUpperArm"), at(&skel, &locals, "LeftHand"));
        let want = shoulder + (target - shoulder) * 0.7;
        assert!(hand.distance(want) < 0.02, "hand {hand} want {want}");
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
