//! Runtime physics (rapier3d), driven by [`crate::play::PlaySession`].
//!
//! Bodies are keyed by `ObjectId`, so spawning / destroying / disabling objects
//! mid-play just marks the world dirty and [`PhysicsWorld::sync_structure`]
//! adds or removes the affected bodies. The simulation runs at a fixed
//! [`FIXED_DT`]; dynamic bodies are interpolated between the last two steps when
//! written back, so motion stays smooth at any frame rate.
//!
//! Per object: every collider component becomes a collider on one body. The
//! body kind comes from `RigidBody` (no `RigidBody` = fixed). A
//! `CharacterController` makes a kinematic capsule moved by `move_character`.
//! Kinematic and fixed bodies follow their scene transform; dynamic bodies own
//! theirs, unless gameplay writes a new transform (treated as a teleport).
//!
//! rapier3d 0.33 here uses a glam math backend of a different glam version, so
//! vectors and rotations cross the boundary component-wise.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, channel};

use glam::{Mat4, Quat, Vec3};
use rapier3d::control::{CharacterAutostep, CharacterLength, KinematicCharacterController};
use rapier3d::math::{Pose, Rotation, Vector};
use rapier3d::prelude::*;

use citrus_core::{
    BodyKind, BoxCollider, CharacterController, JointKind, MeshCollider, ObjectId, PhysicsJoint, RayFilter,
    RayHit, RigidBody as RbComponent, SphereCollider,
};

use crate::scene::LoadedScene;

/// Fixed simulation step (seconds).
pub const FIXED_DT: f32 = 1.0 / 60.0;

/// Joint velocity-motor gain (rapier's acceleration-based "factor"): high so
/// motors reach their target speed in a few steps; `motor_max_force` caps it.
const MOTOR_GAIN: f32 = 50.0;

/// A collision / trigger contact change between two objects.
#[derive(Clone, Copy, Debug)]
pub struct ContactEvent {
    pub a: ObjectId,
    pub b: ObjectId,
    pub started: bool,
    pub trigger: bool,
}

struct Body {
    handle: RigidBodyHandle,
    kind: BodyKind,
    character: Option<KinematicCharacterController>,
    capsule: Option<ColliderHandle>,
    /// World pose after the previous / latest step (dynamic interpolation).
    prev: (Vec3, Quat),
    curr: (Vec3, Quat),
    /// Local translation/rotation last written into (or read from) the scene;
    /// a mismatch means gameplay moved the object.
    written: (Vec3, Quat),
    grounded: bool,
}

pub struct PhysicsWorld {
    pipeline: PhysicsPipeline,
    gravity: Vector,
    params: IntegrationParameters,
    islands: IslandManager,
    broad_phase: DefaultBroadPhase,
    narrow_phase: NarrowPhase,
    rigid_bodies: RigidBodySet,
    colliders: ColliderSet,
    impulse_joints: ImpulseJointSet,
    multibody_joints: MultibodyJointSet,
    ccd: CCDSolver,
    bodies: HashMap<ObjectId, Body>,
    /// Kept past collider removal so the final "stopped" event still resolves.
    collider_owner: HashMap<ColliderHandle, ObjectId>,
    collector: ChannelEventCollector,
    collision_rx: Receiver<CollisionEvent>,
    _force_rx: Receiver<ContactForceEvent>,
    pending: Vec<ContactEvent>,
    dirty: bool,
    /// Joint per object carrying a `Joint` component.
    joints: HashMap<ObjectId, (ImpulseJointHandle, JointKind)>,
    /// Static body at the origin that world-anchored joints attach to.
    world_anchor: RigidBodyHandle,
}

fn rv(v: Vec3) -> Vector {
    Vector::new(v.x, v.y, v.z)
}

fn gv(v: Vector) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

fn rq(q: Quat) -> Rotation {
    Rotation::from_xyzw(q.x, q.y, q.z, q.w)
}

fn gq(r: &Rotation) -> Quat {
    Quat::from_xyzw(r.x, r.y, r.z, r.w)
}

fn pose(t: Vec3, r: Quat) -> Pose {
    Pose::from_parts(rv(t), rq(r))
}

impl PhysicsWorld {
    /// Build a world from the scene's active collider / rigid-body objects.
    pub fn build(scene: &LoadedScene) -> Self {
        let (collision_tx, collision_rx) = channel();
        let (force_tx, force_rx) = channel();
        let mut rigid_bodies = RigidBodySet::new();
        let world_anchor = rigid_bodies.insert(RigidBodyBuilder::fixed().build());
        let mut world = Self {
            pipeline: PhysicsPipeline::new(),
            gravity: Vector::new(0.0, -9.81, 0.0),
            params: IntegrationParameters {
                dt: FIXED_DT,
                ..Default::default()
            },
            islands: IslandManager::new(),
            broad_phase: DefaultBroadPhase::new(),
            narrow_phase: NarrowPhase::new(),
            rigid_bodies,
            colliders: ColliderSet::new(),
            impulse_joints: ImpulseJointSet::new(),
            multibody_joints: MultibodyJointSet::new(),
            ccd: CCDSolver::new(),
            bodies: HashMap::new(),
            collider_owner: HashMap::new(),
            collector: ChannelEventCollector::new(collision_tx, force_tx),
            collision_rx,
            _force_rx: force_rx,
            pending: Vec::new(),
            dirty: true,
            joints: HashMap::new(),
            world_anchor,
        };
        world.sync_structure(scene);
        world
    }

