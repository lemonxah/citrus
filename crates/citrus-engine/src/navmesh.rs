//! Navigation grid + A* pathfinding (ENGINE_FEATURE_CHECKLIST T1 #24).
//!
//! A walkability grid baked from the scene (cells blocked by static geometry) plus
//! A* search with diagonal moves and a straight-line (line-of-sight) string-pull so
//! agents follow natural paths instead of staircase grid steps. This is the CPU
//! pathfinding core; a polygon navmesh is a later upgrade, but a grid covers most
//! top-down / arena / level AI and is fully testable.

use std::collections::BinaryHeap;

use glam::Vec3;

/// A walkability grid in the XZ plane (Y is up). `cell` metres per cell.
#[derive(Clone, Debug)]
pub struct NavGrid {
    origin: Vec3, // world position of cell (0,0)'s corner
    cell: f32,
    w: i32,
    h: i32,
    blocked: Vec<bool>, // row-major [z*w + x]
}

impl NavGrid {
    /// Empty (all walkable) grid covering `[min,max]` in world XZ at `cell` size.
    pub fn new(min: Vec3, max: Vec3, cell: f32) -> Self {
        let cell = cell.max(0.05);
        let w = (((max.x - min.x) / cell).ceil() as i32).max(1);
        let h = (((max.z - min.z) / cell).ceil() as i32).max(1);
        Self {
            origin: Vec3::new(min.x, min.y, min.z),
            cell,
            w,
            h,
            blocked: vec![false; (w * h) as usize],
        }
    }

    pub fn dims(&self) -> (i32, i32) {
        (self.w, self.h)
    }

    fn idx(&self, x: i32, z: i32) -> usize {
        (z * self.w + x) as usize
    }

    fn in_bounds(&self, x: i32, z: i32) -> bool {
        x >= 0 && z >= 0 && x < self.w && z < self.h
    }

    /// Mark a cell blocked/clear.
    pub fn set_blocked(&mut self, x: i32, z: i32, blocked: bool) {
        if self.in_bounds(x, z) {
            let i = self.idx(x, z);
            self.blocked[i] = blocked;
        }
    }

    /// Block every cell whose centre falls inside a world-space AABB (e.g. a static
    /// obstacle's bounds). Used to bake the grid from scene geometry.
    pub fn block_aabb(&mut self, min: Vec3, max: Vec3) {
        let (x0, z0) = self.world_to_cell(Vec3::new(min.x, 0.0, min.z));
        let (x1, z1) = self.world_to_cell(Vec3::new(max.x, 0.0, max.z));
        for z in z0.min(z1)..=z0.max(z1) {
            for x in x0.min(x1)..=x0.max(x1) {
                self.set_blocked(x, z, true);
            }
        }
    }

    pub fn is_blocked(&self, x: i32, z: i32) -> bool {
        !self.in_bounds(x, z) || self.blocked[self.idx(x, z)]
    }

    /// World position → cell coords (clamped to the grid).
    pub fn world_to_cell(&self, p: Vec3) -> (i32, i32) {
        let x = ((p.x - self.origin.x) / self.cell).floor() as i32;
        let z = ((p.z - self.origin.z) / self.cell).floor() as i32;
        (x.clamp(0, self.w - 1), z.clamp(0, self.h - 1))
    }

    /// Cell centre in world space (keeps the grid's Y origin).
    pub fn cell_to_world(&self, x: i32, z: i32) -> Vec3 {
        Vec3::new(
            self.origin.x + (x as f32 + 0.5) * self.cell,
            self.origin.y,
            self.origin.z + (z as f32 + 0.5) * self.cell,
        )
    }

    /// A* from `start` to `goal` (world space). Returns world-space waypoints
    /// (string-pulled to skip collinear / line-of-sight-clear cells), or `None` if
    /// unreachable. Diagonal moves are allowed but never cut a blocked corner.
    pub fn find_path(&self, start: Vec3, goal: Vec3) -> Option<Vec<Vec3>> {
        let s = self.world_to_cell(start);
        let g = self.world_to_cell(goal);
        if self.is_blocked(g.0, g.1) || self.is_blocked(s.0, s.1) {
            return None;
        }
        let cells = self.astar(s, g)?;
        Some(self.string_pull(&cells, start, goal))
    }

    fn astar(&self, start: (i32, i32), goal: (i32, i32)) -> Option<Vec<(i32, i32)>> {
        let n = (self.w * self.h) as usize;
        let mut g_cost = vec![f32::INFINITY; n];
        let mut came: Vec<Option<usize>> = vec![None; n];
        let mut open = BinaryHeap::new();
        let si = self.idx(start.0, start.1);
        g_cost[si] = 0.0;
        open.push(Node { f: heur(start, goal), idx: si });
        // 8-connected moves: (dx, dz, cost). Diagonals cost √2.
        const M: [(i32, i32, f32); 8] = [
            (1, 0, 1.0),
            (-1, 0, 1.0),
            (0, 1, 1.0),
            (0, -1, 1.0),
            (1, 1, 1.41421),
            (1, -1, 1.41421),
            (-1, 1, 1.41421),
            (-1, -1, 1.41421),
        ];
        let gi = self.idx(goal.0, goal.1);
        while let Some(Node { idx, .. }) = open.pop() {
            if idx == gi {
                // Reconstruct.
                let mut path = vec![idx];
                let mut cur = idx;
                while let Some(p) = came[cur] {
                    path.push(p);
                    cur = p;
                }
                path.reverse();
                return Some(
                    path.into_iter()
                        .map(|i| (i as i32 % self.w, i as i32 / self.w))
                        .collect(),
                );
            }
            let (cx, cz) = (idx as i32 % self.w, idx as i32 / self.w);
            for (dx, dz, cost) in M {
                let (nx, nz) = (cx + dx, cz + dz);
                if self.is_blocked(nx, nz) {
                    continue;
                }
                // No corner-cutting: a diagonal needs both orthogonal cells clear.
                if dx != 0 && dz != 0 && (self.is_blocked(cx + dx, cz) || self.is_blocked(cx, cz + dz)) {
                    continue;
                }
                let ni = self.idx(nx, nz);
                let tentative = g_cost[idx] + cost;
                if tentative < g_cost[ni] {
                    g_cost[ni] = tentative;
                    came[ni] = Some(idx);
                    open.push(Node { f: tentative + heur((nx, nz), goal), idx: ni });
                }
            }
        }
        None
    }

