//! Animation graph runtime: evaluates Animator Controllers (`.animctrl` state
//! machines) and authored `.anim` clips for an [`Animator`].
//!
//! A [`Rig`] lists what an Animator animates: skeleton joints for skinned
//! models, or the object and its descendants (by relative path) otherwise.
//! Clips sample into a [`Pose`] (TRS + one scalar lane per node, with a bit
//! mask of the channels a clip actually drives), poses cross-fade and layers
//! compose (override with weight, additive over the rest pose, per-node mask).
//! Untouched channels keep their rest value, so only animated properties are
//! written back to objects.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use glam::{Mat4, Quat, Vec3};

use zestengine_assets::{
    AnimClip, AnimParamKind, AnimationClip, AnimatorController, ChannelPath, LayerBlend, Motion, Skeleton, StateMachine,
    TrackProperty,
};
use zestengine_core::{Animator, BlendTree, LayerPlayback};

pub const CH_T: u8 = 1;
pub const CH_R: u8 = 2;
pub const CH_S: u8 = 4;
pub const CH_F: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Trs {
    pub t: Vec3,
    pub r: Quat,
    pub s: Vec3,
}

impl Default for Trs {
    fn default() -> Self {
        Self { t: Vec3::ZERO, r: Quat::IDENTITY, s: Vec3::ONE }
    }
}

impl Trs {
    pub fn to_mat4(self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.s, self.r, self.t)
    }
}

/// The nodes an Animator drives.
#[derive(Clone, Debug, Default)]
pub struct Rig {
    pub names: Vec<String>,
    pub parents: Vec<Option<usize>>,
    pub rest: Vec<Trs>,
    /// Rest value of the scalar lane (light intensity).
    pub rest_scalar: Vec<f32>,
    /// Imported skeletal clips address nodes by joint index.
    pub skeletal: bool,
    lookup: HashMap<String, usize>,
}

impl Rig {
    pub fn new(names: Vec<String>, parents: Vec<Option<usize>>, rest: Vec<Trs>, rest_scalar: Vec<f32>, skeletal: bool) -> Self {
        let lookup = names.iter().enumerate().map(|(i, n)| (n.clone(), i)).collect();
        Self { names, parents, rest, rest_scalar, skeletal, lookup }
    }

    pub fn from_skeleton(skel: &Skeleton) -> Self {
        let n = skel.joints.len();
        Self::new(
            skel.joints.iter().map(|j| j.name.clone()).collect(),
            skel.joints.iter().map(|j| j.parent).collect(),
            skel.joints.iter().map(|j| Trs { t: j.rest_translation, r: j.rest_rotation, s: j.rest_scale }).collect(),
            vec![0.0; n],
            true,
        )
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn index(&self, name: &str) -> Option<usize> {
        self.lookup.get(name).copied()
    }

    /// Nodes covered by a layer mask (each listed node plus its descendants).
    /// None = every node.
    pub fn mask(&self, names: &[String]) -> Option<Vec<bool>> {
        if names.is_empty() {
            return None;
        }
        let roots: Vec<usize> = names.iter().filter_map(|n| self.index(n)).collect();
        let mut out = vec![false; self.len()];
        for (i, slot) in out.iter_mut().enumerate() {
            let mut cur = Some(i);
            let mut guard = 0;
            while let Some(c) = cur {
                if roots.contains(&c) {
                    *slot = true;
                    break;
                }
                cur = self.parents.get(c).copied().flatten();
                guard += 1;
                if guard > 256 {
                    break;
                }
            }
        }
        Some(out)
    }
}

/// Per-node values plus which channels were driven.
#[derive(Clone, Debug)]
pub struct Pose {
    pub trs: Vec<Trs>,
    pub scalar: Vec<f32>,
    pub touched: Vec<u8>,
}

impl Pose {
    pub fn rest(rig: &Rig) -> Self {
        Self { trs: rig.rest.clone(), scalar: rig.rest_scalar.clone(), touched: vec![0; rig.len()] }
    }

    pub fn to_locals(&self) -> Vec<Mat4> {
        self.trs.iter().map(|t| t.to_mat4()).collect()
    }
}

/// Per-node TRS blend (`w` = 0 -> `a`, 1 -> `b`).
pub fn blend_pose(a: &Pose, b: &Pose, w: f32) -> Pose {
    let n = a.trs.len().min(b.trs.len());
    let mut out = a.clone();
    for i in 0..n {
        let (x, y) = (a.trs[i], b.trs[i]);
        out.trs[i] = Trs { t: x.t.lerp(y.t, w), r: x.r.slerp(y.r, w), s: x.s.lerp(y.s, w) };
        out.scalar[i] = a.scalar[i] + (b.scalar[i] - a.scalar[i]) * w;
        out.touched[i] = a.touched[i] | b.touched[i];
    }
    out
}

/// Weighted average of several poses (weights need not be normalized).
fn blend_many(poses: &[(Pose, f32)]) -> Option<Pose> {
    let (first, _) = poses.first()?;
    let total: f32 = poses.iter().map(|(_, w)| *w).sum::<f32>().max(1e-6);
    let mut out = first.clone();
    for i in 0..first.trs.len() {
        let (mut t, mut s, mut q, mut f) = (Vec3::ZERO, Vec3::ZERO, glam::Vec4::ZERO, 0.0);
        let reference = first.trs[i].r;
        let mut touched = 0;
        for (p, w) in poses {
            let w = w / total;
            let x = p.trs[i];
            t += x.t * w;
            s += x.s * w;
            let r = if x.r.dot(reference) < 0.0 { -x.r } else { x.r };
            q += glam::Vec4::from(r) * w;
            f += p.scalar[i] * w;
            touched |= p.touched[i];
        }
        out.trs[i] = Trs { t, r: Quat::from_vec4(q).normalize(), s };
        out.scalar[i] = f;
        out.touched[i] = touched;
    }
    Some(out)
}

// ------------------------------------------------------------------ clips

/// An imported skeletal clip or an authored `.anim` asset.
#[derive(Clone, Copy)]
pub enum ClipRef<'a> {
    Imported(&'a AnimationClip),
    Asset(&'a AnimClip),
}

impl ClipRef<'_> {
    pub fn duration(&self) -> f32 {
        match self {
            ClipRef::Imported(c) => c.duration,
            ClipRef::Asset(c) => c.length,
        }
    }

    /// Write this clip's channels at time `t` into `pose`.
    pub fn sample(&self, rig: &Rig, t: f32, looping: bool, pose: &mut Pose) {
        let d = self.duration();
        let tt = if d <= 1e-6 {
            0.0
        } else if looping {
            t.rem_euclid(d)
        } else {
            t.clamp(0.0, (d - 1e-4).max(0.0))
        };
        match self {
            ClipRef::Imported(clip) => {
                if !rig.skeletal {
                    return;
                }
                for ch in &clip.channels {
                    if ch.joint >= rig.len() || ch.times.is_empty() {
                        continue;
                    }
                    let (i0, i1, f) = key_indices(&ch.times, tt);
                    let n = &mut pose.trs[ch.joint];
                    match ch.path {
                        ChannelPath::Translation => {
                            if let (Some(a), Some(b)) = (ch.vec_values.get(i0), ch.vec_values.get(i1)) {
                                n.t = a.lerp(*b, f);
                                pose.touched[ch.joint] |= CH_T;
                            }
                        }
                        ChannelPath::Scale => {
                            if let (Some(a), Some(b)) = (ch.vec_values.get(i0), ch.vec_values.get(i1)) {
                                n.s = a.lerp(*b, f);
                                pose.touched[ch.joint] |= CH_S;
                            }
                        }
                        ChannelPath::Rotation => {
                            if let (Some(a), Some(b)) = (ch.quat_values.get(i0), ch.quat_values.get(i1)) {
                                n.r = a.slerp(*b, f);
                                pose.touched[ch.joint] |= CH_R;
                            }
                        }
                    }
                }
            }
            ClipRef::Asset(clip) => {
                for track in &clip.tracks {
                    let Some(node) = rig.index(&track.target) else {
                        continue;
                    };
                    if track.keys.is_empty() {
                        continue;
                    }
                    let v = track.sample(tt);
                    match track.property {
                        TrackProperty::Translation => {
                            pose.trs[node].t = Vec3::new(v[0], v[1], v[2]);
                            pose.touched[node] |= CH_T;
                        }
                        TrackProperty::Rotation => {
                            pose.trs[node].r = Quat::from_array(v).normalize();
                            pose.touched[node] |= CH_R;
                        }
                        TrackProperty::Scale => {
                            pose.trs[node].s = Vec3::new(v[0], v[1], v[2]);
                            pose.touched[node] |= CH_S;
                        }
                        TrackProperty::LightIntensity => {
                            pose.scalar[node] = v[0];
                            pose.touched[node] |= CH_F;
                        }
                    }
                }
            }
        }
    }
}

fn key_indices(times: &[f32], t: f32) -> (usize, usize, f32) {
    if times.len() == 1 || t <= times[0] {
        return (0, 0, 0.0);
    }
    let last = times.len() - 1;
    if t >= times[last] {
        return (last, last, 0.0);
    }
    let i = times.partition_point(|&k| k <= t).saturating_sub(1).min(last - 1);
    let (a, b) = (times[i], times[i + 1]);
    let f = if (b - a).abs() > 1e-6 { (t - a) / (b - a) } else { 0.0 };
    (i, i + 1, f.clamp(0.0, 1.0))
}

/// Clips reachable by name for one Animator: the model's imported clips plus
/// loaded `.anim` assets (looked up by project-relative path).
pub struct ClipSet<'a> {
    pub imported: &'a [AnimationClip],
    /// Indices into `imported` that belong to this Animator's skeleton.
    pub indices: &'a [usize],
    pub assets: &'a HashMap<String, Option<Arc<AnimClip>>>,
}