    /// Objects were spawned, destroyed or toggled; rebuild membership next sync.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Add bodies for newly active physics objects and drop bodies whose object
    /// is gone or disabled. Cheap no-op unless marked dirty.
    pub fn sync_structure(&mut self, scene: &LoadedScene) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let world = scene.world_transforms();
        let mut wanted = HashMap::new();
        for (i, object) in scene.objects.iter().enumerate() {
            if is_physics_object(scene, i) && scene.is_active(i) {
                wanted.insert(object.id, i);
            }
        }
        let stale: Vec<ObjectId> = self
            .bodies
            .keys()
            .filter(|id| !wanted.contains_key(id))
            .copied()
            .collect();
        for id in stale {
            if let Some(body) = self.bodies.remove(&id) {
                self.rigid_bodies.remove(
                    body.handle,
                    &mut self.islands,
                    &mut self.colliders,
                    &mut self.impulse_joints,
                    &mut self.multibody_joints,
                    true,
                );
            }
        }
        for (&id, &i) in &wanted {
            if !self.bodies.contains_key(&id) {
                self.add_body(scene, i, world[i]);
            }
        }
        // Joints: drop those whose bodies went away (rapier removed them with
        // the body), then create missing ones once both ends exist.
        let impulse_joints = &self.impulse_joints;
        self.joints
            .retain(|id, (h, _)| wanted.contains_key(id) && impulse_joints.get(*h).is_some());
        for (&id, &i) in &wanted {
            if self.joints.contains_key(&id) {
                continue;
            }
            if let Some(j) = scene.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<PhysicsJoint>())
            {
                self.add_joint(id, j, world[i]);
            }
        }
    }

    fn add_joint(&mut self, id: ObjectId, j: &PhysicsJoint, world: Mat4) {
        let Some(b1) = self.bodies.get(&id).map(|b| b.handle) else {
            return;
        };
        let b2 = match j.connected.id() {
            Some(other) => match self.bodies.get(&other) {
                Some(b) => b.handle,
                None => return,
            },
            None => self.world_anchor,
        };
        let (Some(rb1), Some(rb2)) = (self.rigid_bodies.get(b1), self.rigid_bodies.get(b2)) else {
            return;
        };
        let (t1, r1) = (gv(rb1.translation()), gq(rb1.rotation()));
        let (t2, r2) = (gv(rb2.translation()), gq(rb2.rotation()));
        let scale = world.to_scale_rotation_translation().0;
        let a1 = Vec3::from(j.anchor) * scale;
        let anchor_world = t1 + r1 * a1;
        let a2 = r2.inverse() * (anchor_world - t2);
        let axis1 = Vec3::from(j.axis).normalize_or(Vec3::Y);
        let axis2 = r2.inverse() * (r1 * axis1);
        // Springs / ropes to another body attach at its origin, so the start
        // distance is the rest length; world-anchored ones pull to the start point.
        let far2 = if j.connected.is_set() { Vec3::ZERO } else { a2 };
        let rest = if j.rest_length > 0.0 {
            j.rest_length
        } else {
            (t2 + r2 * far2).distance(anchor_world)
        };
        let lim = |deg: bool| {
            let f = |v: f32| if deg { v.to_radians() } else { v };
            [f(j.limits[0].min(j.limits[1])), f(j.limits[0].max(j.limits[1]))]
        };
        let mut data: GenericJoint = match j.kind {
            JointKind::Fixed => FixedJointBuilder::new()
                .local_frame1(Pose::from_parts(rv(a1), Rotation::IDENTITY))
                .local_frame2(Pose::from_parts(rv(a2), rq(r2.inverse() * r1)))
                .build()
                .into(),
            JointKind::Hinge => {
                let mut b = RevoluteJointBuilder::new(rv(axis1)).local_anchor1(rv(a1)).local_anchor2(rv(a2));
                if j.limits_enabled {
                    b = b.limits(lim(true));
                }
                if j.motor_enabled {
                    b = b.motor_velocity(j.motor_speed.to_radians(), MOTOR_GAIN).motor_max_force(j.motor_max_force);
                }
                let mut data = b.build().data;
                data.set_local_axis2(rv(axis2));
                data
            }
            JointKind::Ball => SphericalJointBuilder::new()
                .local_anchor1(rv(a1))
                .local_anchor2(rv(a2))
                .build()
                .into(),
            JointKind::Slider => {
                let mut b = PrismaticJointBuilder::new(rv(axis1))
                    .local_anchor1(rv(a1))
                    .local_anchor2(rv(a2))
                    .local_axis2(rv(axis2));
                if j.limits_enabled {
                    b = b.limits(lim(false));
                }
                if j.motor_enabled {
                    b = b.motor_velocity(j.motor_speed, MOTOR_GAIN).motor_max_force(j.motor_max_force);
                }
                b.build().into()
            }
            JointKind::Spring => SpringJointBuilder::new(rest, j.stiffness, j.damping)
                .local_anchor1(rv(a1))
                .local_anchor2(rv(far2))
                .build()
                .into(),
            JointKind::Rope => RopeJointBuilder::new(rest)
                .local_anchor1(rv(a1))
                .local_anchor2(rv(far2))
                .build()
                .into(),
        };
        data.set_contacts_enabled(j.collide_connected);
        let h = self.impulse_joints.insert(b1, b2, data, true);
        self.joints.insert(id, (h, j.kind));
    }

    pub fn set_joint_motor(&mut self, id: ObjectId, speed: f32, max_force: f32) {
        let Some(&(h, kind)) = self.joints.get(&id) else {
            return;
        };
        let (axis, speed) = match kind {
            JointKind::Hinge => (JointAxis::AngX, speed.to_radians()),
            JointKind::Slider => (JointAxis::LinX, speed),
            _ => return,
        };
        if let Some(joint) = self.impulse_joints.get_mut(h, true) {
            joint.data.set_motor_velocity(axis, speed, MOTOR_GAIN);
            joint.data.set_motor_max_force(axis, max_force);
        }
    }

    fn add_body(&mut self, scene: &LoadedScene, i: usize, world: Mat4) {
        let object = &scene.objects[i];
        let id = object.id;
        let (scale, rot, trans) = world.to_scale_rotation_translation();
        let character = object
            .components
            .iter()
            .find_map(|c| c.as_any().downcast_ref::<CharacterController>());
        let rb = object
            .components
            .iter()
            .find_map(|c| c.as_any().downcast_ref::<RbComponent>());
        let kind = if character.is_some() {
            BodyKind::Kinematic
        } else {
            rb.map(|r| r.kind).unwrap_or(BodyKind::Fixed)
        };
        let builder = match kind {
            BodyKind::Dynamic => RigidBodyBuilder::dynamic(),
            BodyKind::Kinematic => RigidBodyBuilder::kinematic_position_based(),
            BodyKind::Fixed => RigidBodyBuilder::fixed(),
        }
        .pose(pose(trans, rot))
        .user_data(id.raw());
        let builder = match rb {
            Some(r) if kind == BodyKind::Dynamic => {
                let b = builder.gravity_scale(r.gravity_scale).ccd_enabled(true);
                if r.mass > 0.0 { b.additional_mass(r.mass) } else { b }
            }
            _ => builder,
        };
        let handle = self.rigid_bodies.insert(builder.build());

        let layer = object.layer;
        let groups = InteractionGroups::new(
            Group::from_bits_truncate(1u32 << (layer as u32 & 31)),
            Group::from_bits_truncate(scene.layers.collision_mask(layer)),
            InteractionTestMode::And,
        );
        let (rest, fric) = rb.map(|r| (r.restitution, r.friction)).unwrap_or((0.0, 0.5));
        let finish = |b: ColliderBuilder, trigger: bool| {
            let b = b
                .restitution(rest)
                .friction(fric)
                .collision_groups(groups)
                .active_events(ActiveEvents::COLLISION_EVENTS)
                .user_data(id.raw());
            if trigger {
                b.sensor(true).active_collision_types(ActiveCollisionTypes::all())
            } else {
                b
            }
        };

        let mut controller = None;
        let mut capsule = None;
        if let Some(cc) = character {
            let s = scale.max_element().max(1e-3);
            let radius = (cc.radius * s).max(0.01);
            let half = ((cc.height * s) * 0.5 - radius).max(0.0);
            let center = Vec3::from(cc.center) * scale;
            let collider = finish(ColliderBuilder::capsule_y(half, radius).translation(rv(center)), false).build();
            let h = self
                .colliders
                .insert_with_parent(collider, handle, &mut self.rigid_bodies);
            self.collider_owner.insert(h, id);
            capsule = Some(h);
            controller = Some(KinematicCharacterController {
                autostep: (cc.step_height > 0.0).then_some(CharacterAutostep {
                    max_height: CharacterLength::Absolute(cc.step_height * s),
                    min_width: CharacterLength::Absolute(radius * 0.5),
                    include_dynamic_bodies: false,
                }),
                max_slope_climb_angle: cc.max_slope.to_radians(),
                min_slope_slide_angle: cc.max_slope.to_radians(),
                snap_to_ground: (cc.snap_to_ground > 0.0).then_some(CharacterLength::Absolute(cc.snap_to_ground)),
                offset: CharacterLength::Absolute(0.02),
                ..Default::default()
            });
        } else {
            for (shape, trigger) in collider_shapes(scene, i, scale, kind) {
                let h = self.colliders.insert_with_parent(
                    finish(shape, trigger).build(),
                    handle,
                    &mut self.rigid_bodies,
                );
                self.collider_owner.insert(h, id);
            }
        }
        self.bodies.insert(
            id,
            Body {
                handle,
                kind,
                character: controller,
                capsule,
                prev: (trans, rot),
                curr: (trans, rot),
                written: (object.translation, object.rotation),
                grounded: false,
            },
        );
    }

    /// Push gameplay-driven transforms into the simulation: kinematic bodies get
    /// their next target pose; any other body whose scene transform changed
    /// since the last write-back is teleported.
    pub fn push_poses(&mut self, scene: &LoadedScene, id_index: &HashMap<ObjectId, usize>, world: &[Mat4]) {
        for (id, body) in &mut self.bodies {
            let Some(&i) = id_index.get(id) else {
                continue;
            };
            let object = &scene.objects[i];
            let (_, r, t) = world[i].to_scale_rotation_translation();
            let Some(rb) = self.rigid_bodies.get_mut(body.handle) else {
                continue;
            };
            let moved = (object.translation, object.rotation) != body.written;
            if body.kind == BodyKind::Kinematic && body.character.is_none() {
                rb.set_next_kinematic_position(pose(t, r));
            } else if moved {
                rb.set_position(pose(t, r), true);
                if body.kind == BodyKind::Kinematic {
                    rb.set_next_kinematic_position(pose(t, r));
                }
                body.prev = (t, r);
                body.curr = (t, r);
            }
            body.written = (object.translation, object.rotation);
        }
    }

    /// Advance one fixed step and queue the resulting contact events.
    pub fn step(&mut self) {
        self.params.dt = FIXED_DT;
        self.pipeline.step(
            self.gravity,
            &self.params,
            &mut self.islands,
            &mut self.broad_phase,
            &mut self.narrow_phase,
            &mut self.rigid_bodies,
            &mut self.colliders,
            &mut self.impulse_joints,
            &mut self.multibody_joints,
            &mut self.ccd,
            &(),
            &self.collector,
        );
        for body in self.bodies.values_mut() {
            if body.kind != BodyKind::Dynamic {
                continue;
            }
            if let Some(rb) = self.rigid_bodies.get(body.handle) {
                body.prev = body.curr;
                body.curr = (gv(rb.translation()), gq(rb.rotation()));
            }
        }
        while let Ok(event) = self.collision_rx.try_recv() {
            let (h1, h2) = (event.collider1(), event.collider2());
            let (Some(&a), Some(&b)) = (self.collider_owner.get(&h1), self.collider_owner.get(&h2)) else {
                continue;
            };
            let trigger = match event {
                CollisionEvent::Started(_, _, f) | CollisionEvent::Stopped(_, _, f) => {
                    f.contains(CollisionEventFlags::SENSOR)
                }
            };
            self.pending.push(ContactEvent {
                a,
                b,
                started: event.started(),
                trigger,
            });
            if event.stopped() {
                for h in [h1, h2] {
                    if self.colliders.get(h).is_none() {
                        self.collider_owner.remove(&h);
                    }
                }
            }
        }
    }

    /// Write dynamic bodies back to the scene, interpolated `alpha` (0..1)
    /// between the last two steps.
    pub fn write_back(&mut self, scene: &mut LoadedScene, id_index: &HashMap<ObjectId, usize>, alpha: f32) {
        for (id, body) in &mut self.bodies {
            if body.kind != BodyKind::Dynamic {
                continue;
            }
            let Some(&i) = id_index.get(id) else {
                continue;
            };
            let t = body.prev.0.lerp(body.curr.0, alpha);
            let r = body.prev.1.slerp(body.curr.1, alpha);
            let local = world_to_local(scene, i, t, r);
            scene.objects[i].translation = local.0;
            scene.objects[i].rotation = local.1;
            body.written = local;
        }
    }

    pub fn take_events(&mut self) -> Vec<ContactEvent> {
        std::mem::take(&mut self.pending)
    }

    fn dynamic_body(&mut self, id: ObjectId) -> Option<&mut rapier3d::dynamics::RigidBody> {
        let body = self.bodies.get(&id)?;
        if body.kind != BodyKind::Dynamic {
            return None;
        }
        self.rigid_bodies.get_mut(body.handle)
    }

    pub fn add_force(&mut self, id: ObjectId, force: Vec3) {
        if let Some(rb) = self.dynamic_body(id) {
            rb.add_force(rv(force), true);
        }
    }

    pub fn add_impulse(&mut self, id: ObjectId, impulse: Vec3) {
        if let Some(rb) = self.dynamic_body(id) {
            rb.apply_impulse(rv(impulse), true);
        }
    }

    pub fn add_torque(&mut self, id: ObjectId, torque: Vec3) {
        if let Some(rb) = self.dynamic_body(id) {
            rb.add_torque(rv(torque), true);
        }
    }

    pub fn set_velocity(&mut self, id: ObjectId, linear: Vec3) {
        if let Some(rb) = self.dynamic_body(id) {
            rb.set_linvel(rv(linear), true);
        }
    }

    /// Clear per-step forces once all fixed steps of a frame ran (forces added
    /// in `update` apply to the next frame's steps only).
    pub fn reset_forces(&mut self) {
        for body in self.bodies.values() {
            if body.kind == BodyKind::Dynamic
                && let Some(rb) = self.rigid_bodies.get_mut(body.handle)
            {
                rb.reset_forces(false);
                rb.reset_torques(false);
            }
        }
    }

    /// Move a character controller by `delta` (world space), resolving
    /// collisions now, and write the result into the scene.
    pub fn move_character(&mut self, scene: &mut LoadedScene, index: usize, delta: Vec3) {
        let id = scene.objects[index].id;
        let Some(body) = self.bodies.get_mut(&id) else {
            return;
        };
        let (Some(controller), Some(capsule)) = (body.character, body.capsule) else {
            return;
        };
        let Some(rb) = self.rigid_bodies.get(body.handle) else {
            return;
        };
        let Some(col) = self.colliders.get(capsule) else {
            return;
        };
        let shape_pos = *col.position();
        let filter = QueryFilter::default()
            .exclude_rigid_body(body.handle)
            .exclude_sensors();
        let qp = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.rigid_bodies,
            &self.colliders,
            filter,
        );
        let movement = controller.move_shape(FIXED_DT, &qp, col.shape(), &shape_pos, rv(delta), |_| {});
        let new_t = gv(rb.translation()) + gv(movement.translation);
        let rot = gq(rb.rotation());
        body.grounded = movement.grounded;
        if let Some(rb) = self.rigid_bodies.get_mut(body.handle) {
            rb.set_position(pose(new_t, rot), true);
            rb.set_next_kinematic_position(pose(new_t, rot));
        }
        let local = world_to_local(scene, index, new_t, rot);
        scene.objects[index].translation = local.0;
        body.written = (local.0, scene.objects[index].rotation);
    }

    fn query_filter<'a>(filter: &RayFilter, predicate: &'a impl Fn(ColliderHandle, &Collider) -> bool) -> QueryFilter<'a> {
        let mut f = QueryFilter::default().predicate(predicate);
        if !filter.include_triggers {
            f = f.exclude_sensors();
        }
        f
    }
}

