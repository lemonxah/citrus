//! Walls the GPU-simulated water reflects off: every static object (its
//! colliders, or its render mesh when it has none) and every still collider
//! through the surface, in the editor and in Play alike. Moving bodies are
//! not walls: they push the water as displacers instead, and a collider
//! being moved counts as moving until it has stood still a moment. Only
//! objects whose layer collides with the water's count (the Layers matrix).
//!
//! Closed shapes mark the cells whose centres at the surface lie inside them;
//! triangle meshes (which need not be closed: a pool's single-sided walls)
//! mark the cells along the line where they cross the surface. Shapes are
//! cached per object until it moves or its colliders change, and a water's
//! cells are only rebuilt when its window or the walls in it change.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use citrus_core::{
    BodyKind, BoxCollider, CapsuleCollider, CharacterController, MeshCollider, ObjectId, RigidBody, SimWindow, SphereCollider, Water,
};
use glam::{Mat4, Vec2, Vec3};

use crate::physics::{Solid, solid_contains, world_solids};
use crate::scene::LoadedScene;

/// Updates a collider counts as moving for after it last moved.
const SETTLE_UPDATES: u32 = 10;

struct Cached {
    key: u64,
    solids: Arc<Vec<Solid>>,
    /// World bounds of the solids.
    lo: Vec3,
    hi: Vec3,
}

#[derive(Default)]
pub struct WaterWalls {
    shapes: HashMap<ObjectId, Cached>,
    /// Each object's last world transform and updates left until it counts
    /// as still.
    moved: HashMap<ObjectId, ([f32; 16], u32)>,
    /// Each water's last wall set.
    masks: HashMap<ObjectId, u64>,
}

impl WaterWalls {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Refresh the wall cells of every GPU-simulated water that has a
    /// window from what stands in it now.
    pub fn update(&mut self, scene: &mut LoadedScene, world: &[Mat4]) {
        let active = scene.active_flags();
        let waters: Vec<usize> = (0..scene.objects.len())
            .filter(|&i| active[i])
            .filter(|&i| water_of(scene, i).is_some_and(|w| w.interactive && w.sim_feed.window.is_some() && w.ripples.as_ref().is_some_and(|r| r.gpu)))
            .collect();
        if waters.is_empty() {
            return;
        }
        let mut walls = Vec::new();
        let mut seen = HashMap::new();
        for (i, o) in scene.objects.iter().enumerate() {
            if !active[i] || moves(o) || !(has_collider(o) || (o.static_geometry && o.render.is_some())) {
                continue;
            }
            let m = world[i].to_cols_array();
            let settle = match self.moved.get(&o.id) {
                Some((last, left)) if *last == m => left.saturating_sub(1),
                Some(_) => SETTLE_UPDATES,
                None => 0,
            };
            seen.insert(o.id, (m, settle));
            if settle > 0 {
                continue;
            }
            let key = shape_key(o, &m);
            if self.shapes.get(&o.id).is_none_or(|c| c.key != key) {
                let solids = world_solids(scene, i, world[i]);
                let (lo, hi) = bounds(&solids);
                self.shapes.insert(o.id, Cached { key, solids: Arc::new(solids), lo, hi });
            }
            walls.push(i);
        }
        self.moved = seen;
        self.shapes.retain(|id, _| self.moved.contains_key(id));
        for wi in waters {
            let Some(water) = water_of(scene, wi) else { continue };
            let Some(window) = water.sim_feed.window else { continue };
            let water_world = world[wi];
            let to_local = water_world.inverse();
            let (wlo, whi) = window_bounds(window, water_world);
            let mut here: Vec<(ObjectId, u64)> = walls
                .iter()
                .filter(|&&i| i != wi && crate::physics::layers_meet(scene, wi, i))
                .filter_map(|&i| {
                    let id = scene.objects[i].id;
                    let c = self.shapes.get(&id)?;
                    let overlaps = c.hi.x >= wlo.x && c.lo.x <= whi.x && c.hi.z >= wlo.z && c.lo.z <= whi.z && c.hi.y >= wlo.y && c.lo.y <= whi.y;
                    overlaps.then_some((id, c.key))
                })
                .collect();
            here.sort_by_key(|(id, _)| id.raw());
            let mut h = DefaultHasher::new();
            (window.n, window.cell.to_bits(), window.origin.x, window.origin.y).hash(&mut h);
            here.hash(&mut h);
            let key = h.finish();
            if self.masks.get(&scene.objects[wi].id) == Some(&key) && water.sim_feed.blocked.as_ref().is_some_and(|(w, _)| *w == window) {
                continue;
            }
            let mut mask = vec![false; (window.n * window.n) as usize];
            for (id, _) in &here {
                if let Some(c) = self.shapes.get(id) {
                    mark(&mut mask, window, water, &to_local, water_world, &c.solids);
                }
            }
            let id = scene.objects[wi].id;
            self.masks.insert(id, key);
            if let Some(w) = water_mut(scene, wi) {
                w.sim_feed.blocked = Some((window, Arc::new(mask)));
            }
        }
    }
}