impl<'a> ClipSet<'a> {
    pub fn find(&self, name: &str) -> Option<ClipRef<'a>> {
        if is_clip_asset(name) {
            return self.assets.get(name)?.as_deref().map(ClipRef::Asset);
        }
        let imported = self.imported;
        self.indices.iter().copied().find(|&c| imported[c].name == name).map(|c| ClipRef::Imported(&imported[c]))
    }
}

pub fn is_clip_asset(name: &str) -> bool {
    // `@<id>` references come from clip asset slots.
    zestengine_core::asset_ref::is_ref(name) || name.to_ascii_lowercase().ends_with(".anim")
}

// ----------------------------------------------------------------- motions

fn tree_weights(tree: &BlendTree, params: &HashMap<String, f32>) -> Vec<f32> {
    let p = |n: &str| params.get(n).copied().unwrap_or(0.0);
    tree.weights([p(&tree.parameter), p(&tree.parameter_y)])
}

/// Length of one pass of a motion (blend trees: weight-averaged).
pub fn motion_duration(motion: &Motion, params: &HashMap<String, f32>, clips: &ClipSet) -> f32 {
    match motion {
        Motion::Clip(name) => clips.find(name).map(|c| c.duration()).unwrap_or(0.0),
        Motion::BlendTree(tree) => tree
            .motions
            .iter()
            .zip(tree_weights(tree, params))
            .filter_map(|(m, w)| clips.find(&m.clip).map(|c| c.duration() * w))
            .sum(),
    }
}

/// Pose of a motion at `time` seconds (blend-tree clips phase-synced).
pub fn sample_motion(
    motion: &Motion,
    params: &HashMap<String, f32>,
    clips: &ClipSet,
    rig: &Rig,
    time: f32,
    looping: bool,
) -> Pose {
    let mut pose = Pose::rest(rig);
    match motion {
        Motion::Clip(name) => {
            if let Some(c) = clips.find(name) {
                c.sample(rig, time, looping, &mut pose);
            }
            pose
        }
        Motion::BlendTree(tree) => {
            let w = tree_weights(tree, params);
            let parts: Vec<(ClipRef, f32)> = tree
                .motions
                .iter()
                .zip(&w)
                .filter(|(_, w)| **w > 1e-4)
                .filter_map(|(m, &w)| clips.find(&m.clip).map(|c| (c, w)))
                .collect();
            // Each clip advances at its own rate off the shared state clock, so
            // moving the blend between clips (idle <-> walk) never rewinds or
            // jumps a clip mid-cycle. The old weight-averaged phase made the
            // walk visibly restart as the weights shifted between clips with
            // different lengths.
            let poses: Vec<(Pose, f32)> = parts
                .iter()
                .map(|(c, w)| {
                    let mut p = Pose::rest(rig);
                    c.sample(rig, time, looping, &mut p);
                    (p, *w)
                })
                .collect();
            blend_many(&poses).unwrap_or(pose)
        }
    }
}

/// The clip carrying most weight in a motion, with its name and the local
/// time matching motion time `time`. Drives events and root motion.
fn dominant_clip<'a>(
    motion: &'a Motion,
    params: &HashMap<String, f32>,
    clips: &ClipSet<'a>,
    time: f32,
) -> Option<(&'a str, ClipRef<'a>, f32)> {
    match motion {
        Motion::Clip(name) => clips.find(name).map(|c| (name.as_str(), c, time)),
        Motion::BlendTree(tree) => {
            let w = tree_weights(tree, params);
            let parts: Vec<(&str, ClipRef, f32)> = tree
                .motions
                .iter()
                .zip(&w)
                .filter_map(|(m, &w)| clips.find(&m.clip).map(|c| (m.clip.as_str(), c, w)))
                .collect();
            let (name, c, _) = parts.iter().copied().max_by(|a, b| a.2.total_cmp(&b.2))?;
            // Local time is the raw state time: `sample_motion` samples each
            // blend-tree clip at `time`, so events and root motion must follow
            // the same clock rather than a weight-averaged phase.
            Some((name, c, time))
        }
    }
}