impl citrus_core::PhysicsQuery for PhysicsWorld {
    fn raycast(&self, origin: Vec3, dir: Vec3, max_dist: f32, filter: &RayFilter) -> Option<RayHit> {
        let dir = dir.normalize_or_zero();
        if dir == Vec3::ZERO {
            return None;
        }
        let (mask, exclude) = (filter.layer_mask, filter.exclude.map(|e| e.raw()));
        let predicate = move |_: ColliderHandle, c: &Collider| {
            c.collision_groups().memberships.bits() & mask != 0 && Some(c.user_data) != exclude
        };
        let qp = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.rigid_bodies,
            &self.colliders,
            Self::query_filter(filter, &predicate),
        );
        let ray = Ray::new(rv(origin), rv(dir));
        let (handle, hit) = qp.cast_ray_and_get_normal(&ray, max_dist, true)?;
        let collider = self.colliders.get(handle)?;
        Some(RayHit {
            object: ObjectId::from_raw(collider.user_data),
            point: origin + dir * hit.time_of_impact,
            normal: gv(hit.normal),
            distance: hit.time_of_impact,
        })
    }

    fn overlap_sphere(&self, center: Vec3, radius: f32, filter: &RayFilter) -> Vec<ObjectId> {
        let (mask, exclude) = (filter.layer_mask, filter.exclude.map(|e| e.raw()));
        let predicate = move |_: ColliderHandle, c: &Collider| {
            c.collision_groups().memberships.bits() & mask != 0 && Some(c.user_data) != exclude
        };
        let qp = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.rigid_bodies,
            &self.colliders,
            Self::query_filter(filter, &predicate),
        );
        let ball = Ball::new(radius.max(1e-4));
        let mut out: Vec<ObjectId> = Vec::new();
        for (_, c) in qp.intersect_shape(Pose::from_translation(rv(center)), &ball) {
            let id = ObjectId::from_raw(c.user_data);
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    fn velocity(&self, id: ObjectId) -> Option<Vec3> {
        let body = self.bodies.get(&id)?;
        self.rigid_bodies.get(body.handle).map(|rb| gv(rb.linvel()))
    }

    fn grounded(&self, id: ObjectId) -> bool {
        self.bodies.get(&id).is_some_and(|b| b.grounded)
    }
}