/// Mark the window's cells `solids` fill at the water's surface.
fn mark(mask: &mut [bool], window: SimWindow, water: &Water, to_local: &Mat4, water_world: Mat4, solids: &[Solid]) {
    let n = window.n as i32;
    let lo = window.lo();
    let cell = window.cell;
    let surface = |xz: Vec2| water.column(xz).map_or(0.0, |c| c.surface);
    let set = |mask: &mut [bool], xz: Vec2| {
        let c = ((xz - lo) / cell).floor().as_ivec2();
        if c.x >= 0 && c.y >= 0 && c.x < n && c.y < n {
            mask[(c.y * n + c.x) as usize] = true;
        }
    };
    for s in solids {
        match s {
            Solid::Shape(col) => {
                let aabb = col.compute_aabb();
                let (a, b) = (Vec3::new(aabb.mins.x, aabb.mins.y, aabb.mins.z), Vec3::new(aabb.maxs.x, aabb.maxs.y, aabb.maxs.z));
                let corners: [Vec3; 8] = std::array::from_fn(|k| {
                    to_local.transform_point3(Vec3::new(
                        if k & 1 == 0 { a.x } else { b.x },
                        if k & 2 == 0 { a.y } else { b.y },
                        if k & 4 == 0 { a.z } else { b.z },
                    ))
                });
                let (fl, fh) = corners.iter().fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(p, q), c| (p.min(*c), q.max(*c)));
                let x0 = (((fl.x - lo.x) / cell).floor() as i32).max(0);
                let x1 = (((fh.x - lo.x) / cell).ceil() as i32).min(n);
                let z0 = (((fl.z - lo.y) / cell).floor() as i32).max(0);
                let z1 = (((fh.z - lo.y) / cell).ceil() as i32).min(n);
                for z in z0..z1 {
                    for x in x0..x1 {
                        let xz = lo + (Vec2::new(x as f32, z as f32) + 0.5) * cell;
                        let p = water_world.transform_point3(Vec3::new(xz.x, surface(xz), xz.y));
                        if solid_contains(col, p) {
                            mask[(z * n + x) as usize] = true;
                        }
                    }
                }
            }
            Solid::Mesh(verts, tris) => {
                let local: Vec<Vec3> = verts.iter().map(|v| to_local.transform_point3(*v)).collect();
                for t in tris {
                    let p = [local[t[0] as usize], local[t[1] as usize], local[t[2] as usize]];
                    let mid = (p[0] + p[1] + p[2]) / 3.0;
                    let level = surface(Vec2::new(mid.x, mid.z));
                    let d = p.map(|v| v.y - level);
                    // The line where the triangle crosses the level.
                    let mut ends = Vec::with_capacity(2);
                    for k in 0..3 {
                        let (a, b) = (k, (k + 1) % 3);
                        if (d[a] <= 0.0) != (d[b] <= 0.0) {
                            let f = d[a] / (d[a] - d[b]);
                            let q = p[a].lerp(p[b], f);
                            ends.push(Vec2::new(q.x, q.z));
                        }
                    }
                    if let [a, b] = ends[..] {
                        let steps = ((b - a).length() / (cell * 0.5)).ceil().max(1.0) as usize;
                        for k in 0..=steps {
                            set(mask, a.lerp(b, k as f32 / steps as f32));
                        }
                    }
                }
            }
        }
    }
}

