//! Runtime physics (rapier3d): build a simulation from the scene's colliders +
//! rigid bodies when Play starts, step it under gravity each frame, and write
//! the simulated transforms back onto dynamic/kinematic objects.
//!
//! Scope: one collider per object (Box -> cuboid, Sphere ->
//! ball, Mesh -> its AABB as a cuboid). Objects with a `RigidBody` component use
//! its body kind; a collider with no `RigidBody` becomes a fixed (static)
//! collider, so level geometry catches falling dynamics. Transforms are written
//! back in world space (correct for unparented bodies).
//!
//! rapier3d 0.33 here uses a glam math backend, so `Vector`/rotations are glam
//! vectors and bodies report glam Vec3/Quat.

use glam::{Quat, Vec3};
use rapier3d::math::Vector;
use rapier3d::prelude::*;

use citrus_core::{BodyKind, BoxCollider, MeshCollider, RigidBody as RbComponent, SphereCollider};

use crate::scene::LoadedScene;

pub struct PhysicsWorld {
    pipeline: PhysicsPipeline,
    gravity: Vector,
    params: IntegrationParameters,
    islands: IslandManager,
    broad_phase: DefaultBroadPhase,
    narrow_phase: NarrowPhase,
    bodies: RigidBodySet,
    colliders: ColliderSet,
    impulse_joints: ImpulseJointSet,
    multibody_joints: MultibodyJointSet,
    ccd: CCDSolver,
    /// Bodies whose simulated transform is written back (dynamic + kinematic).
    sync: Vec<(usize, RigidBodyHandle)>,
}

impl PhysicsWorld {
    /// Build a world from the scene's collider/rigid-body objects.
    pub fn build(scene: &LoadedScene) -> Self {
        let mut bodies = RigidBodySet::new();
        let mut colliders = ColliderSet::new();
        let mut sync = Vec::new();

        for i in 0..scene.objects.len() {
            let Some((shape, offset)) = collider_shape(scene, i) else {
                continue;
            };
            let rb = scene.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<RbComponent>());
            let kind = rb.map(|r| r.kind).unwrap_or(BodyKind::Fixed);

            let world = scene.world_transform(i);
            let (_scale, rot, trans) = world.to_scale_rotation_translation();
            let (axis, angle) = rot.to_axis_angle();
            let rotvec = axis * angle; // rapier takes a scaled-axis rotation vector

            let builder = match kind {
                BodyKind::Dynamic => RigidBodyBuilder::dynamic(),
                BodyKind::Kinematic => RigidBodyBuilder::kinematic_position_based(),
                BodyKind::Fixed => RigidBodyBuilder::fixed(),
            }
            .translation(Vector::new(trans.x, trans.y, trans.z))
            .rotation(Vector::new(rotvec.x, rotvec.y, rotvec.z));
            let builder = match rb {
                Some(r) if kind == BodyKind::Dynamic => {
                    let b = builder.gravity_scale(r.gravity_scale);
                    if r.mass > 0.0 { b.additional_mass(r.mass) } else { b }
                }
                _ => builder,
            };
            let handle = bodies.insert(builder.build());

            let (rest, fric) = rb.map(|r| (r.restitution, r.friction)).unwrap_or((0.0, 0.5));
            // Layer-collision matrix (Unity-style): membership = this object's
            // layer bit, filter = the layers it's allowed to collide with. Two
            // colliders interact only if each is in the other's filter, so a
            // symmetric matrix gives "layer A ignores layer B" both ways.
            let layer = scene.objects[i].layer;
            let groups = InteractionGroups::new(
                Group::from_bits_truncate(1u32 << (layer as u32 & 31)),
                Group::from_bits_truncate(scene.layers.collision_mask(layer)),
                InteractionTestMode::And,
            );
            let collider = shape
                .translation(offset)
                .restitution(rest)
                .friction(fric)
                .collision_groups(groups)
                .build();
            colliders.insert_with_parent(collider, handle, &mut bodies);

            if kind != BodyKind::Fixed {
                sync.push((i, handle));
            }
        }