/// Event names passed between clip-local times `a` and `b` (wrapping when looping).
fn crossed_events(animator: &Animator, name: &str, clip: ClipRef, a: f32, b: f32, looping: bool, out: &mut Vec<String>) {
    let cd = clip.duration();
    if cd <= 1e-6 || b <= a {
        return;
    }
    let (a, b) = if looping { (a, b) } else { (a.clamp(0.0, cd), b.clamp(0.0, cd)) };
    let mut marks: Vec<(f32, &str)> =
        animator.events.iter().filter(|e| e.clip == name).map(|e| (e.time, e.name.as_str())).collect();
    if let ClipRef::Asset(c) = clip {
        marks.extend(c.events.iter().map(|e| (e.time, e.name.as_str())));
    }
    for (time, ev) in marks {
        let first = (a / cd).floor() as i64;
        let last = (b / cd).floor() as i64;
        for k in first..=last {
            let t = k as f32 * cd + time;
            if t > a && t <= b {
                out.push(ev.to_owned());
            }
        }
    }
}

/// Horizontal root-joint motion of an imported clip between local times `a`
/// and `b`, plus the clip's start root position (to pin the skeleton).
fn root_motion(rig: &Rig, clip: ClipRef, a: f32, b: f32, looping: bool) -> Option<(Vec3, Vec3)> {
    let ClipRef::Imported(_) = clip else {
        return None;
    };
    let root = rig.parents.iter().position(|p| p.is_none())?;
    let cd = clip.duration();
    if cd <= 1e-6 {
        return None;
    }
    let root_at = |t: f32| {
        let mut p = Pose::rest(rig);
        clip.sample(rig, t, true, &mut p);
        p.trs[root].t
    };
    let wrap = |t: f32| t.rem_euclid(cd);
    let start = root_at(0.0);
    let mut d = if looping && (b / cd).floor() > (a / cd).floor() {
        (root_at(cd - 1e-4) - root_at(wrap(a))) + (root_at(wrap(b)) - start)
    } else if looping {
        root_at(wrap(b)) - root_at(wrap(a))
    } else {
        root_at(b.clamp(0.0, cd - 1e-4)) - root_at(a.clamp(0.0, cd - 1e-4))
    };
    d.y = 0.0;
    Some((d, start))
}

// -------------------------------------------------------------- controller

/// Result of advancing an Animator one frame.
pub struct AnimOutput {
    pub pose: Pose,
    /// Horizontal root motion (rig space).
    pub root_delta: Vec3,
}

fn state_duration(machine: &StateMachine, index: usize, params: &HashMap<String, f32>, clips: &ClipSet) -> f32 {
    machine.states.get(index).map(|s| motion_duration(&s.motion, params, clips)).unwrap_or(0.0)
}

/// Normalizing length: a state without a clip counts as one second so exit
/// times still work.
fn norm_len(d: f32) -> f32 {
    if d > 1e-4 { d } else { 1.0 }
}

/// Whether a transition's exit time was reached this frame.
fn exit_reached(prev: f32, now: f32, exit: f32, looping: bool) -> bool {
    if now < exit {
        return false;
    }
    if looping && exit < 1.0 {
        // Checked once per loop, at the exit point.
        (now - exit).floor() > (prev - exit).floor()
    } else {
        true
    }
}

#[allow(clippy::too_many_arguments)]
fn enter_state(
    animator: &mut Animator,
    machine: &StateMachine,
    layer: usize,
    to: usize,
    fade: f32,
    transition: Option<usize>,
    offset: f32,
    params: &HashMap<String, f32>,
    clips: &ClipSet,
) {
    let from_name = animator.state.layers[layer].state_name.clone();
    if let Some(from) = machine.states.get(animator.state.layers[layer].state)
        && !from.on_exit.is_empty()
    {
        animator.state.fired.push(from.on_exit.clone());
    }
    let target = &machine.states[to];
    if !target.on_enter.is_empty() {
        animator.state.fired.push(target.on_enter.clone());
    }
    // Drained by the play session; bounded when nothing drains it (editor preview).
    if animator.state.state_changes.len() >= 32 {
        animator.state.state_changes.remove(0);
    }
    animator.state.state_changes.push((layer, from_name, target.name.clone()));
    let len = norm_len(state_duration(machine, to, params, clips));
    let lp = &mut animator.state.layers[layer];
    lp.previous = if fade > 0.0 { Some((lp.state, lp.time)) } else { None };
    lp.fade_elapsed = 0.0;
    lp.fade_duration = fade.max(0.0);
    lp.state = to;
    lp.state_name = target.name.clone();
    lp.time = offset.max(0.0) * len;
    lp.normalized_time = offset.max(0.0);
    lp.transition = if fade > 0.0 { transition } else { None };
}

/// (Re)start the controller: parameter defaults (kept when already set from
/// code) and every layer in its default state.
fn init_controller(animator: &mut Animator, ctrl: &AnimatorController, path: &str) {
    for p in &ctrl.parameters {
        animator.state.params.entry(p.name.clone()).or_insert(p.default);
    }
    animator.state.layers = ctrl
        .layers
        .iter()
        .map(|l| {
            let state = l.machine.default_index().unwrap_or(0);
            LayerPlayback {
                state,
                state_name: l.machine.states.get(state).map(|s| s.name.clone()).unwrap_or_default(),
                ..Default::default()
            }
        })
        .collect();
    for l in &ctrl.layers {
        if let Some(s) = l.machine.default_index().and_then(|i| l.machine.states.get(i))
            && !s.on_enter.is_empty()
        {
            animator.state.fired.push(s.on_enter.clone());
        }
    }
    animator.state.controller_ready = Some(path.to_owned());
    animator.state.started = true;
    animator.state.playing = true;
}

