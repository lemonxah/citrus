//! Mesh simplification for generated LODs: quadric-error edge collapse
//! (Garland & Heckbert). Open borders, which include UV and normal seams in
//! an unwelded mesh, are locked so neighbouring charts never crack apart, and
//! collapses that would flip a triangle are refused. The surviving vertex
//! keeps its attributes (UVs, skin weights), so skinned meshes stay skinned.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use zestengine_render::MeshData;
use glam::{DMat4, DVec3, DVec4, Vec3};

#[derive(Clone, Copy, Default)]
struct Quadric(DMat4);

impl Quadric {
    fn plane(n: DVec3, d: f64, weight: f64) -> Self {
        let p = DVec4::new(n.x, n.y, n.z, d);
        Quadric(DMat4::from_cols(p * p.x, p * p.y, p * p.z, p * p.w) * weight)
    }
    fn error(&self, v: DVec3) -> f64 {
        let p = DVec4::new(v.x, v.y, v.z, 1.0);
        p.dot(self.0 * p).max(0.0)
    }
    fn add(&self, o: &Quadric) -> Quadric {
        Quadric(self.0 + o.0)
    }
}

#[derive(PartialEq)]
struct Candidate {
    error: f64,
    a: u32,
    b: u32,
    /// Collapse target: 0 = a's position, 1 = b's, 2 = midpoint.
    at: u8,
    stamp: (u32, u32),
}

