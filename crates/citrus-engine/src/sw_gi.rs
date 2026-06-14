//! Software (Lumen-style) GI probe march — pure CPU, no RT cores. Mirrors the
//! ray-query `bake_probe.comp` but sphere-marches per-mesh signed distance
//! fields instead of tracing a hardware BVH, so it runs on any GPU. Produces the
//! same SH-L1 `ProbeSh` representation the standard shader already samples.
//!
//! Probes are few and the update is throttled + dirty-gated, so a CPU march is
//! affordable for a coarse soft-GI preview; a GPU compute version is a later
//! perf optimization.

use std::sync::Arc;

use citrus_render::sdf::SdfVolume;
use citrus_render::{BakeLight, LightKind, ProbeSh};
use glam::{Mat4, Vec3};

const PI: f32 = std::f32::consts::PI;
const MAX_STEPS: u32 = 96;

/// One marchable instance: a mesh SDF placed in the world. Owns an `Arc` to its
/// SDF so the whole march can be moved onto a background thread (the SDFs are
/// shared, not copied).
#[derive(Clone)]
pub struct SdfInstance {
    /// World → mesh-local transform.
    pub inv: Mat4,
    /// Local-distance → world-distance scale (mesh's world scale).
    pub scale: f32,
    pub sdf: Arc<SdfVolume>,
    pub albedo: [f32; 3],
    pub emission: [f32; 3],
}

fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846ca68b);
    x ^= x >> 16;
    x
}
fn rnd(state: &mut u32) -> f32 {
    *state = hash(*state);
    *state as f32 * (1.0 / 4294967296.0)
}
fn uniform_sphere(rng: &mut u32) -> Vec3 {
    let z = rnd(rng) * 2.0 - 1.0;
    let a = rnd(rng) * 2.0 * PI;
    let r = (1.0 - z * z).max(0.0).sqrt();
    Vec3::new(r * a.cos(), r * a.sin(), z)
}
/// Orthonormal basis around `n` (Duff et al.).
fn basis(n: Vec3) -> (Vec3, Vec3) {
    let s = if n.z >= 0.0 { 1.0 } else { -1.0 };
    let a = -1.0 / (s + n.z);
    let b = n.x * n.y * a;
    (
        Vec3::new(1.0 + s * n.x * n.x * a, s * b, -s * n.x),
        Vec3::new(b, s + n.y * n.y * a, -n.y),
    )
}
/// Cosine-weighted hemisphere sample around `n` (for diffuse bounce rays).
fn cosine_hemisphere(n: Vec3, rng: &mut u32) -> Vec3 {
    let (u1, u2) = (rnd(rng), rnd(rng));
    let r = u1.sqrt();
    let phi = 2.0 * PI * u2;
    let (t, b) = basis(n);
    (t * (r * phi.cos()) + b * (r * phi.sin()) + n * (1.0 - u1).max(0.0).sqrt()).normalize_or(n)
}

/// Signed distance to the merged scene at world point `p` (min over instances),
/// returning (distance, nearest-instance index).
fn scene_distance(insts: &[SdfInstance], p: Vec3) -> (f32, usize) {
    let mut best = f32::INFINITY;
    let mut who = 0;
    for (i, inst) in insts.iter().enumerate() {
        let local = inst.inv.transform_point3(p);
        let d = inst.sdf.sample(local) * inst.scale;
        if d < best {
            best = d;
            who = i;
        }
    }
    (best, who)
}

/// Central-difference gradient of the merged field → surface normal.
fn scene_normal(insts: &[SdfInstance], p: Vec3, eps: f32) -> Vec3 {
    let dx = scene_distance(insts, p + Vec3::X * eps).0 - scene_distance(insts, p - Vec3::X * eps).0;
    let dy = scene_distance(insts, p + Vec3::Y * eps).0 - scene_distance(insts, p - Vec3::Y * eps).0;
    let dz = scene_distance(insts, p + Vec3::Z * eps).0 - scene_distance(insts, p - Vec3::Z * eps).0;
    Vec3::new(dx, dy, dz).normalize_or(Vec3::Y)
}

/// Sphere-march; returns the hit (point, instance index) or None on escape.
fn march(insts: &[SdfInstance], origin: Vec3, dir: Vec3, eps: f32, max_dist: f32) -> Option<(Vec3, usize)> {
    let mut t = eps * 2.0;
    for _ in 0..MAX_STEPS {
        let p = origin + dir * t;
        let (d, who) = scene_distance(insts, p);
        if d < eps {
            return Some((p, who));
        }
        t += d.max(eps);
        if t > max_dist {
            break;
        }
    }
    None
}