/// Advance an Animator driven by a controller by `dt` seconds.
pub fn advance_controller(
    animator: &mut Animator,
    ctrl: &AnimatorController,
    path: &str,
    rig: &Rig,
    clips: &ClipSet,
    dt: f32,
) -> AnimOutput {
    // Sub-state machines run flattened (`Outer/Inner` state names); assets
    // from `AnimAssets` arrive flattened already.
    let flat;
    let ctrl = if ctrl.layers.iter().any(|l| l.machine.has_sub_machines()) {
        flat = ctrl.flattened();
        &flat
    } else {
        ctrl
    };
    animator.state.fired.clear();
    if animator.state.controller_ready.as_deref() != Some(path) || animator.state.layers.len() != ctrl.layers.len() {
        init_controller(animator, ctrl, path);
    }

    // Forced states from code.
    for req in std::mem::take(&mut animator.state.requests) {
        let target = match req.layer {
            Some(l) => ctrl.layers.get(l).and_then(|layer| layer.machine.state_index(&req.state).map(|s| (l, s))),
            None => ctrl.layers.iter().enumerate().find_map(|(l, layer)| layer.machine.state_index(&req.state).map(|s| (l, s))),
        };
        if let Some((l, s)) = target {
            let params = animator.state.params.clone();
            enter_state(animator, &ctrl.layers[l].machine, l, s, req.fade, None, 0.0, &params, clips);
            animator.state.playing = true;
        }
    }

    let playing = animator.state.playing;
    let mut out: Option<Pose> = None;
    let mut root_delta = Vec3::ZERO;

    for (li, layer) in ctrl.layers.iter().enumerate() {
        let machine = &layer.machine;
        if machine.states.is_empty() {
            continue;
        }
        // The asset may have been edited live: keep indices valid.
        {
            let lp = &mut animator.state.layers[li];
            if lp.state >= machine.states.len() || machine.states[lp.state].name != lp.state_name {
                match machine.state_index(&lp.state_name).or(machine.default_index()) {
                    Some(i) => {
                        lp.state = i;
                        lp.state_name = machine.states[i].name.clone();
                    }
                    None => continue,
                }
            }
            if lp.previous.is_some_and(|(p, _)| p >= machine.states.len()) {
                lp.previous = None;
            }
            if lp.transition.is_some_and(|t| t >= machine.transitions.len()) {
                lp.transition = None;
            }
        }

        let params = animator.state.params.clone();
        let speed_of = |i: usize| {
            let s = &machine.states[i];
            let p = if s.speed_parameter.is_empty() { 1.0 } else { params.get(&s.speed_parameter).copied().unwrap_or(1.0) };
            s.speed * p * animator.speed
        };
        let cur = animator.state.layers[li].state;
        let st = &machine.states[cur];
        let len = norm_len(state_duration(machine, cur, &params, clips));
        let step = if playing { dt * speed_of(cur) } else { 0.0 };
        let prev_time = animator.state.layers[li].time;
        let prev_norm = prev_time / len;
        {
            let lp = &mut animator.state.layers[li];
            lp.time += step;
            if !st.looping {
                lp.time = lp.time.clamp(0.0, len);
            }
            lp.normalized_time = lp.time / len;
            if let Some((p, t)) = lp.previous.as_mut() {
                let p = *p;
                if playing {
                    *t += dt * speed_of(p);
                }
            }
            if playing {
                lp.fade_elapsed += dt;
            }
            if lp.fade_duration <= 0.0 || lp.fade_elapsed >= lp.fade_duration {
                lp.previous = None;
                lp.transition = None;
            }
        }

        // Events from the current state's dominant clip.
        let new_time = animator.state.layers[li].time;
        if let (Some((name, clip, a)), Some((_, _, b))) =
            (dominant_clip(&st.motion, &params, clips, prev_time), dominant_clip(&st.motion, &params, clips, new_time))
        {
            let mut fired = Vec::new();
            crossed_events(animator, name, clip, a, b, st.looping, &mut fired);
            animator.state.fired.extend(fired);
            if li == 0
                && animator.root_motion
                && let Some((d, _)) = root_motion(rig, clip, a, b, st.looping)
            {
                root_delta = d;
            }
        }

        // Transitions: Any State first, then the current state's own.
        let now_norm = animator.state.layers[li].normalized_time;
        let active = animator.state.layers[li].transition;
        let can_check = active.is_none_or(|ti| machine.transitions.get(ti).is_none_or(|t| t.interruptible));
        if can_check && playing {
            let order = machine
                .transitions
                .iter()
                .enumerate()
                .filter(|(_, t)| t.from.is_none())
                .chain(machine.transitions.iter().enumerate().filter(|(_, t)| t.from.as_deref() == Some(st.name.as_str())));
            let mut fire: Option<(usize, usize)> = None;
            for (ti, t) in order {
                if Some(ti) == active {
                    continue;
                }
                let Some(to) = machine.state_index(&t.to) else {
                    continue;
                };
                if t.from.is_none() && to == cur && !t.can_transition_to_self {
                    continue;
                }
                if !t.has_exit_time && t.conditions.is_empty() {
                    continue;
                }
                if t.has_exit_time && !exit_reached(prev_norm, now_norm, t.exit_time, st.looping) {
                    continue;
                }
                if !t.conditions.iter().all(|c| c.op.test(params.get(&c.parameter).copied().unwrap_or(0.0), c.threshold)) {
                    continue;
                }
                fire = Some((ti, to));
                break;
            }
            if let Some((ti, to)) = fire {
                let t = &machine.transitions[ti];
                for c in &t.conditions {
                    if ctrl.parameter(&c.parameter).is_some_and(|p| p.kind == AnimParamKind::Trigger) {
                        animator.state.params.insert(c.parameter.clone(), 0.0);
                    }
                }
                let params = animator.state.params.clone();
                enter_state(animator, machine, li, to, t.duration, Some(ti), t.offset, &params, clips);
            }
        }

        // Sample: current state, cross-faded from the previous one.
        let lp = animator.state.layers[li].clone();
        let st = &machine.states[lp.state];
        let mut pose = sample_motion(&st.motion, &params, clips, rig, lp.time, st.looping);
        if let Some((p, t)) = lp.previous {
            let ps = &machine.states[p];
            let prev = sample_motion(&ps.motion, &params, clips, rig, t, ps.looping);
            pose = blend_pose(&prev, &pose, lp.transition_progress());
        }
        if li == 0 && animator.root_motion && rig.skeletal {
            pin_root(rig, &mut pose, &st.motion, &params, clips);
        }

        out = Some(match out {
            None => pose,
            Some(base) => compose_layer(rig, base, &pose, layer.weight, layer.blend, &layer.mask),
        });
    }

    let layer0 = animator.state.layers.first().map(|l| (l.state_name.clone(), l.time));
    if let Some((name, time)) = layer0 {
        animator.state.current = Some(name);
        animator.state.time = time;
    }
    AnimOutput { pose: out.unwrap_or_else(|| Pose::rest(rig)), root_delta }
}

/// Keep the skeleton's root in place horizontally (root motion moves the object).
fn pin_root(rig: &Rig, pose: &mut Pose, motion: &Motion, params: &HashMap<String, f32>, clips: &ClipSet) {
    let Some(root) = rig.parents.iter().position(|p| p.is_none()) else {
        return;
    };
    if let Some((_, clip @ ClipRef::Imported(_), _)) = dominant_clip(motion, params, clips, 0.0) {
        let mut p = Pose::rest(rig);
        clip.sample(rig, 0.0, true, &mut p);
        let start = p.trs[root].t;
        pose.trs[root].t.x = start.x;
        pose.trs[root].t.z = start.z;
    }
}