/// Convert a world pose into object `i`'s local translation/rotation.
fn world_to_local(scene: &LoadedScene, i: usize, t: Vec3, r: Quat) -> (Vec3, Quat) {
    match scene.objects[i].parent {
        Some(p) if p < scene.objects.len() => {
            let parent = scene.world_transform(p);
            let (_, pr, _) = parent.to_scale_rotation_translation();
            (parent.inverse().transform_point3(t), pr.inverse() * r)
        }
        _ => (t, r),
    }
}

fn is_physics_object(scene: &LoadedScene, i: usize) -> bool {
    scene.objects[i].components.iter().any(|c| {
        let a = c.as_any();
        a.is::<BoxCollider>() || a.is::<SphereCollider>() || a.is::<MeshCollider>() || a.is::<CharacterController>()
    })
}

/// Rapier collider shapes (with object-space offsets baked in) for every
/// collider component on object `i`, scaled by its world scale. Returns
/// `(builder, is_trigger)`.
fn collider_shapes(scene: &LoadedScene, i: usize, scale: Vec3, kind: BodyKind) -> Vec<(ColliderBuilder, bool)> {
    let obj = &scene.objects[i];
    let mut out = Vec::new();
    for c in &obj.components {
        let any = c.as_any();
        if let Some(b) = any.downcast_ref::<BoxCollider>() {
            let half = (Vec3::from(b.size) * 0.5 * scale).max(Vec3::splat(1e-3));
            let off = Vec3::from(b.center) * scale;
            out.push((ColliderBuilder::cuboid(half.x, half.y, half.z).translation(rv(off)), b.is_trigger));
        } else if let Some(s) = any.downcast_ref::<SphereCollider>() {
            let r = (s.radius * scale.max_element()).max(1e-3);
            let off = Vec3::from(s.center) * scale;
            out.push((ColliderBuilder::ball(r).translation(rv(off)), s.is_trigger));
        } else if let Some(m) = any.downcast_ref::<MeshCollider>()
            && let Some(shape) = mesh_shape(scene, i, scale, m.convex || kind != BodyKind::Fixed)
        {
            out.push((shape, m.is_trigger));
        }
    }
    out
}