/// True if anything occludes the segment from `origin` toward `dir` within `dist`.
fn occluded(insts: &[SdfInstance], origin: Vec3, dir: Vec3, dist: f32, eps: f32) -> bool {
    let mut t = eps * 2.0;
    for _ in 0..MAX_STEPS {
        if t > dist - eps {
            return false;
        }
        let (d, _) = scene_distance(insts, origin + dir * t);
        if d < eps {
            return true;
        }
        t += d.max(eps);
    }
    false
}

fn light_cos(deg: f32) -> f32 {
    (deg.to_radians() * 0.5).cos()
}

/// Direct lighting at surface point `p` with normal `n` (matches the bake's
/// `direct_light`, with SDF-marched shadow occlusion).
fn direct_light(insts: &[SdfInstance], lights: &[BakeLight], p: Vec3, n: Vec3, eps: f32) -> Vec3 {
    let mut sum = Vec3::ZERO;
    for l in lights {
        let (to_light, dist, atten) = match l.kind {
            LightKind::Directional => (-l.direction.normalize_or(Vec3::NEG_Y), 1e16, 1.0),
            _ => {
                let d = l.position - p;
                let dist = d.length();
                if dist < 1e-4 {
                    continue;
                }
                let tl = d / dist;
                let range = l.range.max(1e-3);
                let f = (1.0 - dist / range).clamp(0.0, 1.0);
                let mut atten = f * f / (dist * dist).max(1e-3);
                if matches!(l.kind, LightKind::Spot) {
                    let cd = l.direction.normalize_or(Vec3::NEG_Z).dot(-tl);
                    let (ci, co) = (light_cos(l.spot_inner_deg), light_cos(l.spot_outer_deg));
                    let s = ((cd - co) / (ci - co).max(1e-4)).clamp(0.0, 1.0);
                    atten *= s * s;
                }
                (tl, dist, atten)
            }
        };
        let ndl = n.dot(to_light).max(0.0);
        if ndl <= 0.0 || atten <= 0.0 {
            continue;
        }
        if occluded(insts, p + n * (eps * 2.0), to_light, dist, eps) {
            continue;
        }
        sum += Vec3::from(l.color) * (ndl * atten);
    }
    sum
}

/// Incoming radiance arriving along `dir`, path-traced through up to `bounces`
/// diffuse bounces (sky on escape; emission + direct at each hit; cosine-weighted
/// continuation, throughput attenuated by albedo each bounce — the "lumens drop
/// per bounce" scatter). One bounce ≈ the old behavior; more fills cavities.
#[allow(clippy::too_many_arguments)]
fn incoming(
    insts: &[SdfInstance],
    lights: &[BakeLight],
    origin: Vec3,
    dir: Vec3,
    sky: Vec3,
    eps: f32,
    max_dist: f32,
    bounces: u32,
    rng: &mut u32,
) -> Vec3 {
    let mut radiance = Vec3::ZERO;
    let mut throughput = Vec3::ONE;
    let (mut ro, mut rd) = (origin, dir);
    for _ in 0..bounces.max(1) {
        match march(insts, ro, rd, eps, max_dist) {
            None => {
                radiance += throughput * sky;
                break;
            }
            Some((hp, who)) => {
                let mut n = scene_normal(insts, hp, eps);
                if n.dot(rd) > 0.0 {
                    n = -n;
                }
                let inst = &insts[who];
                let albedo = Vec3::from(inst.albedo);
                radiance += throughput * Vec3::from(inst.emission);
                // Lambert emit toward the ray: albedo/π · direct (cosine in direct).
                radiance += throughput * (albedo / PI) * direct_light(insts, lights, hp, n, eps);
                // Continue the path: cosine-weighted bounce; for a Lambert BRDF the
                // /π, cosine and pdf cancel, so throughput just folds in albedo.
                throughput *= albedo;
                if throughput.max_element() < 0.01 {
                    break;
                }
                ro = hp + n * (eps * 2.0);
                rd = cosine_hemisphere(n, rng);
            }
        }
    }
    radiance
}