/// Apply a layer's pose over the layers below it.
pub fn compose_layer(rig: &Rig, mut base: Pose, layer: &Pose, weight: f32, blend: LayerBlend, mask: &[String]) -> Pose {
    let w = weight.clamp(0.0, 1.0);
    if w <= 0.0 {
        return base;
    }
    let mask = rig.mask(mask);
    for i in 0..base.trs.len().min(layer.trs.len()) {
        if mask.as_ref().is_some_and(|m| !m[i]) {
            continue;
        }
        let ch = layer.touched[i];
        if ch == 0 {
            continue;
        }
        let (b, l, rest) = (base.trs[i], layer.trs[i], rig.rest[i]);
        match blend {
            LayerBlend::Override => {
                if ch & CH_T != 0 {
                    base.trs[i].t = b.t.lerp(l.t, w);
                }
                if ch & CH_R != 0 {
                    base.trs[i].r = b.r.slerp(l.r, w);
                }
                if ch & CH_S != 0 {
                    base.trs[i].s = b.s.lerp(l.s, w);
                }
                if ch & CH_F != 0 {
                    base.scalar[i] += (layer.scalar[i] - base.scalar[i]) * w;
                }
            }
            LayerBlend::Additive => {
                if ch & CH_T != 0 {
                    base.trs[i].t = b.t + (l.t - rest.t) * w;
                }
                if ch & CH_R != 0 {
                    let delta = rest.r.inverse() * l.r;
                    base.trs[i].r = (b.r * Quat::IDENTITY.slerp(delta, w)).normalize();
                }
                if ch & CH_S != 0 {
                    let ratio = l.s / rest.s.max(Vec3::splat(1e-6));
                    base.trs[i].s = b.s * Vec3::ONE.lerp(ratio, w);
                }
                if ch & CH_F != 0 {
                    base.scalar[i] += (layer.scalar[i] - rig.rest_scalar[i]) * w;
                }
            }
        }
        base.touched[i] |= ch;
    }
    base
}

/// Advance an Animator without a controller (direct `play` of clips, `.anim`
/// assets or its own blend trees), with cross-fades and events.
pub fn advance_simple(animator: &mut Animator, rig: &Rig, clips: &ClipSet, dt: f32) -> AnimOutput {
    animator.state.fired.clear();
    if !animator.state.started {
        animator.state.started = true;
        if animator.play_on_start && !animator.clip.is_empty() {
            animator.state.current = Some(animator.clip.clone());
            animator.state.playing = true;
        }
    }
    let motion_of = |name: &str| -> Motion {
        match animator.blend_trees.iter().find(|t| t.name == name) {
            Some(tree) => Motion::BlendTree(tree.clone()),
            None => Motion::Clip(name.to_owned()),
        }
    };
    let Some(current) = animator.state.current.clone() else {
        return AnimOutput { pose: Pose::rest(rig), root_delta: Vec3::ZERO };
    };
    let motion = motion_of(&current);
    let looping = animator.looping;
    let params = animator.state.params.clone();
    let prev_time = animator.state.time;
    if animator.state.playing {
        let s = &mut animator.state;
        s.time += dt * animator.speed;
        s.fade_elapsed += dt;
        if let Some((_, t)) = s.previous.as_mut() {
            *t += dt * animator.speed;
        }
    }
    let duration = motion_duration(&motion, &params, clips);
    animator.state.duration = duration;
    if !looping && duration > 0.0 {
        animator.state.time = animator.state.time.min(duration);
    }
    let time = animator.state.time;
    if let (Some((name, clip, a)), Some((_, _, b))) =
        (dominant_clip(&motion, &params, clips, prev_time), dominant_clip(&motion, &params, clips, time))
    {
        let mut fired = Vec::new();
        crossed_events(animator, name, clip, a, b, looping, &mut fired);
        animator.state.fired.extend(fired);
    }
    let mut pose = sample_motion(&motion, &params, clips, rig, time, looping);
    let s = &mut animator.state;
    let w = if s.fade_duration > 0.0 { (s.fade_elapsed / s.fade_duration).min(1.0) } else { 1.0 };
    if w >= 1.0 {
        s.previous = None;
    }
    if let Some((name, t)) = s.previous.clone() {
        let prev = sample_motion(&motion_of(&name), &params, clips, rig, t, looping);
        pose = blend_pose(&prev, &pose, w);
    }
    AnimOutput { pose, root_delta: Vec3::ZERO }
}

/// Whether an Animator needs the graph runtime (controller or `.anim` clips)
/// rather than the classic imported-clip path.
pub fn wants_graph(animator: &Animator) -> bool {
    !animator.controller.is_empty()
        || is_clip_asset(&animator.clip)
        || animator.state.current.as_deref().is_some_and(is_clip_asset)
}

// ------------------------------------------------------------------ assets

/// Loaded controllers and `.anim` clips by project-relative path, plus cached
/// skeleton rigs. Edited assets are pushed in by the editor so changes show
/// live while playing.
#[derive(Default)]
pub struct AnimAssets {
    root: Option<PathBuf>,
    controllers: HashMap<String, Option<Arc<AnimatorController>>>,
    pub clips: HashMap<String, Option<Arc<AnimClip>>>,
    skeleton_rigs: HashMap<usize, Arc<Rig>>,
}

impl AnimAssets {
    pub fn set_root(&mut self, root: &Path) {
        if self.root.as_deref() != Some(root) {
            self.root = Some(root.to_owned());
            self.controllers.retain(|_, c| c.is_some());
            self.clips.retain(|_, c| c.is_some());
        }
    }

    /// Whether two references (paths or `@<id>`s) name the same file.
    pub fn same_asset(&self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        let Some(root) = self.root.as_deref() else { return false };
        let r = |s: &str| zestengine_core::asset_ref::resolve(root, s);
        r(a).is_some() && r(a) == r(b)
    }

    /// Cache keys that refer to `path` (it, and any `@<id>` naming it).
    fn keys_for<'k>(&self, path: &str, keys: impl Iterator<Item = &'k String>) -> Vec<String> {
        let mut out = vec![path.to_owned()];
        out.extend(keys.filter(|k| zestengine_core::asset_ref::is_ref(k) && self.same_asset(k, path)).cloned());
        out
    }

    /// Replace a controller with an in-memory version (editor live edit).
    pub fn set_controller(&mut self, path: &str, ctrl: AnimatorController) {
        let ctrl = ctrl.flattened();
        self.load_controller_clips(&ctrl);
        let arc = Some(Arc::new(ctrl));
        for k in self.keys_for(path, self.controllers.keys()) {
            self.controllers.insert(k, arc.clone());
        }
    }

    /// Replace a clip with an in-memory version (editor live edit).
    pub fn set_clip(&mut self, path: &str, clip: AnimClip) {
        let arc = Some(Arc::new(clip));
        for k in self.keys_for(path, self.clips.keys()) {
            self.clips.insert(k, arc.clone());
        }
    }

    /// Forget everything loaded from disk (re-read on next use).
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.controllers.clear();
        self.clips.clear();
    }

    /// A controller with its sub-state machines flattened (what the runtime plays).
    pub fn controller(&mut self, path: &str) -> Option<Arc<AnimatorController>> {
        if let Some(c) = self.controllers.get(path) {
            return c.clone();
        }
        let loaded = self.root.as_ref().and_then(|root| match zestengine_assets::load_anim_controller(zestengine_core::asset_ref::resolve_path(root, path)) {
            Ok(c) => Some(c.flattened()),
            Err(e) => {
                tracing::warn!("animator controller {path}: {e:#}");
                None
            }
        });
        if let Some(c) = &loaded {
            self.load_controller_clips(c);
        }
        let arc = loaded.map(Arc::new);
        self.controllers.insert(path.to_owned(), arc.clone());
        arc
    }

    pub fn ensure_clip(&mut self, name: &str) {
        if !is_clip_asset(name) || self.clips.contains_key(name) {
            return;
        }
        let loaded = self.root.as_ref().and_then(|root| match zestengine_assets::load_anim_clip(zestengine_core::asset_ref::resolve_path(root, name)) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                tracing::warn!("animation clip {name}: {e:#}");
                None
            }
        });
        self.clips.insert(name.to_owned(), loaded);
    }

    fn load_controller_clips(&mut self, ctrl: &AnimatorController) {
        for layer in &ctrl.layers {
            for s in &layer.machine.states {
                match &s.motion {
                    Motion::Clip(c) => self.ensure_clip(c),
                    Motion::BlendTree(t) => {
                        for m in &t.motions {
                            self.ensure_clip(&m.clip);
                        }
                    }
                }
            }
        }
    }

    /// Load what an Animator references; returns its controller if it has one.
    pub fn prepare(&mut self, animator: &Animator) -> Option<Arc<AnimatorController>> {
        self.ensure_clip(&animator.clip);
        if let Some(c) = animator.state.current.as_deref() {
            self.ensure_clip(c);
        }
        for t in &animator.blend_trees {
            for m in &t.motions {
                self.ensure_clip(&m.clip);
            }
        }
        if animator.controller.is_empty() { None } else { self.controller(&animator.controller) }
    }

    pub fn skeleton_rig(&mut self, index: usize, skel: &Skeleton) -> Arc<Rig> {
        let rig = self.skeleton_rigs.entry(index).or_insert_with(|| Arc::new(Rig::from_skeleton(skel)));
        if rig.len() != skel.joints.len() {
            *rig = Arc::new(Rig::from_skeleton(skel));
        }
        rig.clone()
    }
}