        Self {
            pipeline: PhysicsPipeline::new(),
            gravity: Vector::new(0.0, -9.81, 0.0),
            params: IntegrationParameters::default(),
            islands: IslandManager::new(),
            broad_phase: DefaultBroadPhase::new(),
            narrow_phase: NarrowPhase::new(),
            bodies,
            colliders,
            impulse_joints: ImpulseJointSet::new(),
            multibody_joints: MultibodyJointSet::new(),
            ccd: CCDSolver::new(),
            sync,
        }
    }

    /// Any dynamic/kinematic bodies to simulate? (Skip stepping if not.)
    pub fn is_empty(&self) -> bool {
        self.sync.is_empty()
    }

    /// Advance the simulation by `dt` seconds (clamped to a sane range).
    pub fn step(&mut self, dt: f32) {
        self.params.dt = dt.clamp(1.0 / 240.0, 1.0 / 15.0);
        self.pipeline.step(
            self.gravity,
            &self.params,
            &mut self.islands,
            &mut self.broad_phase,
            &mut self.narrow_phase,
            &mut self.bodies,
            &mut self.colliders,
            &mut self.impulse_joints,
            &mut self.multibody_joints,
            &mut self.ccd,
            &(),
            &(),
        );
    }

    /// Write simulated transforms back onto the scene's dynamic/kinematic
    /// objects (world space; correct for unparented bodies).
    pub fn sync_back(&self, scene: &mut LoadedScene) {
        for &(i, handle) in &self.sync {
            let Some(body) = self.bodies.get(handle) else {
                continue;
            };
            if i >= scene.objects.len() {
                continue;
            }
            let t = body.translation();
            let r = body.rotation();
            scene.objects[i].translation = Vec3::new(t.x, t.y, t.z);
            scene.objects[i].rotation = Quat::from_xyzw(r.x, r.y, r.z, r.w);
        }
    }
}

/// Build a rapier collider shape (+ its object-space offset) for the first
/// collider component on object `i`, scaled by the object's world scale.
fn collider_shape(scene: &LoadedScene, i: usize) -> Option<(ColliderBuilder, Vector)> {
    let scale = scene.world_transform(i).to_scale_rotation_translation().0;
    let obj = &scene.objects[i];
    for c in &obj.components {
        let any = c.as_any();
        if let Some(b) = any.downcast_ref::<BoxCollider>() {
            let hx = (b.size[0] * 0.5 * scale.x).max(1e-3);
            let hy = (b.size[1] * 0.5 * scale.y).max(1e-3);
            let hz = (b.size[2] * 0.5 * scale.z).max(1e-3);
            let off = Vector::new(
                b.center[0] * scale.x,
                b.center[1] * scale.y,
                b.center[2] * scale.z,
            );
            return Some((ColliderBuilder::cuboid(hx, hy, hz), off));
        }
        if let Some(s) = any.downcast_ref::<SphereCollider>() {
            let r = (s.radius * scale.max_element()).max(1e-3);
            let off = Vector::new(
                s.center[0] * scale.x,
                s.center[1] * scale.y,
                s.center[2] * scale.z,
            );
            return Some((ColliderBuilder::ball(r), off));
        }
        if any.downcast_ref::<MeshCollider>().is_some()
            && let Some(render) = obj.render
        {
            // Approximate the mesh with its AABB as a cuboid.
            let (min, max) = scene.mesh_aabb(render.mesh);
            let half = (max - min) * 0.5 * scale;
            let center = (min + max) * 0.5 * scale;
            return Some((
                ColliderBuilder::cuboid(half.x.max(1e-3), half.y.max(1e-3), half.z.max(1e-3)),
                Vector::new(center.x, center.y, center.z),
            ));
        }
    }
    None
}