/// SH-L1 radiance at one probe (uniform-sphere Monte-Carlo + multi-bounce).
#[allow(clippy::too_many_arguments)]
fn probe_sh(
    insts: &[SdfInstance],
    lights: &[BakeLight],
    origin: Vec3,
    sky: Vec3,
    eps: f32,
    max_dist: f32,
    samples: u32,
    bounces: u32,
    pi: usize,
    seed: u32,
) -> ProbeSh {
    let mut rng = hash(
        (pi as u32)
            .wrapping_mul(9277)
            .wrapping_add(seed.wrapping_mul(26699)),
    );
    let (mut sh0, mut sh1, mut sh2, mut sh3) = (Vec3::ZERO, Vec3::ZERO, Vec3::ZERO, Vec3::ZERO);
    for _ in 0..samples {
        let d = uniform_sphere(&mut rng);
        let r = incoming(insts, lights, origin, d, sky, eps, max_dist, bounces, &mut rng)
            .min(Vec3::splat(8.0));
        sh0 += r * 0.282095;
        sh1 += r * (0.488603 * d.y);
        sh2 += r * (0.488603 * d.z);
        sh3 += r * (0.488603 * d.x);
    }
    let wgt = (4.0 * PI) / samples as f32;
    let pack = |v: Vec3| [v.x, v.y, v.z];
    ProbeSh {
        coeffs: [pack(sh0 * wgt), pack(sh1 * wgt), pack(sh2 * wgt), pack(sh3 * wgt)],
    }
}

/// March every probe and return SH-L1 radiance coefficients (same layout as the
/// ray-query probe bake). Parallelized across CPU cores — each probe is
/// independent. `scene_size` sizes the march epsilon + reach.
pub fn march_probes(
    insts: &[SdfInstance],
    lights: &[BakeLight],
    probes: &[Vec3],
    sky_color: [f32; 3],
    samples: u32,
    scene_size: f32,
    bounces: u32,
    seed: u32,
) -> Vec<ProbeSh> {
    let sky = Vec3::from(sky_color);
    let eps = (scene_size * 0.004).max(1e-3);
    let max_dist = scene_size * 1.5 + 1.0;
    let samples = samples.max(1);
    let bounces = bounces.clamp(1, 16);
    let n = probes.len();
    let mut out = vec![ProbeSh::default(); n];
    let cores = std::thread::available_parallelism()
        .map(|c| c.get())
        .unwrap_or(4)
        .max(1);
    let chunk = n.div_ceil(cores).max(1);
    std::thread::scope(|s| {
        for (ci, (pchunk, ochunk)) in probes.chunks(chunk).zip(out.chunks_mut(chunk)).enumerate() {
            let base = ci * chunk;
            s.spawn(move || {
                for (k, &origin) in pchunk.iter().enumerate() {
                    ochunk[k] = probe_sh(
                        insts, lights, origin, sky, eps, max_dist, samples, bounces, base + k, seed,
                    );
                }
            });
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use citrus_render::sdf::generate_sdf;

    // A probe above a lit floor cube must gather non-zero bounced radiance.
    #[test]
    fn probe_gathers_bounced_light() {
        // Unit cube centered at origin (top face at y = 0.5).
        let h = 0.5f32;
        let v: Vec<Vec3> = [
            [-h, -h, -h], [h, -h, -h], [h, h, -h], [-h, h, -h],
            [-h, -h, h], [h, -h, h], [h, h, h], [-h, h, h],
        ]
        .iter()
        .map(|c| Vec3::from_array(*c))
        .collect();
        let idx = vec![
            0, 2, 1, 0, 3, 2, 4, 5, 6, 4, 6, 7, 0, 1, 5, 0, 5, 4, 3, 7, 6, 3, 6, 2, 0, 4, 7, 0, 7,
            3, 1, 2, 6, 1, 6, 5,
        ];
        let sdf = Arc::new(generate_sdf(&v, &idx, 32, 0.1));
        let inst = SdfInstance {
            inv: Mat4::IDENTITY,
            scale: 1.0,
            sdf,
            albedo: [0.8, 0.8, 0.8],
            emission: [0.0; 3],
        };
        let light = BakeLight {
            kind: LightKind::Point,
            position: Vec3::new(0.0, 2.0, 0.0),
            direction: Vec3::ZERO,
            color: [20.0, 20.0, 20.0],
            range: 20.0,
            spot_inner_deg: 0.0,
            spot_outer_deg: 0.0,
            radius: 0.0,
        };
        // Probe above the cube's top face.
        let probe = Vec3::new(0.0, 1.2, 0.0);
        let out = march_probes(&[inst], &[light], &[probe], [0.0; 3], 512, 4.0, 1, 1);
        let l0 = out[0].coeffs[0];
        assert!(
            l0[0] > 0.0,
            "probe should gather bounced light from the lit cube top, got L0 {l0:?}"
        );
    }
}
