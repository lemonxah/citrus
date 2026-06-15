//! Per-mesh signed distance field generation for the software (Lumen-style) GI
//! path. A mesh is voxelized once into a local-space SDF grid: each voxel stores
//! the signed distance to the nearest triangle (negative inside). The software
//! GI marches these instead of a hardware BVH, so it needs no RT cores.
//!
//! Sign is taken from the nearest triangle's geometric normal. That works for
//! the face-dominant primitives we ship; thin shells / sharp edges on imported
//! meshes can mis-sign a few voxels (a generalized-winding-number sign is the
//! follow-up).
//!
//! Generation is done; the GPU march that consumes it lands in Phase 1c.
#![allow(dead_code)]

use glam::Vec3;

/// A mesh's local-space signed distance field over a padded AABB. `data` is
/// `dims.x * dims.y * dims.z` distances, x fastest then y then z.
#[derive(Clone, Debug)]
pub struct SdfVolume {
    pub dims: [u32; 3],
    /// World/local-space bounds the grid spans (mesh AABB + padding).
    pub min: Vec3,
    pub max: Vec3,
    pub data: Vec<f32>,
}

impl SdfVolume {
    /// Trilinearly sample the signed distance at a local-space point. Points
    /// outside the grid clamp to the border (distance grows roughly linearly
    /// outside via the clamp + the padding margin).
    pub fn sample(&self, p: Vec3) -> f32 {
        // Exterior points: return the distance to the SDF's box. The grid only
        // stores distances near the surface; outside it, the clamped border
        // value badly underestimates the true distance, which stalls sphere
        // marching. The box distance is conservative (the surface lies inside
        // the box, so a ray never overshoots it) and lets the march advance.
        let q = p.clamp(self.min, self.max);
        let outside = p.distance(q);
        if outside > 1e-6 {
            return outside;
        }
        let size = self.max - self.min;
        let inv = Vec3::new(
            (self.dims[0].max(1) - 1).max(1) as f32 / size.x.max(1e-6),
            (self.dims[1].max(1) - 1).max(1) as f32 / size.y.max(1e-6),
            (self.dims[2].max(1) - 1).max(1) as f32 / size.z.max(1e-6),
        );
        let g = (p - self.min) * inv;
        let (dx, dy, dz) = (self.dims[0] as i32, self.dims[1] as i32, self.dims[2] as i32);
        let clampi = |v: f32, n: i32| (v.floor() as i32).clamp(0, n - 1);
        let x0 = clampi(g.x, dx);
        let y0 = clampi(g.y, dy);
        let z0 = clampi(g.z, dz);
        let x1 = (x0 + 1).min(dx - 1);
        let y1 = (y0 + 1).min(dy - 1);
        let z1 = (z0 + 1).min(dz - 1);
        let fx = (g.x - x0 as f32).clamp(0.0, 1.0);
        let fy = (g.y - y0 as f32).clamp(0.0, 1.0);
        let fz = (g.z - z0 as f32).clamp(0.0, 1.0);
        let at = |x: i32, y: i32, z: i32| {
            self.data[(z * dy * dx + y * dx + x) as usize]
        };
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let c00 = lerp(at(x0, y0, z0), at(x1, y0, z0), fx);
        let c10 = lerp(at(x0, y1, z0), at(x1, y1, z0), fx);
        let c01 = lerp(at(x0, y0, z1), at(x1, y0, z1), fx);
        let c11 = lerp(at(x0, y1, z1), at(x1, y1, z1), fx);
        lerp(lerp(c00, c10, fy), lerp(c01, c11, fy), fz)
    }
}

/// Closest point on triangle (a,b,c) to p (Ericson, Real-Time Collision
/// Detection). Returns the point.
fn closest_on_tri(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Vec3 {
    let ab = b - a;
    let ac = c - a;
    let ap = p - a;
    let d1 = ab.dot(ap);
    let d2 = ac.dot(ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }
    let bp = p - b;
    let d3 = ab.dot(bp);
    let d4 = ac.dot(bp);
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return a + ab * v;
    }
    let cp = p - c;
    let d5 = ab.dot(cp);
    let d6 = ac.dot(cp);
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return a + ac * w;
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return b + (c - b) * w;
    }
    let denom = 1.0 / (va + vb + vc);
    let v = vb * denom;
    let w = vc * denom;
    a + ab * v + ac * w
}