// ------------------------------------------------------- object hierarchies

/// Rig over an object and its descendants, named by path relative to `root`
/// ("" = root, "Arm/Hand"). Rest values are the objects' current local
/// transforms. Returns the rig and the scene index of each node.
pub fn object_rig(scene: &crate::scene::LoadedScene, root: usize) -> (Rig, Vec<usize>) {
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, o) in scene.objects.iter().enumerate() {
        if let Some(p) = o.parent {
            children.entry(p).or_default().push(i);
        }
    }
    let (mut names, mut parents, mut rest, mut scalar, mut nodes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut stack: Vec<(usize, Option<usize>, String)> = vec![(root, None, String::new())];
    while let Some((i, parent, path)) = stack.pop() {
        if nodes.len() > 4096 {
            break;
        }
        let o = &scene.objects[i];
        let me = nodes.len();
        names.push(path.clone());
        parents.push(parent);
        rest.push(Trs { t: o.translation, r: o.rotation, s: o.scale });
        scalar.push(
            o.components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<zestengine_core::LightComponent>())
                .map(|l| l.intensity)
                .unwrap_or(0.0),
        );
        nodes.push(i);
        if let Some(kids) = children.get(&i) {
            for &k in kids.iter().rev() {
                let name = &scene.objects[k].name;
                let child_path = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
                stack.push((k, Some(me), child_path));
            }
        }
    }
    (Rig::new(names, parents, rest, scalar, false), nodes)
}

/// Write the driven channels of `pose` onto the rig's objects.
pub fn apply_pose_to_objects(scene: &mut crate::scene::LoadedScene, nodes: &[usize], pose: &Pose) {
    for (n, &i) in nodes.iter().enumerate() {
        let Some(&ch) = pose.touched.get(n) else {
            break;
        };
        if ch == 0 || i >= scene.objects.len() {
            continue;
        }
        let o = &mut scene.objects[i];
        let v = pose.trs[n];
        if ch & CH_T != 0 {
            o.translation = v.t;
        }
        if ch & CH_R != 0 {
            o.rotation = v.r;
        }
        if ch & CH_S != 0 {
            o.scale = v.s;
        }
        if ch & CH_F != 0
            && let Some(l) = o.components.iter_mut().find_map(|c| c.as_any_mut().downcast_mut::<zestengine_core::LightComponent>())
        {
            l.intensity = pose.scalar[n];
        }
    }
}