fn bounds(solids: &[Solid]) -> (Vec3, Vec3) {
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for s in solids {
        match s {
            Solid::Shape(c) => {
                let a = c.compute_aabb();
                lo = lo.min(Vec3::new(a.mins.x, a.mins.y, a.mins.z));
                hi = hi.max(Vec3::new(a.maxs.x, a.maxs.y, a.maxs.z));
            }
            Solid::Mesh(v, _) => {
                for p in v {
                    lo = lo.min(*p);
                    hi = hi.max(*p);
                }
            }
        }
    }
    (lo, hi)
}

/// World bounds of a water's window, a little above and below its surface.
fn window_bounds(window: SimWindow, water_world: Mat4) -> (Vec3, Vec3) {
    let (a, b) = (window.lo(), window.lo() + Vec2::splat(window.size()));
    let corners: [Vec3; 8] = std::array::from_fn(|k| {
        water_world.transform_point3(Vec3::new(if k & 1 == 0 { a.x } else { b.x }, if k & 2 == 0 { -2.0 } else { 2.0 }, if k & 4 == 0 { a.y } else { b.y }))
    });
    corners.iter().fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(p, q), c| (p.min(*c), q.max(*c)))
}

/// Changes when the object moves or its colliders or mesh change.
fn shape_key(o: &crate::scene::SceneObject, world: &[f32; 16]) -> u64 {
    let mut h = DefaultHasher::new();
    for v in world {
        v.to_bits().hash(&mut h);
    }
    o.static_geometry.hash(&mut h);
    o.render.map(|r| r.mesh).hash(&mut h);
    for c in &o.components {
        let a = c.as_any();
        if let Some(b) = a.downcast_ref::<BoxCollider>() {
            (0u8, b.is_trigger).hash(&mut h);
            b.center.iter().chain(&b.size).for_each(|v| v.to_bits().hash(&mut h));
        } else if let Some(s) = a.downcast_ref::<SphereCollider>() {
            (1u8, s.is_trigger, s.radius.to_bits()).hash(&mut h);
            s.center.iter().for_each(|v| v.to_bits().hash(&mut h));
        } else if let Some(cap) = a.downcast_ref::<CapsuleCollider>() {
            (2u8, cap.is_trigger, cap.radius.to_bits(), cap.height.to_bits(), cap.direction as u8).hash(&mut h);
            cap.center.iter().for_each(|v| v.to_bits().hash(&mut h));
        } else if let Some(m) = a.downcast_ref::<MeshCollider>() {
            (3u8, m.is_trigger, m.convex, m.follow_shape, m.detail as u8).hash(&mut h);
        }
    }
    h.finish()
}

fn has_collider(o: &crate::scene::SceneObject) -> bool {
    o.components.iter().any(|c| {
        let a = c.as_any();
        a.is::<BoxCollider>() || a.is::<SphereCollider>() || a.is::<CapsuleCollider>() || a.is::<MeshCollider>()
    })
}

/// Bodies that move the water instead: non-fixed Rigid Bodies, characters.
fn moves(o: &crate::scene::SceneObject) -> bool {
    o.components.iter().any(|c| {
        let a = c.as_any();
        a.is::<CharacterController>() || a.downcast_ref::<RigidBody>().is_some_and(|r| r.kind != BodyKind::Fixed)
    })
}

fn water_of(scene: &LoadedScene, i: usize) -> Option<&Water> {
    scene.objects[i].components.iter().find_map(|c| c.as_any().downcast_ref::<Water>())
}

fn water_mut(scene: &mut LoadedScene, i: usize) -> Option<&mut Water> {
    scene.objects[i].components.iter_mut().find_map(|c| c.as_any_mut().downcast_mut::<Water>())
}