    /// True if a straight line between two cell centres crosses no blocked cell
    /// (supercover line). Used by the string-pull.
    fn line_clear(&self, a: (i32, i32), b: (i32, i32)) -> bool {
        let (mut x0, mut z0) = (a.0, a.1);
        let (x1, z1) = (b.0, b.1);
        let dx = (x1 - x0).abs();
        let dz = (z1 - z0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sz = if z0 < z1 { 1 } else { -1 };
        let mut err = dx - dz;
        loop {
            if self.is_blocked(x0, z0) {
                return false;
            }
            if x0 == x1 && z0 == z1 {
                return true;
            }
            let e2 = 2 * err;
            if e2 > -dz {
                err -= dz;
                x0 += sx;
            }
            if e2 < dx {
                err += dx;
                z0 += sz;
            }
        }
    }

    /// Greedy string-pull: keep the farthest cell still in line-of-sight, dropping
    /// intermediate staircase waypoints. Endpoints use the true world start/goal.
    fn string_pull(&self, cells: &[(i32, i32)], start: Vec3, goal: Vec3) -> Vec<Vec3> {
        if cells.len() <= 2 {
            return vec![start, goal];
        }
        let mut out = vec![start];
        let mut anchor = 0usize;
        let mut i = 1usize;
        while i < cells.len() - 1 {
            if self.line_clear(cells[anchor], cells[i + 1]) {
                i += 1; // can still see one further; extend
            } else {
                out.push(self.cell_to_world(cells[i].0, cells[i].1));
                anchor = i;
                i += 1;
            }
        }
        out.push(goal);
        out
    }
}

struct Node {
    f: f32,
    idx: usize,
}
impl PartialEq for Node {
    fn eq(&self, o: &Self) -> bool {
        self.f == o.f
    }
}
impl Eq for Node {}
impl Ord for Node {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // Min-heap on f: reverse so BinaryHeap (max-heap) pops the smallest f.
        o.f.partial_cmp(&self.f).unwrap_or(std::cmp::Ordering::Equal)
    }
}
impl PartialOrd for Node {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

fn heur(a: (i32, i32), b: (i32, i32)) -> f32 {
    // Octile distance (admissible for 8-connected grids).
    let dx = (a.0 - b.0).abs() as f32;
    let dz = (a.1 - b.1).abs() as f32;
    (dx + dz) + (1.41421 - 2.0) * dx.min(dz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_path_in_open_grid_is_two_points() {
        let grid = NavGrid::new(Vec3::ZERO, Vec3::new(10.0, 0.0, 10.0), 1.0);
        let path = grid
            .find_path(Vec3::new(0.5, 0.0, 0.5), Vec3::new(9.5, 0.0, 9.5))
            .unwrap();
        // Clear line of sight -> string-pull collapses to start + goal.
        assert_eq!(path.len(), 2);
        assert!((path[0] - Vec3::new(0.5, 0.0, 0.5)).length() < 1e-3);
    }

    #[test]
    fn routes_around_a_wall() {
        let mut grid = NavGrid::new(Vec3::ZERO, Vec3::new(10.0, 0.0, 10.0), 1.0);
        // A vertical wall at x=5 with a gap at the top.
        for z in 0..9 {
            grid.set_blocked(5, z, true);
        }
        let path = grid
            .find_path(Vec3::new(1.0, 0.0, 1.0), Vec3::new(9.0, 0.0, 1.0))
            .unwrap();
        // Must detour (more than a straight 2-point path) and avoid the wall.
        assert!(path.len() > 2, "path should bend around the wall: {path:?}");
        // No waypoint sits on the wall column.
        for p in &path {
            let (cx, _) = grid.world_to_cell(*p);
            assert!(cx != 5 || grid.is_blocked(5, grid.world_to_cell(*p).1) == false);
        }
    }

    #[test]
    fn unreachable_goal_returns_none() {
        let mut grid = NavGrid::new(Vec3::ZERO, Vec3::new(6.0, 0.0, 6.0), 1.0);
        // Fully wall off the goal cell.
        grid.block_aabb(Vec3::new(4.0, 0.0, 4.0), Vec3::new(6.0, 0.0, 6.0));
        assert!(grid
            .find_path(Vec3::new(0.5, 0.0, 0.5), Vec3::new(5.0, 0.0, 5.0))
            .is_none());
    }
}