/// Advance an Animator through whichever path applies.
pub fn advance(
    animator: &mut Animator,
    ctrl: Option<&AnimatorController>,
    rig: &Rig,
    clips: &ClipSet,
    dt: f32,
) -> AnimOutput {
    match ctrl {
        Some(c) => {
            let path = animator.controller.clone();
            advance_controller(animator, c, &path, rig, clips, dt)
        }
        None => advance_simple(animator, rig, clips, dt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zestengine_core::BlendMotion;
    use zestengine_assets::{
        AnimCondition, AnimLayer, AnimParameter, AnimState, AnimTrack, AnimTransition, CondOp, KeyInterp, Keyframe,
    };

    /// Two nodes: "" (root) and "Arm" (child).
    fn rig() -> Rig {
        Rig::new(
            vec![String::new(), "Arm".into()],
            vec![None, Some(0)],
            vec![Trs::default(); 2],
            vec![0.0; 2],
            false,
        )
    }

    /// Moves `target` along X from 0 to `to` over one second.
    fn clip(target: &str, to: f32) -> AnimClip {
        AnimClip {
            length: 1.0,
            looping: true,
            tracks: vec![AnimTrack {
                target: target.into(),
                property: TrackProperty::Translation,
                keys: vec![
                    Keyframe { time: 0.0, value: [0.0; 4], interp: KeyInterp::Linear, ..Default::default() },
                    Keyframe { time: 1.0, value: [to, 0.0, 0.0, 0.0], interp: KeyInterp::Linear, ..Default::default() },
                ],
            }],
            ..Default::default()
        }
    }

    /// Moves `target` along X from 0 to `to` over `len` seconds.
    fn clip_len(target: &str, to: f32, len: f32) -> AnimClip {
        AnimClip {
            length: len,
            looping: true,
            tracks: vec![AnimTrack {
                target: target.into(),
                property: TrackProperty::Translation,
                keys: vec![
                    Keyframe { time: 0.0, value: [0.0; 4], interp: KeyInterp::Linear, ..Default::default() },
                    Keyframe { time: len, value: [to, 0.0, 0.0, 0.0], interp: KeyInterp::Linear, ..Default::default() },
                ],
            }],
            ..Default::default()
        }
    }

    fn assets(list: &[(&str, AnimClip)]) -> HashMap<String, Option<Arc<AnimClip>>> {
        list.iter().map(|(n, c)| (n.to_string(), Some(Arc::new(c.clone())))).collect()
    }

    fn state(name: &str, clip: &str) -> AnimState {
        AnimState { name: name.into(), motion: Motion::Clip(clip.into()), ..Default::default() }
    }

    fn controller(states: Vec<AnimState>, transitions: Vec<AnimTransition>, params: Vec<AnimParameter>) -> AnimatorController {
        AnimatorController {
            parameters: params,
            layers: vec![AnimLayer {
                machine: StateMachine { states, transitions, ..Default::default() },
                ..Default::default()
            }],
        }
    }

    fn animator() -> Animator {
        Animator { controller: "test.animctrl".into(), ..Default::default() }
    }

    fn step(a: &mut Animator, c: &AnimatorController, a_set: &HashMap<String, Option<Arc<AnimClip>>>, dt: f32) -> Pose {
        let set = ClipSet { imported: &[], indices: &[], assets: a_set };
        advance_controller(a, c, "test.animctrl", &rig(), &set, dt).pose
    }

    fn x(p: &Pose, node: usize) -> f32 {
        p.trs[node].t.x
    }

    #[test]
    fn transitions_on_condition() {
        let clips = assets(&[("idle.anim", clip("", 1.0)), ("run.anim", clip("", 10.0))]);
        let c = controller(
            vec![state("Idle", "idle.anim"), state("Run", "run.anim")],
            vec![AnimTransition {
                from: Some("Idle".into()),
                to: "Run".into(),
                conditions: vec![AnimCondition { parameter: "Speed".into(), op: CondOp::Greater, threshold: 0.5 }],
                duration: 0.0,
                ..Default::default()
            }],
            vec![AnimParameter { name: "Speed".into(), ..Default::default() }],
        );
        let mut a = animator();
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.current_state(0), Some("Idle"));
        a.set_float("Speed", 1.0);
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.current_state(0), Some("Run"));
        assert_eq!(a.state.state_changes.last().map(|s| s.2.as_str()), Some("Run"));
        let p = step(&mut a, &c, &clips, 0.5);
        assert!((x(&p, 0) - 5.0).abs() < 1e-3, "{}", x(&p, 0));
    }

    #[test]
    fn trigger_is_consumed() {
        let clips = assets(&[("a.anim", clip("", 1.0))]);
        let c = controller(
            vec![state("A", "a.anim"), state("B", "a.anim")],
            vec![
                AnimTransition {
                    from: Some("A".into()),
                    to: "B".into(),
                    conditions: vec![AnimCondition { parameter: "Go".into(), op: CondOp::If, threshold: 0.0 }],
                    duration: 0.0,
                    ..Default::default()
                },
                AnimTransition {
                    from: Some("B".into()),
                    to: "A".into(),
                    conditions: vec![AnimCondition { parameter: "Go".into(), op: CondOp::If, threshold: 0.0 }],
                    duration: 0.0,
                    ..Default::default()
                },
            ],
            vec![AnimParameter { name: "Go".into(), kind: AnimParamKind::Trigger, default: 0.0 }],
        );
        let mut a = animator();
        step(&mut a, &c, &clips, 0.1);
        a.set_trigger("Go");
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.current_state(0), Some("B"));
        assert!(!a.get_bool("Go"), "trigger consumed");
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.current_state(0), Some("B"), "no second transition without a new trigger");
    }

    #[test]
    fn exit_time_transition() {
        let clips = assets(&[("a.anim", clip("", 1.0))]);
        let c = controller(
            vec![AnimState { looping: false, ..state("Attack", "a.anim") }, state("Idle", "a.anim")],
            vec![AnimTransition {
                from: Some("Attack".into()),
                to: "Idle".into(),
                has_exit_time: true,
                exit_time: 0.75,
                duration: 0.0,
                ..Default::default()
            }],
            vec![],
        );
        let mut a = animator();
        step(&mut a, &c, &clips, 0.5);
        assert_eq!(a.current_state(0), Some("Attack"));
        step(&mut a, &c, &clips, 0.3);
        assert_eq!(a.current_state(0), Some("Idle"));
    }

    #[test]
    fn looping_exit_time_fires_at_the_crossing() {
        let clips = assets(&[("a.anim", clip("", 1.0))]);
        let c = controller(
            vec![state("Loop", "a.anim"), state("Next", "a.anim")],
            vec![AnimTransition {
                from: Some("Loop".into()),
                to: "Next".into(),
                has_exit_time: true,
                exit_time: 0.5,
                conditions: vec![AnimCondition { parameter: "Ok".into(), op: CondOp::If, threshold: 0.0 }],
                duration: 0.0,
                ..Default::default()
            }],
            vec![AnimParameter { name: "Ok".into(), kind: AnimParamKind::Bool, default: 0.0 }],
        );
        let mut a = animator();
        step(&mut a, &c, &clips, 0.6);
        a.set_bool("Ok", true);
        // Past the 0.5 crossing of this loop: waits for the next one.
        step(&mut a, &c, &clips, 0.2);
        assert_eq!(a.current_state(0), Some("Loop"));
        step(&mut a, &c, &clips, 0.8);
        assert_eq!(a.current_state(0), Some("Next"));
    }

    #[test]
    fn crossfade_weight_progresses() {
        let clips = assets(&[("zero.anim", clip("", 0.0)), ("ten.anim", AnimClip {
            tracks: vec![AnimTrack {
                target: String::new(),
                property: TrackProperty::Translation,
                keys: vec![Keyframe { time: 0.0, value: [10.0, 0.0, 0.0, 0.0], interp: KeyInterp::Step, ..Default::default() }],
            }],
            ..Default::default()
        })]);
        let c = controller(
            vec![state("A", "zero.anim"), state("B", "ten.anim")],
            vec![AnimTransition {
                from: Some("A".into()),
                to: "B".into(),
                conditions: vec![AnimCondition { parameter: "Go".into(), op: CondOp::If, threshold: 0.0 }],
                duration: 1.0,
                ..Default::default()
            }],
            vec![AnimParameter { name: "Go".into(), kind: AnimParamKind::Bool, default: 0.0 }],
        );
        let mut a = animator();
        step(&mut a, &c, &clips, 0.0);
        a.set_bool("Go", true);
        let p = step(&mut a, &c, &clips, 0.0);
        assert!(a.is_in_transition(0));
        assert!(x(&p, 0).abs() < 1e-3);
        let p = step(&mut a, &c, &clips, 0.25);
        assert!((x(&p, 0) - 2.5).abs() < 1e-3, "{}", x(&p, 0));
        let p = step(&mut a, &c, &clips, 0.5);
        assert!((x(&p, 0) - 7.5).abs() < 1e-3, "{}", x(&p, 0));
        let p = step(&mut a, &c, &clips, 0.5);
        assert!(!a.is_in_transition(0));
        assert!((x(&p, 0) - 10.0).abs() < 1e-3);
    }

    #[test]
    fn blend_tree_clip_time_does_not_rewind_on_weight_change() {
        // Idle and walk with different lengths: the old weight-averaged phase
        // scaled the dominant clip's local time, so the walk loop jumped
        // backwards as the blend moved between the two (visible as a restart).
        let clips = assets(&[
            ("idle.anim", clip_len("", 20.0, 2.0)),
            ("walk.anim", clip_len("", 10.0, 1.0)),
        ]);
        let tree = BlendTree {
            name: "Locomotion".into(),
            parameter: "Forward".into(),
            parameter_y: String::new(),
            motions: vec![
                BlendMotion { clip: "idle.anim".into(), position: [0.0, 0.0] },
                BlendMotion { clip: "walk.anim".into(), position: [1.0, 0.0] },
            ],
        };
        let motion = Motion::BlendTree(tree);
        let mut params = HashMap::new();
        params.insert("Forward".into(), 0.9);
        let set = ClipSet { imported: &[], indices: &[], assets: &clips };
        let (name, _clip, t) = dominant_clip(&motion, &params, &set, 1.0).expect("walk dominates at Forward 0.9");
        assert_eq!(name, "walk.anim");
        assert!((t - 1.0).abs() < 1e-5, "walk local time must be the raw state time, got {t}");
    }

    #[test]
    fn any_state_has_priority() {
        let clips = assets(&[("a.anim", clip("", 1.0))]);
        let cond = |p: &str| vec![AnimCondition { parameter: p.into(), op: CondOp::If, threshold: 0.0 }];
        let c = controller(
            vec![state("Idle", "a.anim"), state("Walk", "a.anim"), state("Die", "a.anim")],
            vec![
                AnimTransition { from: Some("Idle".into()), to: "Walk".into(), conditions: cond("Move"), duration: 0.0, ..Default::default() },
                AnimTransition { from: None, to: "Die".into(), conditions: cond("Dead"), duration: 0.0, ..Default::default() },
            ],
            vec![
                AnimParameter { name: "Move".into(), kind: AnimParamKind::Bool, default: 0.0 },
                AnimParameter { name: "Dead".into(), kind: AnimParamKind::Bool, default: 0.0 },
            ],
        );
        let mut a = animator();
        step(&mut a, &c, &clips, 0.1);
        a.set_bool("Move", true);
        a.set_bool("Dead", true);
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.current_state(0), Some("Die"));
        // Any State does not re-enter the state it targets.
        let changes = a.state.state_changes.len();
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.state.state_changes.len(), changes);
    }

    #[test]
    fn layers_override_and_additive_with_mask() {
        let r = rig();
        let base = {
            let mut p = Pose::rest(&r);
            p.trs[0].t.x = 1.0;
            p.trs[1].t.x = 1.0;
            p.touched = vec![CH_T; 2];
            p
        };
        let layer = {
            let mut p = Pose::rest(&r);
            p.trs[0].t.x = 3.0;
            p.trs[1].t.x = 3.0;
            p.touched = vec![CH_T; 2];
            p
        };
        let o = compose_layer(&r, base.clone(), &layer, 0.5, LayerBlend::Override, &[]);
        assert!((x(&o, 0) - 2.0).abs() < 1e-5 && (x(&o, 1) - 2.0).abs() < 1e-5);
        let a = compose_layer(&r, base.clone(), &layer, 1.0, LayerBlend::Additive, &[]);
        assert!((x(&a, 0) - 4.0).abs() < 1e-5);
        let m = compose_layer(&r, base, &layer, 1.0, LayerBlend::Override, &["Arm".to_string()]);
        assert!((x(&m, 0) - 1.0).abs() < 1e-5, "root masked out");
        assert!((x(&m, 1) - 3.0).abs() < 1e-5);
    }

    #[test]
    fn controller_layers_compose() {
        let clips = assets(&[("root.anim", clip("", 2.0)), ("arm.anim", clip("Arm", 4.0))]);
        let mut c = controller(vec![state("Base", "root.anim")], vec![], vec![]);
        c.layers.push(AnimLayer {
            name: "Arm".into(),
            weight: 0.5,
            blend: LayerBlend::Override,
            mask: vec![],
            machine: StateMachine { states: vec![state("Wave", "arm.anim")], ..Default::default() },
        });
        let mut a = animator();
        let p = step(&mut a, &c, &clips, 0.5);
        assert!((x(&p, 0) - 1.0).abs() < 1e-3);
        assert!((x(&p, 1) - 1.0).abs() < 1e-3, "{}", x(&p, 1));
        assert_eq!(p.touched[1] & CH_T, CH_T);
    }

    #[test]
    fn play_state_forces_and_fires_state_events() {
        let clips = assets(&[("a.anim", clip("", 1.0))]);
        let c = controller(
            vec![state("A", "a.anim"), AnimState { on_enter: "Entered".into(), ..state("B", "a.anim") }],
            vec![],
            vec![],
        );
        let mut a = animator();
        step(&mut a, &c, &clips, 0.1);
        a.play_state("B", 0.0);
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.current_state(0), Some("B"));
        assert_eq!(a.state.fired, vec!["Entered".to_string()]);
    }

    #[test]
    fn simple_path_plays_anim_assets() {
        let clips = assets(&[("wave.anim", clip("Arm", 2.0))]);
        let set = ClipSet { imported: &[], indices: &[], assets: &clips };
        let mut a = Animator { clip: "wave.anim".into(), ..Default::default() };
        let p = advance_simple(&mut a, &rig(), &set, 0.25).pose;
        assert!((x(&p, 1) - 0.5).abs() < 1e-4);
        assert_eq!(p.touched[0], 0, "root untouched");
    }

    #[test]
    fn sub_state_machine_plays_nested_states() {
        let clips = assets(&[("idle.anim", clip("", 1.0)), ("walk.anim", clip("", 4.0)), ("run.anim", clip("", 10.0))]);
        let cond = |p: &str, op, threshold| AnimCondition { parameter: p.into(), op, threshold };
        let go = |from: Option<&str>, to: &str, c: AnimCondition| AnimTransition {
            from: from.map(Into::into),
            to: to.into(),
            conditions: vec![c],
            duration: 0.0,
            ..Default::default()
        };
        let locomotion = StateMachine {
            states: vec![state("Walk", "walk.anim"), state("Run", "run.anim")],
            default_state: "Walk".into(),
            transitions: vec![go(Some("Walk"), "Run", cond("Speed", CondOp::Greater, 2.0))],
            ..Default::default()
        };
        let c = controller(
            vec![state("Idle", "idle.anim"), AnimState { name: "Locomotion".into(), sub_machine: Some(Box::new(locomotion)), ..Default::default() }],
            vec![
                go(Some("Idle"), "Locomotion", cond("Speed", CondOp::Greater, 0.5)),
                go(Some("Locomotion"), "Idle", cond("Speed", CondOp::Less, 0.5)),
            ],
            vec![AnimParameter { name: "Speed".into(), ..Default::default() }],
        );
        let mut a = animator();
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.current_state(0), Some("Idle"));
        // Entering the sub-machine enters its default state.
        a.set_float("Speed", 1.0);
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.current_state(0), Some("Locomotion/Walk"));
        let p = step(&mut a, &c, &clips, 0.25);
        assert!((x(&p, 0) - 1.0).abs() < 1e-3, "walk clip plays: {}", x(&p, 0));
        // An inner transition.
        a.set_float("Speed", 3.0);
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.current_state(0), Some("Locomotion/Run"));
        // Leaving from an inner state through the sub-machine's own transition.
        a.set_float("Speed", 0.0);
        step(&mut a, &c, &clips, 0.1);
        assert_eq!(a.current_state(0), Some("Idle"));
        assert_eq!(a.state.state_changes.last().map(|s| s.1.as_str()), Some("Locomotion/Run"));
        // Scripts force nested states by path.
        a.set_float("Speed", 1.0);
        a.play_state("Locomotion/Run", 0.0);
        step(&mut a, &c, &clips, 0.0);
        assert_eq!(a.current_state(0), Some("Locomotion/Run"));
    }
}