/// Collider from the object's render meshes: exact triangles for static
/// geometry, convex hull for moving bodies (rapier can't simulate concave
/// dynamic meshes). Falls back to the mesh AABB when no CPU geometry exists.
fn mesh_shape(scene: &LoadedScene, i: usize, scale: Vec3, convex: bool) -> Option<ColliderBuilder> {
    let obj = &scene.objects[i];
    let mut verts: Vec<Vector> = Vec::new();
    let mut tris: Vec<[u32; 3]> = Vec::new();
    for render in obj.render_slots() {
        if let Some((pos, idx)) = scene.mesh_geometry(render.mesh) {
            let base = verts.len() as u32;
            verts.extend(pos.iter().map(|p| rv(*p * scale)));
            tris.extend(idx.as_chunks::<3>().0.iter().map(|t| [base + t[0], base + t[1], base + t[2]]));
        }
    }
    if !verts.is_empty() {
        let shape = if convex {
            ColliderBuilder::convex_hull(&verts)
        } else {
            ColliderBuilder::trimesh(verts, tris).ok()
        };
        if shape.is_some() {
            return shape;
        }
    }
    let render = obj.render?;
    let (min, max) = scene.mesh_aabb(render.mesh);
    let half = ((max - min) * 0.5 * scale).max(Vec3::splat(1e-3));
    let center = (min + max) * 0.5 * scale;
    Some(ColliderBuilder::cuboid(half.x, half.y, half.z).translation(rv(center)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::SceneObject;
    use citrus_assets::ObjectSource;
    use citrus_core::{Component, PhysicsQuery};

    fn obj(scene: &mut LoadedScene, pos: Vec3, components: Vec<Box<dyn Component>>) -> ObjectId {
        let id = ObjectId::new();
        scene.objects.push(SceneObject {
            id,
            name: String::new(),
            render: None,
            extra_render: Vec::new(),
            source: ObjectSource::Empty,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            layer: 0,
            parent: None,
            translation: pos,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            components,
        });
        id
    }

    fn floor(scene: &mut LoadedScene) -> ObjectId {
        obj(
            scene,
            Vec3::ZERO,
            vec![Box::new(BoxCollider {
                size: [20.0, 1.0, 20.0],
                ..Default::default()
            })],
        )
    }

    fn dynamic_ball(scene: &mut LoadedScene, pos: Vec3) -> ObjectId {
        obj(
            scene,
            pos,
            vec![Box::new(SphereCollider::default()), Box::new(RbComponent::default())],
        )
    }

    fn run(pw: &mut PhysicsWorld, scene: &mut LoadedScene, steps: usize) -> Vec<ContactEvent> {
        let mut events = Vec::new();
        for _ in 0..steps {
            pw.sync_structure(scene);
            let ids = scene.id_index();
            let world = scene.world_transforms();
            pw.push_poses(scene, &ids, &world);
            pw.step();
            pw.write_back(scene, &ids, 1.0);
            events.extend(pw.take_events());
        }
        events
    }

    #[test]
    fn queries_on_empty_world_are_safe() {
        let scene = LoadedScene::empty();
        let pw = PhysicsWorld::build(&scene);
        let f = RayFilter::default();
        assert!(pw.raycast(Vec3::ZERO, Vec3::NEG_Y, 100.0, &f).is_none());
        assert!(pw.overlap_sphere(Vec3::ZERO, 5.0, &f).is_empty());
        assert!(pw.raycast(Vec3::ZERO, Vec3::ZERO, 100.0, &f).is_none());
    }

    #[test]
    fn dynamic_body_lands_on_floor_and_reports_contact() {
        let mut scene = LoadedScene::empty();
        let floor_id = floor(&mut scene);
        let ball = dynamic_ball(&mut scene, Vec3::new(0.0, 3.0, 0.0));
        let mut pw = PhysicsWorld::build(&scene);
        let events = run(&mut pw, &mut scene, 180);
        let y = scene.objects[1].translation.y;
        assert!((y - 1.0).abs() < 0.05, "ball rests on the floor top (y=1), got {y}");
        assert!(events.iter().any(|e| e.started
            && !e.trigger
            && ((e.a == ball && e.b == floor_id) || (e.a == floor_id && e.b == ball))));
    }

    #[test]
    fn raycast_respects_exclude_and_layers() {
        let mut scene = LoadedScene::empty();
        let floor_id = floor(&mut scene);
        let mut pw = PhysicsWorld::build(&scene);
        run(&mut pw, &mut scene, 1);
        let from = Vec3::new(0.0, 5.0, 0.0);
        let hit = pw
            .raycast(from, Vec3::NEG_Y, 100.0, &RayFilter::default())
            .expect("hits floor");
        assert_eq!(hit.object, floor_id);
        assert!((hit.point.y - 0.5).abs() < 1e-3);
        assert!(hit.normal.y > 0.99);
        let excluded = RayFilter {
            exclude: Some(floor_id),
            ..Default::default()
        };
        assert!(pw.raycast(from, Vec3::NEG_Y, 100.0, &excluded).is_none());
        let other_layer = RayFilter {
            layer_mask: 1 << 3,
            ..Default::default()
        };
        assert!(pw.raycast(from, Vec3::NEG_Y, 100.0, &other_layer).is_none());
        assert_eq!(pw.overlap_sphere(Vec3::new(0.0, 0.8, 0.0), 0.5, &RayFilter::default()), vec![floor_id]);
    }

    #[test]
    fn trigger_reports_enter_without_blocking() {
        let mut scene = LoadedScene::empty();
        floor(&mut scene);
        let zone = obj(
            &mut scene,
            Vec3::new(0.0, 2.0, 0.0),
            vec![Box::new(BoxCollider {
                size: [2.0, 1.0, 2.0],
                is_trigger: true,
                ..Default::default()
            })],
        );
        let ball = dynamic_ball(&mut scene, Vec3::new(0.0, 4.0, 0.0));
        let mut pw = PhysicsWorld::build(&scene);
        let events = run(&mut pw, &mut scene, 180);
        assert!(events.iter().any(|e| e.trigger && e.started && (e.a == zone || e.b == zone)));
        assert!(events.iter().any(|e| e.trigger && !e.started && (e.a == ball || e.b == ball)));
        assert!(scene.objects[2].translation.y < 1.1, "fell through the trigger onto the floor");
    }

    #[test]
    fn character_controller_is_blocked_by_walls_and_grounds() {
        let mut scene = LoadedScene::empty();
        floor(&mut scene);
        obj(
            &mut scene,
            Vec3::new(3.0, 2.0, 0.0),
            vec![Box::new(BoxCollider {
                size: [1.0, 4.0, 4.0],
                ..Default::default()
            })],
        );
        let player = obj(&mut scene, Vec3::new(0.0, 0.6, 0.0), vec![Box::new(CharacterController::default())]);
        let mut pw = PhysicsWorld::build(&scene);
        run(&mut pw, &mut scene, 1);
        for _ in 0..30 {
            pw.move_character(&mut scene, 2, Vec3::new(0.2, -0.1, 0.0));
            run(&mut pw, &mut scene, 1);
        }
        let p = scene.objects[2].translation;
        assert!(p.x < 2.5 - 0.3, "stopped at the wall (face at x=2.5), got {}", p.x);
        assert!((p.y - 0.5).abs() < 0.1, "standing on the floor (top y=0.5), got {}", p.y);
        assert!(pw.grounded(player));
    }

    #[test]
    fn destroyed_and_disabled_objects_leave_the_simulation() {
        let mut scene = LoadedScene::empty();
        floor(&mut scene);
        let mut pw = PhysicsWorld::build(&scene);
        run(&mut pw, &mut scene, 1);
        let down = |pw: &PhysicsWorld| pw.raycast(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y, 100.0, &RayFilter::default());
        assert!(down(&pw).is_some());
        scene.objects[0].enabled = false;
        pw.mark_dirty();
        run(&mut pw, &mut scene, 1);
        assert!(down(&pw).is_none());
        scene.objects[0].enabled = true;
        pw.mark_dirty();
        run(&mut pw, &mut scene, 1);
        assert!(down(&pw).is_some());
        scene.remove_object(0);
        pw.mark_dirty();
        run(&mut pw, &mut scene, 1);
        assert!(down(&pw).is_none());
    }

    fn jointed(scene: &mut LoadedScene, pos: Vec3, joint: PhysicsJoint) -> ObjectId {
        obj(
            scene,
            pos,
            vec![
                Box::new(BoxCollider { size: [0.5, 0.5, 0.5], ..Default::default() }),
                Box::new(RbComponent::default()),
                Box::new(joint),
            ],
        )
    }

    #[test]
    fn fixed_joint_to_world_holds_body() {
        let mut scene = LoadedScene::empty();
        jointed(&mut scene, Vec3::new(0.0, 5.0, 0.0), PhysicsJoint { kind: JointKind::Fixed, ..Default::default() });
        let mut pw = PhysicsWorld::build(&scene);
        run(&mut pw, &mut scene, 120);
        assert!((scene.objects[0].translation.y - 5.0).abs() < 0.05, "{}", scene.objects[0].translation);
    }

    #[test]
    fn hinge_and_rope_constrain_distance() {
        let mut scene = LoadedScene::empty();
        // Pendulum: hinge anchor 1 m above the body, axis Z.
        jointed(
            &mut scene,
            Vec3::new(0.0, 5.0, 0.0),
            PhysicsJoint {
                kind: JointKind::Hinge,
                anchor: [1.0, 1.0, 0.0],
                axis: [0.0, 0.0, 1.0],
                ..Default::default()
            },
        );
        // Rope from the world start point, 2 m long, body starts 1 m below it.
        jointed(
            &mut scene,
            Vec3::new(10.0, 5.0, 0.0),
            PhysicsJoint {
                kind: JointKind::Rope,
                anchor: [0.0, 1.0, 0.0],
                rest_length: 2.0,
                ..Default::default()
            },
        );
        let mut pw = PhysicsWorld::build(&scene);
        let pivot = Vec3::new(1.0, 6.0, 0.0);
        let rope_top = Vec3::new(10.0, 6.0, 0.0);
        let start = scene.objects[0].translation.distance(pivot);
        for _ in 0..180 {
            run(&mut pw, &mut scene, 1);
            let d = scene.objects[0].translation.distance(pivot);
            assert!((d - start).abs() < 0.05, "hinge distance drifted to {d}");
            assert!(scene.objects[1].translation.distance(rope_top) < 3.0 + 0.05, "rope stretched");
        }
        assert!(scene.objects[0].translation.y < 5.0, "pendulum swung down");
    }

    #[test]
    fn hinge_motor_spins_body() {
        let mut scene = LoadedScene::empty();
        let id = obj(
            &mut scene,
            Vec3::new(0.0, 5.0, 0.0),
            vec![
                Box::new(BoxCollider::default()),
                Box::new(RbComponent { gravity_scale: 0.0, ..Default::default() }),
                Box::new(PhysicsJoint { kind: JointKind::Hinge, ..Default::default() }),
            ],
        );
        let mut pw = PhysicsWorld::build(&scene);
        pw.set_joint_motor(id, 180.0, 1000.0);
        run(&mut pw, &mut scene, 30);
        let yaw = scene.objects[0].rotation.to_euler(glam::EulerRot::YXZ).0.to_degrees().abs();
        assert!(yaw > 30.0, "wheel turned {yaw} deg");
    }

    #[test]
    fn gameplay_teleport_moves_dynamic_body() {
        let mut scene = LoadedScene::empty();
        floor(&mut scene);
        dynamic_ball(&mut scene, Vec3::new(0.0, 3.0, 0.0));
        let mut pw = PhysicsWorld::build(&scene);
        run(&mut pw, &mut scene, 60);
        scene.objects[1].translation = Vec3::new(5.0, 3.0, 0.0);
        run(&mut pw, &mut scene, 1);
        assert!((scene.objects[1].translation.x - 5.0).abs() < 1e-3);
    }
}