/// Voxelize `positions`/`indices` into a signed distance field. `res` is the
/// voxel count along the longest axis (voxels are kept ~cubic); `pad` is extra
/// world-units of margin around the AABB so the surface isn't at the very edge.
pub fn generate_sdf(positions: &[Vec3], indices: &[u32], res: u32, pad: f32) -> SdfVolume {
    let res = res.max(2);
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for p in positions {
        lo = lo.min(*p);
        hi = hi.max(*p);
    }
    if !lo.is_finite() {
        return SdfVolume {
            dims: [2, 2, 2],
            min: Vec3::splat(-1.0),
            max: Vec3::splat(1.0),
            data: vec![1.0; 8],
        };
    }
    let margin = pad.max((hi - lo).length() * 0.05 + 1e-3);
    let (min, max) = (lo - margin, hi + margin);
    let extent = (max - min).max(Vec3::splat(1e-3));
    let voxel = extent.max_element() / res as f32;
    let dims = [
        ((extent.x / voxel).ceil() as u32).max(2),
        ((extent.y / voxel).ceil() as u32).max(2),
        ((extent.z / voxel).ceil() as u32).max(2),
    ];

    // Precompute triangle corners + geometric normals.
    let tris: Vec<(Vec3, Vec3, Vec3, Vec3)> = indices
        .chunks_exact(3)
        .map(|t| {
            let a = positions[t[0] as usize];
            let b = positions[t[1] as usize];
            let c = positions[t[2] as usize];
            let n = (b - a).cross(c - a).normalize_or_zero();
            (a, b, c, n)
        })
        .collect();

    let mut data = vec![0.0f32; (dims[0] * dims[1] * dims[2]) as usize];
    let step = Vec3::new(
        extent.x / (dims[0] - 1) as f32,
        extent.y / (dims[1] - 1) as f32,
        extent.z / (dims[2] - 1) as f32,
    );
    for z in 0..dims[2] {
        for y in 0..dims[1] {
            for x in 0..dims[0] {
                let p = min + Vec3::new(x as f32, y as f32, z as f32) * step;
                let mut best = f32::INFINITY;
                let mut best_sign = 1.0f32;
                for (a, b, c, n) in &tris {
                    let cp = closest_on_tri(p, *a, *b, *c);
                    let d2 = (p - cp).length_squared();
                    if d2 < best {
                        best = d2;
                        best_sign = if (p - cp).dot(*n) < 0.0 { -1.0 } else { 1.0 };
                    }
                }
                let idx = (z * dims[1] * dims[0] + y * dims[0] + x) as usize;
                data[idx] = best.sqrt() * best_sign;
            }
        }
    }
    SdfVolume { dims, min, max, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A unit cube (centered) voxelized: center is inside (negative ~ -0.5),
    // a point well outside is positive.
    fn unit_cube() -> (Vec<Vec3>, Vec<u32>) {
        let h = 0.5;
        let v = [
            [-h, -h, -h], [h, -h, -h], [h, h, -h], [-h, h, -h],
            [-h, -h, h], [h, -h, h], [h, h, h], [-h, h, h],
        ]
        .map(|c| Vec3::from_array(c))
        .to_vec();
        // 12 triangles, outward winding.
        let idx = vec![
            0, 2, 1, 0, 3, 2, // -z
            4, 5, 6, 4, 6, 7, // +z
            0, 1, 5, 0, 5, 4, // -y
            3, 7, 6, 3, 6, 2, // +y
            0, 4, 7, 0, 7, 3, // -x
            1, 2, 6, 1, 6, 5, // +x
        ];
        (v, idx)
    }

    #[test]
    fn sdf_sign_inside_outside() {
        let (pos, idx) = unit_cube();
        let sdf = generate_sdf(&pos, &idx, 24, 0.25);
        // Center is inside → negative, ~ -0.5 to a face.
        let c = sdf.sample(Vec3::ZERO);
        assert!(c < 0.0, "center should be inside (negative), got {c}");
        assert!((c + 0.5).abs() < 0.15, "center distance ~ -0.5, got {c}");
        // A point a unit outside along +x → positive ~0.5.
        let o = sdf.sample(Vec3::new(1.0, 0.0, 0.0));
        assert!(o > 0.0, "outside should be positive, got {o}");
    }
}