impl Eq for Candidate {}
impl Ord for Candidate {
    fn cmp(&self, o: &Self) -> Ordering {
        o.error.partial_cmp(&self.error).unwrap_or(Ordering::Equal)
    }
}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// Simplify `mesh` down to about `keep` (0..1) of its triangles.
pub fn simplify(mesh: &MeshData, keep: f32) -> MeshData {
    let tri_count = mesh.indices.len() / 3;
    let target = ((tri_count as f32 * keep.clamp(0.0, 1.0)).round() as usize).max(4);
    if tri_count <= target || mesh.vertices.is_empty() {
        return MeshData { vertices: mesh.vertices.clone(), indices: mesh.indices.clone(), has_lightmap_uv: mesh.has_lightmap_uv };
    }
    let mut pos: Vec<DVec3> = mesh.vertices.iter().map(|v| Vec3::from(v.position).as_dvec3()).collect();
    let mut tris: Vec<[u32; 3]> = mesh.indices.as_chunks::<3>().0.iter().map(|t| [t[0], t[1], t[2]]).collect();
    let mut alive = vec![true; tris.len()];
    let n = pos.len();

    // Border edges (one triangle) lock their vertices: in an unwelded mesh that
    // covers both real holes and attribute seams.
    let mut edge_use: HashMap<(u32, u32), u32> = HashMap::new();
    for t in &tris {
        for k in 0..3 {
            let (a, b) = (t[k].min(t[(k + 1) % 3]), t[k].max(t[(k + 1) % 3]));
            *edge_use.entry((a, b)).or_default() += 1;
        }
    }
    let mut locked = vec![false; n];
    for ((a, b), uses) in &edge_use {
        if *uses == 1 {
            locked[*a as usize] = true;
            locked[*b as usize] = true;
        }
    }
    let mut faces_of: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (f, t) in tris.iter().enumerate() {
        for &v in t {
            faces_of[v as usize].push(f as u32);
        }
    }
    let mut quad = vec![Quadric::default(); n];
    for t in &tris {
        let (a, b, c) = (pos[t[0] as usize], pos[t[1] as usize], pos[t[2] as usize]);
        let cross = (b - a).cross(c - a);
        let area = cross.length();
        if area < 1e-18 {
            continue;
        }
        let nrm = cross / area;
        let q = Quadric::plane(nrm, -nrm.dot(a), area);
        for &v in t {
            quad[v as usize] = quad[v as usize].add(&q);
        }
    }
    let mut version = vec![0u32; n];
    let mut removed = vec![false; n];

    let candidate = |a: u32, b: u32, pos: &[DVec3], quad: &[Quadric], locked: &[bool], version: &[u32]| -> Option<Candidate> {
        let (la, lb) = (locked[a as usize], locked[b as usize]);
        if la && lb {
            return None;
        }
        let q = quad[a as usize].add(&quad[b as usize]);
        let (pa, pb) = (pos[a as usize], pos[b as usize]);
        let options: &[(u8, DVec3)] = if la {
            &[(0, pa)]
        } else if lb {
            &[(1, pb)]
        } else {
            &[(0, pa), (1, pb), (2, (pa + pb) * 0.5)]
        };
        let (at, err) = options.iter().map(|(k, p)| (*k, q.error(*p))).min_by(|x, y| x.1.total_cmp(&y.1))?;
        Some(Candidate { error: err, a, b, at, stamp: (version[a as usize], version[b as usize]) })
    };
    let mut heap = BinaryHeap::new();
    for (a, b) in edge_use.keys() {
        if let Some(c) = candidate(*a, *b, &pos, &quad, &locked, &version) {
            heap.push(c);
        }
    }

    let mut live = tri_count;
    while live > target {
        let Some(c) = heap.pop() else { break };
        let (a, b) = (c.a as usize, c.b as usize);
        if removed[a] || removed[b] || (version[a], version[b]) != c.stamp {
            continue;
        }
        // Keep `keep_v`, drop `gone`; the kept vertex moves to the target.
        let (keep_v, gone, target_pos) = match c.at {
            0 => (a, b, pos[a]),
            1 => (b, a, pos[b]),
            _ => (a, b, (pos[a] + pos[b]) * 0.5),
        };
        // Refuse collapses that flip or squash a surviving triangle.
        let mut flips = false;
        for &f in faces_of[keep_v].iter().chain(&faces_of[gone]) {
            let f = f as usize;
            if !alive[f] {
                continue;
            }
            let t = tris[f];
            if t.contains(&(keep_v as u32)) && t.contains(&(gone as u32)) {
                continue;
            }
            let before = (pos[t[1] as usize] - pos[t[0] as usize]).cross(pos[t[2] as usize] - pos[t[0] as usize]);
            let moved: Vec<DVec3> = t
                .iter()
                .map(|&v| if v as usize == keep_v || v as usize == gone { target_pos } else { pos[v as usize] })
                .collect();
            let after = (moved[1] - moved[0]).cross(moved[2] - moved[0]);
            if before.dot(after) <= 0.0 || after.length_squared() < before.length_squared() * 1e-6 {
                flips = true;
                break;
            }
        }
        if flips {
            continue;
        }
        pos[keep_v] = target_pos;
        quad[keep_v] = quad[keep_v].add(&quad[gone]);
        removed[gone] = true;
        version[keep_v] += 1;
        let moved_faces = std::mem::take(&mut faces_of[gone]);
        for f in moved_faces {
            let fi = f as usize;
            if !alive[fi] {
                continue;
            }
            let t = &mut tris[fi];
            if t.contains(&(keep_v as u32)) {
                alive[fi] = false;
                live -= 1;
                continue;
            }
            for v in t.iter_mut() {
                if *v as usize == gone {
                    *v = keep_v as u32;
                }
            }
            faces_of[keep_v].push(f);
        }
        faces_of[keep_v].retain(|f| alive[*f as usize]);
        // Re-queue the kept vertex's edges.
        let mut neighbours: Vec<u32> = faces_of[keep_v].iter().flat_map(|f| tris[*f as usize]).filter(|v| *v as usize != keep_v).collect();
        neighbours.sort_unstable();
        neighbours.dedup();
        for nb in neighbours {
            if let Some(c) = candidate(keep_v as u32, nb, &pos, &quad, &locked, &version) {
                heap.push(c);
            }
        }
    }

    // Compact the surviving vertices and triangles.
    let mut remap = vec![u32::MAX; n];
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (f, t) in tris.iter().enumerate() {
        if !alive[f] {
            continue;
        }
        for &v in t {
            let v = v as usize;
            if remap[v] == u32::MAX {
                remap[v] = vertices.len() as u32;
                let mut vert = mesh.vertices[v];
                vert.position = pos[v].as_vec3().to_array();
                vertices.push(vert);
            }
            indices.push(remap[v]);
        }
    }
    MeshData { vertices, indices, has_lightmap_uv: mesh.has_lightmap_uv }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(m: &MeshData) -> (Vec3, Vec3) {
        m.vertices.iter().fold((Vec3::MAX, Vec3::MIN), |(lo, hi), v| (lo.min(Vec3::from(v.position)), hi.max(Vec3::from(v.position))))
    }

    #[test]
    fn a_sphere_halves_and_keeps_its_shape() {
        let sphere = crate::procedural::primitive_mesh(crate::PrimitiveShape::Sphere);
        let tris = sphere.indices.len() / 3;
        let low = simplify(&sphere, 0.5);
        let low_tris = low.indices.len() / 3;
        assert!(low_tris <= tris * 6 / 10 && low_tris >= tris / 4, "{tris} -> {low_tris}");
        let (a, b) = (bounds(&sphere), bounds(&low));
        assert!((a.0 - b.0).length() < 0.05 && (a.1 - b.1).length() < 0.05, "silhouette kept");
        for t in low.indices.as_chunks::<3>().0 {
            assert!(t[0] != t[1] && t[1] != t[2] && t[0] != t[2], "no degenerate triangles");
        }
    }

    #[test]
    fn flat_grids_collapse_hard_but_keep_their_border() {
        let n = 20u32;
        let mut vertices = Vec::new();
        for y in 0..=n {
            for x in 0..=n {
                vertices.push(zestengine_render::Vertex { position: [x as f32, 0.0, y as f32], ..Default::default() });
            }
        }
        let mut indices = Vec::new();
        for y in 0..n {
            for x in 0..n {
                let i = y * (n + 1) + x;
                indices.extend([i, i + n + 1, i + 1, i + 1, i + n + 1, i + n + 2]);
            }
        }
        let grid = MeshData { vertices, indices, has_lightmap_uv: false };
        let low = simplify(&grid, 0.1);
        assert!(low.indices.len() / 3 < 300, "a flat plane loses most of its triangles: {}", low.indices.len() / 3);
        let (lo, hi) = bounds(&low);
        assert_eq!((lo, hi), (Vec3::ZERO, Vec3::new(n as f32, 0.0, n as f32)), "border locked");
        assert!(low.vertices.iter().all(|v| v.position[1].abs() < 1e-6), "stays flat");
    }
}
