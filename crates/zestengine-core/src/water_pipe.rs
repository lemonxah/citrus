//! Shallow-water simulation of a Water body's interactive layer: a virtual
//! pipe model (O'Brien & Hodgins; Mei, Decaudin, Hu 2007) on a square window
//! of cells. Each cell holds a water column, four outflow pipes and the
//! water's horizontal velocity; gravity drives the pipes from the head
//! difference, so water has mass and momentum, piles up, sloshes, flows
//! round solid cells and reflects off them.
//!
//! The GPU runs this (`water_sim.comp` in zestengine-render); [`PipeSim`] is the
//! same scheme on the CPU, pass for pass, as its reference and for tests.
//! What the engine feeds both lives here: the window ([`choose_window`]),
//! the per-cell field ([`SimField`]: rest depth, base flow, solid) and the
//! bodies' footprints ([`WaterDisplacer`]).
//!
//! The simulated state is a perturbation of the rest water: a river's or
//! current's base flow is not in the pipes, it advects the perturbation, and
//! obstacles in it push back (the bow and the lee). Bodies occupy volume in
//! the columns under them, so the water they displace is pushed out by the
//! solver and mass is kept. Waves travel at the speed for `min(depth,
//! wave depth)`; the column itself keeps the real depth.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use glam::{IVec2, Vec2, Vec3};

use crate::water::{Water, WaterBody};
use crate::water_ewave::{EwaveLayer, EwaveParams, MAX_ORBITAL, sponge_depth};
use crate::water_sim::WaterPush;

const GRAVITY: f32 = 9.81;
/// Most substeps one frame takes; a longer frame runs the water slower.
pub const MAX_SIM_SUBSTEPS: u32 = 16;
/// Courant number the substeps keep under.
const COURANT: f32 = 0.5;
/// Water-dragging rate (1/s) at `sim_momentum` 1.
const MOMENTUM_RATE: f32 = 1.5;
/// Eddy viscosity rate (1/s) at Chop smoothing 1.
const EDDY_RATE: f32 = 2.0;
/// Most of a cell's velocity difference to its neighbours viscosity evens
/// out in one substep (keeps the explicit diffusion stable).
const VISCOUS_STEP_MAX: f32 = 0.2;

/// Bed friction per unit kinematic viscosity (1/(m^2)): 3 / (h min(h, delta))
/// over water `h` deep, where delta = sqrt(2 nu / omega) is the oscillating
/// boundary layer of the waves the grid carries (about eight cells long):
/// laminar sheet flow when the layer fills the depth, a thin Stokes layer
/// at the bed when it doesn't.
pub fn bed_friction(h: f32, cell: f32, gravity: f32, viscosity: f32) -> f32 {
    let k = std::f32::consts::PI / (4.0 * cell.max(1e-3));
    let omega = (gravity * k * (k * h).min(10.0).tanh()).max(1e-4).sqrt();
    let delta = (2.0 * viscosity / omega).sqrt();
    3.0 / (h * h.min(delta).max(1e-3))
}
/// Rate (1/s) the level eases back to rest, so sources and outflow balance.
const LEVEL_RETURN: f32 = 0.02;
/// A push's dent (m) and added water (m/s) per m/s of push.
const PUSH_DENT: f32 = 0.03;
const PUSH_RATE: f32 = 0.05;
/// Fastest water velocity the solver reports (m/s).
const MAX_SPEED: f32 = 8.0;
/// Field cell kinds (`SimField` cells' w).
pub const CELL_WATER: f32 = 0.0;
pub const CELL_SHORE: f32 = 1.0;
pub const CELL_OBSTACLE: f32 = 2.0;
/// Shallower ground than this (m) starts dry.
const MIN_WET_DEPTH: f32 = 0.03;
/// Ground up to this high above the rest surface (m) is a beach waves can
/// run up onto and drain back from; higher is a bank (a wall).
pub const RUNUP_HEIGHT: f32 = 0.6;
/// Water thinner than this (m) over a beach cell is drawn as dry (sunk
/// under the ground) and holds no dispersive ripples.
pub const DRY_FILM: f32 = 0.01;
/// How far under the ground (m) a dry beach cell's surface is put.
const DRY_SINK: f32 = 0.15;

/// The square of cells a simulation covers (water-local XZ).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimWindow {
    /// The first cell, in whole cells from the local origin.
    pub origin: IVec2,
    pub n: u32,
    /// Cell width (m).
    pub cell: f32,
    /// The window covers the whole body: its edges are walls. Otherwise it
    /// follows a focus and its border absorbs what leaves.
    pub closed: bool,
}

impl SimWindow {
    pub fn size(&self) -> f32 {
        self.n as f32 * self.cell
    }

    pub fn center(&self) -> Vec2 {
        (self.origin.as_vec2() + Vec2::splat(self.n as f32 * 0.5)) * self.cell
    }

    /// Local XZ of the window's first corner.
    pub fn lo(&self) -> Vec2 {
        self.origin.as_vec2() * self.cell
    }

    /// Absorbing border width in cells (0 when closed).
    pub fn sponge(&self) -> u32 {
        if self.closed { 0 } else { (self.n / 16).max(2) }
    }
}

/// Cells per side the simulation runs for a requested resolution (a
/// multiple of 8, 32..512).
pub fn sim_resolution(r: u32) -> u32 {
    (r.clamp(32, 512) + 4) / 8 * 8
}

/// The window for `water` this frame: the whole body when it fits in
/// `ripple_area` (cells sized to it), else `ripple_area` around `focus`
/// (local XZ), kept until the focus is an eighth of the window away.
pub fn choose_window(water: &Water, focus: Vec2, prev: Option<SimWindow>) -> SimWindow {
    let n = sim_resolution(water.ripple_resolution);
    let area = water.ripple_area.max(1.0);
    if let Some((lo, hi)) = water.extent() {
        // A margin past the fade the renderer applies at the window border.
        let size = (hi - lo).max_element().max(1.0) * 1.2;
        if size <= area {
            let cell = size / n as f32;
            let origin = ((lo + hi) * 0.5 / cell - Vec2::splat(n as f32 * 0.5)).round().as_ivec2();
            return SimWindow { origin, n, cell, closed: true };
        }
    }
    let cell = area / n as f32;
    let focus = if focus.is_finite() { focus } else { Vec2::ZERO };
    let want = (focus / cell - Vec2::splat(n as f32 * 0.5)).round().as_ivec2();
    if let Some(p) = prev
        && !p.closed
        && p.n == n
        && (p.cell - cell).abs() <= cell * 1e-5
        && (want - p.origin).abs().max_element() <= (n / 8) as i32
    {
        return p;
    }
    SimWindow { origin: want, n, cell, closed: false }
}

/// Substeps for a frame of `dt` on `cell`-metre cells with waves running at
/// `depth` and water moving up to `max_speed`.
pub fn pipe_substeps(dt: f32, cell: f32, depth: f32, max_speed: f32) -> u32 {
    let c = (GRAVITY * depth.max(0.01)).sqrt() + max_speed.max(0.0);
    ((dt.max(0.0) * c / (COURANT * cell.max(1e-4))).ceil() as u32).clamp(1, MAX_SIM_SUBSTEPS)
}

/// Longest frame the substep cap keeps stable.
pub fn max_sim_dt(cell: f32, depth: f32, max_speed: f32) -> f32 {
    let c = (GRAVITY * depth.max(0.01)).sqrt() + max_speed.max(0.0);
    MAX_SIM_SUBSTEPS as f32 * COURANT * cell / c
}

/// Something occupying the water this frame (water-local XZ): a body's
/// footprint and how deep it sits, or a push (falling water) as a short
/// dent with water added.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaterDisplacer {
    pub center: Vec2,
    /// Half extents along `axis` and across it.
    pub half: Vec2,
    pub axis: Vec2,
    /// Thickness of the body within the water column (m).
    pub thickness: f32,
    /// Rounded (ellipsoid) rather than boxy.
    pub round: bool,
    /// Velocity relative to the water's base flow (m/s).
    pub velocity: Vec2,
    /// Water added under it (m/s of column).
    pub rate: f32,
    /// How much it drags the water toward its velocity (0..1).
    pub momentum: f32,
    /// A one-frame push rather than a body: its dent is not scaled by the
    /// displacement strength, and the dispersive layer takes it as an
    /// impulse instead of following its occupancy.
    pub impulse: bool,
}

impl WaterDisplacer {
    /// A box (its eight corners in water-local space, `axis` its local X
    /// there) in water whose surface is at local `surface`; only the part
    /// within `reach` below the surface counts. None when it is clear of it.
    pub fn from_corners(corners: &[Vec3; 8], axis: Vec3, surface: f32, reach: f32, velocity: Vec2, round: bool) -> Option<Self> {
        let c = corners.iter().copied().sum::<Vec3>() / 8.0;
        let ax = Vec2::new(axis.x, axis.z).try_normalize().unwrap_or(Vec2::X);
        let side = ax.perp();
        let mut half = Vec2::ZERO;
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for p in corners {
            let d = Vec2::new(p.x - c.x, p.z - c.z);
            half = half.max(Vec2::new(d.dot(ax).abs(), d.dot(side).abs()));
            lo = lo.min(p.y);
            hi = hi.max(p.y);
        }
        let thickness = (surface.min(hi) - lo.max(surface - reach.max(0.0))).max(0.0);
        if thickness <= 1e-4 || half.min_element() <= 1e-4 {
            return None;
        }
        Some(Self { center: Vec2::new(c.x, c.z), half, axis: ax, thickness, round, velocity, rate: 0.0, momentum: 1.0, impulse: false })
    }

    /// A push (falling water landing, a wake eddy) as a brief dent that adds
    /// a little water.
    pub fn from_push(p: &WaterPush) -> Self {
        let r = p.radius.max(0.05);
        Self {
            center: p.at,
            half: Vec2::splat(r),
            axis: Vec2::X,
            thickness: p.strength * PUSH_DENT,
            round: true,
            velocity: Vec2::ZERO,
            rate: p.strength.max(0.0) * PUSH_RATE,
            momentum: 0.0,
            impulse: true,
        }
    }

    /// Footprints under ~a cell grow to one, thinner, keeping their volume.
    pub fn fit_to_cell(mut self, cell: f32) -> Self {
        let min = Vec2::splat(cell * 0.75);
        let grown = self.half.max(min);
        let keep = (self.half.x * self.half.y) / (grown.x * grown.y).max(1e-8);
        self.thickness *= keep;
        self.rate *= keep;
        self.half = grown;
        self
    }

    /// Volume it occupies (m^3).
    pub fn volume(&self) -> f32 {
        let area = if self.round { std::f32::consts::PI * self.half.x * self.half.y * (2.0 / 3.0) } else { 4.0 * self.half.x * self.half.y };
        area * self.thickness
    }

    /// The occupancy profile (0..1) at local `p`, `cell` the grid spacing.
    /// Matches `water_sim.comp`'s splat.
    pub fn profile(&self, p: Vec2, cell: f32) -> f32 {
        let rel = p - self.center;
        let l = Vec2::new(rel.dot(self.axis), rel.dot(self.axis.perp())) / self.half.max(Vec2::splat(1e-4));
        if self.round {
            // A paraboloid of the ellipsoid's volume: its rim rises gently
            // where the ellipsoid's is vertical (a jolt per cell crossed).
            (4.0 / 3.0) * (1.0 - l.dot(l)).max(0.0)
        } else {
            let m = l.x.abs().max(l.y.abs());
            let soft = cell / self.half.min_element().max(1e-4);
            ((1.0 - m) / soft + 0.5).clamp(0.0, 1.0)
        }
    }

    /// The occupancy the solver splats at `p`: a body's profile softened
    /// over its neighbouring cells, so its edge slides smoothly across the
    /// grid as it moves instead of stepping a cell at a time (each step
    /// would start a ripple). Pushes stay sharp. Matches `water_sim.comp`.
    pub fn splat_profile(&self, p: Vec2, cell: f32) -> f32 {
        if self.impulse {
            return self.profile(p, cell);
        }
        let side = [Vec2::X, -Vec2::X, Vec2::Y, -Vec2::Y].iter().map(|o| self.profile(p + *o * cell, cell)).sum::<f32>();
        0.5 * self.profile(p, cell) + 0.125 * side
    }

    /// GPU layout (`Displacer` in water_sim.comp).
    pub fn to_gpu(&self) -> [f32; 12] {
        let shape = if self.round { 1.0 } else { 0.0 } + if self.impulse { 2.0 } else { 0.0 };
        [
            self.center.x, self.center.y, self.half.x, self.half.y,
            self.axis.x, self.axis.y, self.thickness, shape,
            self.velocity.x, self.velocity.y, self.rate, self.momentum,
        ]
    }
}

/// Per-cell input of a window: rest depth (m), base flow x/z (m/s, local)
/// and kind (`CELL_*`), row-major from the window's first cell.
#[derive(Clone, Debug, PartialEq)]
pub struct SimField {
    /// Bumped on every change.
    pub version: u64,
    pub window: SimWindow,
    pub cells: Arc<Vec<[f32; 4]>>,
    /// Some cell has a base flow (the solver then advects).
    pub flowing: bool,
    /// Deepest rest depth, and the depth cells outside the field take.
    pub max_depth: f32,
    pub default_depth: f32,
}

impl SimField {
    /// The field of `water` over `window`; `blocked` (row-major, same window)
    /// marks cells obstacles fill.
    pub fn build(water: &Water, window: SimWindow, blocked: Option<&[bool]>) -> Self {
        Self::build_with_bed(water, window, blocked, None)
    }

    /// The same over the ground: `bed` (row-major, same window) is the depth
    /// of the floor under each cell's rest surface (m; NaN = none found, the
    /// water's own depth). Ground just above the surface is a dry beach the
    /// water can run up (`RUNUP_HEIGHT`); higher ground is a bank.
    pub fn build_with_bed(water: &Water, window: SimWindow, blocked: Option<&[bool]>, bed: Option<&[f32]>) -> Self {
        let n = window.n as usize;
        let cols = water.columns_in(window.lo(), n, window.cell);
        let mut flowing = false;
        let mut max_depth = 0.0f32;
        let cells: Vec<[f32; 4]> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| match c {
                Some(c) if blocked.is_some_and(|b| b.get(i).copied().unwrap_or(false)) => [c.depth.max(0.05), 0.0, 0.0, CELL_OBSTACLE],
                Some(c) => {
                    let depth = bed.and_then(|b| b.get(i).copied()).filter(|d| d.is_finite()).unwrap_or(c.depth);
                    if depth < -RUNUP_HEIGHT {
                        return [0.05, 0.0, 0.0, CELL_SHORE];
                    }
                    if depth < MIN_WET_DEPTH {
                        // A beach: dry ground (negative rest depth) the
                        // water can run up onto.
                        return [depth.min(0.0), 0.0, 0.0, CELL_WATER];
                    }
                    flowing |= c.flow.x.abs() + c.flow.z.abs() > 1e-4;
                    max_depth = max_depth.max(depth);
                    [depth.max(0.05), c.flow.x, c.flow.z, CELL_WATER]
                }
                None => [0.05, 0.0, 0.0, CELL_SHORE],
            })
            .collect();
        let default_depth = if water.body == WaterBody::River { 0.05 } else { water.depth.max(0.05) };
        Self { version: 0, window, cells: Arc::new(cells), flowing, max_depth: max_depth.max(0.05), default_depth }
    }
}

/// What the engine hands the GPU simulation of one Water, kept on the
/// component between frames (not saved).
#[derive(Clone, Debug, Default)]
pub struct WaterSimFeed {
    /// The window the simulation runs over now.
    pub window: Option<SimWindow>,
    /// Bodies in the water at the latest physics step.
    pub displacers: Vec<WaterDisplacer>,
    /// Pushes since the last frame (falling water, eddies).
    pub pushes: Vec<WaterPush>,
    /// Cells obstacles fill (from physics), and the window it was made for.
    pub blocked: Option<(SimWindow, Arc<Vec<bool>>)>,
    /// Depth of the ground under each cell (`SimField::build_with_bed`),
    /// and the window it was made for.
    pub bed: Option<(SimWindow, Arc<Vec<f32>>)>,
    /// Last readback applied to `Water::ripples`.
    pub readback_serial: u64,
    /// Frames with nothing moving (the simulation idles past a while).
    pub quiet_frames: u32,
    /// Last frame's displacers (bodies held still let the water idle too).
    pub last_displacers: Vec<WaterDisplacer>,
    field_key: u64,
    field: Option<Arc<SimField>>,
    field_blocked: Option<Arc<Vec<bool>>>,
    field_bed: Option<Arc<Vec<f32>>>,
}

impl WaterSimFeed {
    /// The field for `window`, rebuilt when the window, the water's shape or
    /// the obstacles change; true when the water itself changed under the
    /// same window (the simulation then restarts from rest).
    pub fn field(&mut self, water: &Water, window: SimWindow) -> (Arc<SimField>, bool) {
        let key = field_key(water);
        let blocked = self.blocked.as_ref().filter(|(w, _)| *w == window).map(|(_, b)| b.clone());
        let same_window = self.field.as_ref().is_some_and(|f| f.window == window);
        let blocked_same = match (&blocked, &self.field_blocked) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        let bed = self.bed.as_ref().filter(|(w, _)| *w == window).map(|(_, b)| b.clone());
        let bed_same = match (&bed, &self.field_bed) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if let Some(f) = &self.field
            && same_window
            && key == self.field_key
            && blocked_same
            && bed_same
        {
            return (f.clone(), false);
        }
        let reset = same_window && key != self.field_key;
        let version = self.field.as_ref().map_or(1, |f| f.version + 1);
        let mut field = SimField::build_with_bed(water, window, blocked.as_deref().map(|b| b.as_slice()), bed.as_deref().map(|b| b.as_slice()));
        field.version = version;
        let field = Arc::new(field);
        self.field = Some(field.clone());
        self.field_key = key;
        self.field_blocked = blocked;
        self.field_bed = bed;
        (field, reset)
    }

    /// This frame's displacers for `cell`-metre cells (bodies first, then
    /// the queued pushes, at most `max`); the pushes are used up. The
    /// solver scales the bodies by `PipeParams::displacement`.
    pub fn take_displacers(&mut self, cell: f32, max: usize) -> Vec<WaterDisplacer> {
        let mut out: Vec<WaterDisplacer> = self.displacers.iter().map(|d| d.fit_to_cell(cell)).collect();
        for p in self.pushes.drain(..) {
            if out.len() >= max {
                break;
            }
            out.push(WaterDisplacer::from_push(&p).fit_to_cell(cell));
        }
        out.truncate(max);
        self.pushes.clear();
        out
    }
}

fn field_key(water: &Water) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (water.body as u8).hash(&mut h);
    water.smooth_shape.hash(&mut h);
    for v in water.shape.iter().flatten().chain(water.size.iter()).chain(water.current.iter()) {
        v.to_bits().hash(&mut h);
    }
    for p in &water.river {
        for v in p.position.iter().chain([p.width, p.depth, p.speed].iter()) {
            v.to_bits().hash(&mut h);
        }
    }
    water.depth.to_bits().hash(&mut h);
    h.finish()
}

/// Solver constants for a frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PipeParams {
    pub gravity: f32,
    /// Share of pipe flow lost per second.
    pub damping: f32,
    /// Rate (1/s) displacers drag the water toward their velocity.
    pub momentum: f32,
    /// Rate (1/s) the level eases back to rest.
    pub level_return: f32,
    /// Waves travel as if the water were at most this deep (m).
    pub wave_depth: f32,
    /// Scales the bodies' occupancy (not the pushes').
    pub displacement: f32,
    /// Kinematic viscosity (m^2/s): flow over the bed loses 3 nu / h^2 per
    /// second (laminar sheet flow).
    pub viscosity: f32,
    /// Eddy viscosity as a rate (1/s): nu_eddy = eddy * cell^2, so it acts
    /// on what the grid resolves (the shortest waves most).
    pub eddy: f32,
}

impl PipeParams {
    pub fn for_water(water: &Water) -> Self {
        Self {
            gravity: GRAVITY,
            damping: water.ripple_damping.clamp(0.0, 0.99),
            momentum: water.sim_momentum.max(0.0) * MOMENTUM_RATE,
            level_return: LEVEL_RETURN,
            wave_depth: water.sim_wave_depth.max(0.05),
            displacement: water.sim_displacement.max(0.0),
            viscosity: water.kinematic_viscosity(),
            eddy: water.sim_chop_smoothing.max(0.0) * EDDY_RATE,
        }
    }

    /// Per-substep pipe flow kept after damping.
    pub fn keep(&self, dt: f32) -> f32 {
        (1.0 - self.damping.clamp(0.0, 0.99)).powf(dt)
    }
}

/// The pipe model on the CPU, the same passes as `water_sim.comp`. Storage
/// is toroidal (cell (x, z) of the world lives at (x mod n, z mod n)), so
/// moving the window only resets the cells it newly covers.
#[derive(Clone, Debug)]
pub struct PipeSim {
    pub window: SimWindow,
    /// Column height (m), outflow pipes (-x, +x, -z, +z; m^3/s), velocity.
    pub h: Vec<f32>,
    pub flux: Vec<[f32; 4]>,
    pub vel: Vec<Vec2>,
    field: Vec<[f32; 4]>,
    field_origin: IVec2,
    default_depth: f32,
    flowing: bool,
    max_depth: f32,
    /// This frame's occupancy, drag weight and target velocity, and added
    /// water, per window cell.
    couple: Vec<[f32; 4]>,
    src: Vec<f32>,
    /// Body occupancy and one-frame dents per window cell (the dispersive
    /// layer's sources).
    ew_in: Vec<[f32; 2]>,
    /// The dispersive layer, when it runs ([`Self::set_ewave`]).
    ewave: Option<(EwaveParams, EwaveLayer)>,
    /// Window origin the state was last valid for, when the window moved
    /// since the last step (its new cells are reset after the next SPLAT).
    moved_from: Option<IVec2>,
}

const DIRS: [IVec2; 4] = [IVec2::new(-1, 0), IVec2::new(1, 0), IVec2::new(0, -1), IVec2::new(0, 1)];

impl PipeSim {
    pub fn new(field: &SimField) -> Self {
        let w = field.window;
        let cells = (w.n * w.n) as usize;
        let mut sim = Self {
            window: w,
            h: vec![0.0; cells],
            flux: vec![[0.0; 4]; cells],
            vel: vec![Vec2::ZERO; cells],
            field: Vec::new(),
            field_origin: w.origin,
            default_depth: 0.05,
            flowing: false,
            max_depth: 0.05,
            couple: vec![[0.0; 4]; cells],
            src: vec![0.0; cells],
            ew_in: vec![[0.0; 2]; cells],
            ewave: None,
            moved_from: None,
        };
        sim.set_field(field);
        sim.reset(w.origin + IVec2::splat(w.n as i32 * 4));
        sim
    }

    pub fn set_field(&mut self, field: &SimField) {
        self.field = field.cells.as_ref().clone();
        self.field_origin = field.window.origin;
        self.default_depth = field.default_depth;
        self.flowing = field.flowing;
        self.max_depth = field.max_depth;
    }

    /// Run the dispersive layer with `params` (None stops it; starting it
    /// starts from rest).
    pub fn set_ewave(&mut self, params: Option<EwaveParams>) {
        match (params, &mut self.ewave) {
            (None, _) => self.ewave = None,
            (Some(p), Some((q, _))) => *q = p,
            (Some(p), None) => self.ewave = Some((p, EwaveLayer::new(self.window.n as usize))),
        }
    }

    pub fn ewave(&self) -> Option<&EwaveLayer> {
        self.ewave.as_ref().map(|(_, l)| l)
    }

    /// Move to `window` (same size and cells), keeping the water already in
    /// it in place. The cells it newly covers start at rest with the next
    /// step's bodies in them.
    pub fn move_to(&mut self, window: SimWindow) {
        let prev = self.window.origin;
        self.window = window;
        self.moved_from.get_or_insert(prev);
    }

    fn n(&self) -> i32 {
        self.window.n as i32
    }

    fn inside(&self, w: IVec2) -> bool {
        w.x >= 0 && w.y >= 0 && w.x < self.n() && w.y < self.n()
    }

    fn store(&self, w: IVec2) -> usize {
        let n = self.n();
        let g = self.window.origin + w;
        (g.y.rem_euclid(n) * n + g.x.rem_euclid(n)) as usize
    }

    fn win(&self, w: IVec2) -> usize {
        (w.y * self.n() + w.x) as usize
    }

    fn field_at(&self, w: IVec2) -> [f32; 4] {
        let f = w + self.window.origin - self.field_origin;
        if self.inside(f) { self.field[self.win(f)] } else { [self.default_depth, 0.0, 0.0, CELL_WATER] }
    }

    /// How dry cell `w` is (0 wet .. 1 dry): only beach cells (ground above
    /// the rest surface) with less than a film of water on them.
    fn dryness(&self, w: IVec2) -> f32 {
        if !self.inside(w) || self.field_at(w)[0] > 0.0 {
            return 0.0;
        }
        let h = self.h[self.store(w)];
        1.0 - (h / DRY_FILM).clamp(0.0, 1.0)
    }

    fn solid(&self, w: IVec2) -> bool {
        if self.inside(w) { self.field_at(w)[3] > 0.5 } else { self.window.closed }
    }

    fn head(&self, w: IVec2) -> f32 {
        self.h[self.store(w)] + self.couple[self.win(w)][0] - self.field_at(w)[0]
    }

    fn sponge_at(&self, w: IVec2) -> f32 {
        let s = self.window.sponge();
        if s == 0 {
            return 0.0;
        }
        let e = w.x.min(w.y).min(self.n() - 1 - w.x).min(self.n() - 1 - w.y) as f32;
        let s = s as f32;
        if e < s { 0.25 * (1.0 - e / s).powi(2) } else { 0.0 }
    }

    fn cells(&self) -> impl Iterator<Item = IVec2> + use<> {
        let n = self.n();
        (0..n * n).map(move |i| IVec2::new(i % n, i / n))
    }

    /// RESET: cells whose world position was outside the window at `prev`
    /// go back to rest, around this frame's bodies (the column under a body
    /// already holds its occupancy, so a body the window moves onto is
    /// level with the water round it instead of a mound that collapses).
    fn reset(&mut self, prev: IVec2) {
        for w in self.cells() {
            let p = self.window.origin + w - prev;
            if !self.inside(p) {
                let s = self.store(w);
                self.h[s] = (self.field_at(w)[0] - self.couple[self.win(w)][0]).max(0.0);
                self.flux[s] = [0.0; 4];
                self.vel[s] = Vec2::ZERO;
                if let Some((_, l)) = &mut self.ewave {
                    l.reset_cell(s);
                }
            }
        }
    }

    /// SPLAT: this frame's occupancy and drag from the displacers (bodies
    /// scaled by `displacement`).
    fn splat(&mut self, displacers: &[WaterDisplacer], displacement: f32) {
        let cell = self.window.cell;
        for w in self.cells() {
            let p = (self.window.origin + w).as_vec2() * cell + Vec2::splat(cell * 0.5);
            let (mut b, mut wsum, mut v, mut src) = (0.0, 0.0, Vec2::ZERO, 0.0);
            let (mut body, mut dent) = (0.0, 0.0);
            for d in displacers {
                let prof = d.splat_profile(p, cell);
                if prof <= 0.0 {
                    continue;
                }
                if d.impulse {
                    dent += d.thickness * prof;
                } else {
                    b += d.thickness * prof * displacement;
                    body += d.thickness * prof;
                }
                src += d.rate * prof;
                // Only the part of the column the body fills is dragged along.
                let share = (d.thickness * prof / self.field_at(w)[0].max(0.05)).clamp(0.0, 1.0);
                let wt = prof * d.momentum * share;
                v += d.velocity * wt;
                wsum += wt;
            }
            let v = if wsum > 1e-5 { v / wsum } else { Vec2::ZERO };
            // Bodies fill at most the column there (see water_sim.comp).
            let depth = self.field_at(w)[0].max(0.0);
            let b = b.min(depth) + dent;
            let body = body.min(depth);
            let i = self.win(w);
            self.couple[i] = [b, wsum.min(1.0), v.x, v.y];
            self.src[i] = src;
            self.ew_in[i] = [body, dent];
        }
    }

    /// FLUX: update every cell's outflow pipes from the head differences.
    fn flux_pass(&mut self, dt: f32, keep: f32, p: &PipeParams) {
        let cell = self.window.cell;
        let mut out = self.flux.clone();
        for w in self.cells() {
            let s = self.store(w);
            if self.solid(w) {
                out[s] = [0.0; 4];
                continue;
            }
            let hs = self.h[s];
            let head = self.head(w);
            let mut f = self.flux[s].map(|x| x * keep);
            for (d, off) in DIRS.iter().enumerate() {
                let nb = w + *off;
                if self.solid(nb) {
                    f[d] = 0.0;
                    continue;
                }
                let (hn, nh) = if self.inside(nb) { (self.head(nb), self.h[self.store(nb)]) } else { (0.0, self.default_depth) };
                let face = (0.5 * (hs + nh)).clamp(0.0, p.wave_depth);
                f[d] = (f[d] + dt * p.gravity * face * (head - hn)).max(0.0);
                if p.viscosity > 0.0 {
                    let h = (0.5 * (hs + nh)).max(0.02);
                    f[d] *= (-p.viscosity * bed_friction(h, cell, p.gravity, p.viscosity) * dt).exp();
                }
            }
            let c = self.couple[self.win(w)];
            let mut dq = Vec2::ZERO;
            if c[1] > 0.0 && p.momentum > 0.0 {
                dq += (Vec2::new(c[2], c[3]) - self.vel[s]) * (c[1] * (p.momentum * dt).min(1.0) * cell * hs.clamp(0.01, p.wave_depth));
            }
            if p.viscosity > 0.0 || p.eddy > 0.0 {
                // Viscous shallow water: momentum spreads to the neighbours
                // (nu laplacian u). Walls hold the water still (no slip);
                // past the window it carries on as it is.
                let u = self.vel[s];
                let around: Vec2 = DIRS
                    .iter()
                    .map(|o| {
                        let nb = w + *o;
                        if !self.inside(nb) {
                            u
                        } else if self.solid(nb) {
                            Vec2::ZERO
                        } else {
                            self.vel[self.store(nb)]
                        }
                    })
                    .sum();
                let rate = (p.viscosity * dt / (cell * cell) + p.eddy * dt).min(VISCOUS_STEP_MAX);
                dq += (around - 4.0 * u) * (rate * cell * hs.clamp(0.01, p.wave_depth));
            }
            if dq != Vec2::ZERO {
                if dq.x > 0.0 {
                    if !self.solid(w + DIRS[1]) {
                        f[1] += dq.x;
                    }
                } else if !self.solid(w + DIRS[0]) {
                    f[0] -= dq.x;
                }
                if dq.y > 0.0 {
                    if !self.solid(w + DIRS[3]) {
                        f[3] += dq.y;
                    }
                } else if !self.solid(w + DIRS[2]) {
                    f[2] -= dq.y;
                }
            }
            let sp = 1.0 - self.sponge_at(w);
            let total = (f[0] + f[1] + f[2] + f[3]) * sp * dt;
            let k = if total > 0.0 { (hs * cell * cell / total).min(1.0) } else { 1.0 };
            out[s] = f.map(|x| x * sp * k);
        }
        self.flux = out;
    }

    /// HEIGHT: move the water through the pipes, add sources, update the
    /// velocity, push back from obstacles in a base flow, and relax toward
    /// rest at the border.
    fn height_pass(&mut self, dt: f32, p: &PipeParams) {
        let cell = self.window.cell;
        for w in self.cells() {
            let s = self.store(w);
            let fl = self.field_at(w);
            if self.solid(w) {
                self.h[s] = fl[0];
                self.vel[s] = Vec2::ZERO;
                continue;
            }
            let f = self.flux[s];
            let inflow = |d: usize, from: usize| {
                let nb = w + DIRS[d];
                if self.inside(nb) && !self.solid(nb) { self.flux[self.store(nb)][from] } else { 0.0 }
            };
            let (il, ir, ib, it) = (inflow(0, 1), inflow(1, 0), inflow(2, 3), inflow(3, 2));
            let h_old = self.h[s];
            let c = self.couple[self.win(w)];
            let mut hn = h_old + dt * ((il + ir + ib + it) - (f[0] + f[1] + f[2] + f[3])) / (cell * cell);
            hn += self.src[self.win(w)] * dt;
            let dw = Vec2::new(il - f[0] + f[1] - ir, ib - f[2] + f[3] - it) * 0.5;
            let havg = (0.5 * (h_old + hn)).clamp(0.05, p.wave_depth);
            let mut u = dw / (cell * havg);
            if u.length() > MAX_SPEED {
                u *= MAX_SPEED / u.length();
            }
            let base = Vec2::new(fl[1], fl[2]);
            if base != Vec2::ZERO {
                // A base flow into an obstacle piles water up in front of it
                // and draws it down behind (capped near the stagnation head).
                let eta = h_old + c[0] - fl[0];
                for off in DIRS {
                    let nb = w + off;
                    if !self.inside(nb) || self.field_at(nb)[3] < 1.5 {
                        continue;
                    }
                    let un = base.dot(off.as_vec2());
                    let cap = un * un / p.gravity;
                    if (un > 0.0 && eta < cap) || (un < 0.0 && eta > -cap) {
                        hn += dt * un * h_old.clamp(0.0, p.wave_depth) / cell;
                    }
                }
            }
            let target = (fl[0] - c[0]).max(0.0);
            let relax = (p.level_return * dt + self.sponge_at(w)).min(1.0);
            hn += (target - hn) * relax;
            self.h[s] = hn.max(0.0);
            self.vel[s] = u;
        }
    }

    /// ADVECT: carry the disturbance along the base flow (semi-Lagrangian).
    fn advect(&mut self, dt: f32) {
        let cell = self.window.cell;
        let mut h = self.h.clone();
        let mut flux = self.flux.clone();
        for w in self.cells() {
            let s = self.store(w);
            let fl = self.field_at(w);
            if self.solid(w) || fl[1] * fl[1] + fl[2] * fl[2] < 1e-8 {
                continue;
            }
            let pos = w.as_vec2() - Vec2::new(fl[1], fl[2]) * dt / cell;
            let i0 = pos.floor().as_ivec2();
            let t = pos - i0.as_vec2();
            let (mut eta, mut fx) = (0.0, [0.0f32; 4]);
            for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let c = i0 + IVec2::new(dx, dz);
                let wt = if dx == 0 { 1.0 - t.x } else { t.x } * if dz == 0 { 1.0 - t.y } else { t.y };
                let (e, f) = if self.inside(c) && !self.solid(c) {
                    let sc = self.store(c);
                    (self.h[sc] - self.field_at(c)[0], self.flux[sc])
                } else if self.solid(c) && self.inside(c) {
                    (self.h[s] - fl[0], self.flux[s])
                } else {
                    (0.0, [0.0; 4])
                };
                eta += wt * e;
                for k in 0..4 {
                    fx[k] += wt * f[k];
                }
            }
            h[s] = (fl[0] + eta).max(0.0);
            flux[s] = fx;
        }
        self.h = h;
        self.flux = flux;
    }

    /// SOURCE: the dispersive layer takes this frame's occupancy change
    /// and dents (only remembered when not `stepping`).
    fn ewave_source(&mut self, dt: f32) {
        let stepping = dt > 0.0;
        let Some((p, mut layer)) = self.ewave.take() else { return };
        for w in self.cells() {
            let i = self.win(w);
            layer.inject(self.store(w), self.ew_in[i][0], self.ew_in[i][1], &p, p.follow(dt), stepping && !self.solid(w));
        }
        self.ewave = Some((p, layer));
    }

    /// The dispersive layer's step, then solid cells to rest and the
    /// sponge (MASK).
    fn ewave_step(&mut self, dt: f32) {
        let Some((p, mut layer)) = self.ewave.take() else { return };
        layer.propagate(dt, self.window.cell, &p);
        let sponge = self.window.sponge();
        for w in self.cells() {
            // By the smoothed footprint the source follows, so what a body
            // pushes in ahead and lets go behind are blocked alike.
            let block = (layer.smoothed(self.store(w)) / crate::water_ewave::BLOCK_DRAFT).max(self.dryness(w));
            layer.mask_cell(self.store(w), self.solid(w), sponge_depth(w, self.n(), sponge), block, dt);
        }
        self.ewave = Some((p, layer));
    }

    /// Advance by `dt` with this frame's displacers.
    pub fn step(&mut self, dt: f32, p: &PipeParams, displacers: &[WaterDisplacer]) {
        self.splat(displacers, p.displacement);
        if let Some(prev) = self.moved_from.take() {
            self.reset(prev);
        }
        self.ewave_source(dt);
        if dt <= 0.0 {
            return;
        }
        let max_speed = self.vel.iter().map(|v| v.length()).fold(0.0, f32::max);
        let max_eta = self.cells().map(|w| (self.h[self.store(w)] - self.field_at(w)[0]).abs()).fold(0.0, f32::max);
        let depth = self.max_depth.min(p.wave_depth) + max_eta;
        let dt = dt.min(max_sim_dt(self.window.cell, depth, max_speed));
        if self.flowing {
            self.advect(dt);
        }
        let steps = pipe_substeps(dt, self.window.cell, depth, max_speed);
        let sub = dt / steps as f32;
        let keep = p.keep(sub);
        for _ in 0..steps {
            self.flux_pass(sub, keep, p);
            self.height_pass(sub, p);
        }
        self.ewave_step(dt);
    }

    /// Surface offset from rest at window cell `w` (bodies included).
    fn eta(&self, w: IVec2) -> f32 {
        if self.solid(w) {
            return 0.0;
        }
        self.h[self.store(w)] + self.couple[self.win(w)][0] - self.field_at(w)[0]
    }

    /// OUTPUT: per window cell (height offset, d/dx, d/dz, speed) as the
    /// ripple texture holds it, and (height offset, velocity x, z, drag
    /// weight) as the readback does. The dispersive layer adds its height,
    /// slope and orbital velocity here; other surface layers would too.
    pub fn output(&self) -> (Vec<[f32; 4]>, Vec<[f32; 4]>) {
        let cell = self.window.cell;
        let mut tex = Vec::with_capacity(self.couple.len());
        let mut back = Vec::with_capacity(self.couple.len());
        for w in self.cells() {
            let mut e = self.eta(w);
            let at = |nb: IVec2| if self.inside(nb) && !self.solid(nb) { self.eta(nb) } else { e };
            let mut g = Vec2::new(at(w + DIRS[1]) - at(w + DIRS[0]), at(w + DIRS[3]) - at(w + DIRS[2])) / (2.0 * cell);
            let mut u = if self.solid(w) { Vec2::ZERO } else { self.vel[self.store(w)] };
            if let Some((ep, l)) = &self.ewave
                && !self.solid(w)
            {
                let m = ep.mix;
                let c = l.state[self.store(w)];
                let at = |nb: IVec2| if self.inside(nb) && !self.solid(nb) { l.state[self.store(nb)] } else { c };
                let (x0, x1, z0, z1) = (at(w + DIRS[0]), at(w + DIRS[1]), at(w + DIRS[2]), at(w + DIRS[3]));
                e += c[0] * m;
                g += Vec2::new(x1[0] - x0[0], z1[0] - z0[0]) * m / (2.0 * cell);
                u += Vec2::new(x1[1] - x0[1], z1[1] - z0[1]).clamp_length_max(MAX_ORBITAL * 2.0 * cell) * m / (2.0 * cell);
            }
            // Inside a body's footprint there is no free surface (see
            // water_sim.comp): drawn flattened, the readback keeps it.
            let inside_body = (self.ew_in[self.win(w)][0] / crate::water_ewave::BLOCK_DRAFT).clamp(0.0, 1.0);
            // A dry beach: the surface sinks under the ground there, for
            // drawing and for what floats on it.
            let e = e - DRY_SINK * self.dryness(w);
            let shown = if e > 0.0 { e * (1.0 - inside_body) } else { e };
            tex.push([shown, g.x * (1.0 - inside_body), g.y * (1.0 - inside_body), u.length()]);
            back.push([e, u.x, u.y, self.couple[self.win(w)][1]]);
        }
        (tex, back)
    }

    /// Surface offset at window cell (`x`, `z`).
    pub fn height(&self, x: i32, z: i32) -> f32 {
        self.eta(IVec2::new(x, z))
    }

    pub fn velocity(&self, x: i32, z: i32) -> Vec2 {
        self.vel[self.store(IVec2::new(x, z))]
    }

    /// Water held by the window's open cells (m^3).
    pub fn volume(&self) -> f64 {
        let a = (self.window.cell * self.window.cell) as f64;
        self.cells().filter(|w| !self.solid(*w)).map(|w| self.h[self.store(w)] as f64 * a).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed reference setup for tests of one mechanism, so they don't
    /// shift with the defaults (which are tuned for looks).
    fn reference(w: Water) -> Water {
        Water {
            ripple_damping: 0.35,
            sim_wave_depth: 3.0,
            sim_displacement: 1.0,
            sim_momentum: 1.0,
            sim_ripple_mix: 1.0,
            sim_chop_smoothing: 0.3,
            ..w
        }
    }

    fn tank(n: u32, size: f32, depth: f32) -> (Water, SimField) {
        let water = reference(Water { size: [size, size], depth, waves: Vec::new(), ripple_resolution: n, ripple_area: size * 2.0, ..Default::default() });
        let window = choose_window(&water, Vec2::ZERO, None);
        assert!(window.closed);
        let field = SimField::build(&water, window, None);
        (water, field)
    }

    fn quiet(p: PipeParams) -> PipeParams {
        PipeParams { damping: 0.0, level_return: 0.0, ..p }
    }

    fn bump(sim: &mut PipeSim, at: IVec2, r: i32, height: f32) {
        for z in -r..=r {
            for x in -r..=r {
                let d = ((x * x + z * z) as f32).sqrt() / r as f32;
                if d < 1.0 {
                    let s = sim.store(at + IVec2::new(x, z));
                    sim.h[s] += height * (1.0 - d * d);
                }
            }
        }
    }

    fn center(sim: &PipeSim) -> IVec2 {
        IVec2::splat(sim.window.n as i32 / 2)
    }

    #[test]
    fn a_closed_tank_keeps_its_water() {
        let (water, field) = tank(64, 8.0, 1.0);
        let p = quiet(PipeParams::for_water(&water));
        let mut sim = PipeSim::new(&field);
        let c = center(&sim);
        bump(&mut sim, c, 5, 0.3);
        let before = sim.volume();
        let mover = WaterDisplacer {
            center: Vec2::new(-1.0, 0.0),
            half: Vec2::splat(0.4),
            axis: Vec2::X,
            thickness: 0.3,
            round: false,
            velocity: Vec2::new(1.0, 0.0),
            rate: 0.0,
            momentum: 1.0,
            impulse: false,
        };
        for k in 0..600 {
            let d = WaterDisplacer { center: mover.center + Vec2::new(k as f32 * 0.003, 0.0), ..mover };
            let list = if k < 400 { vec![d] } else { Vec::new() };
            sim.step(1.0 / 90.0, &p, &list);
        }
        let after = sim.volume();
        assert!(((after - before) / before).abs() < 1e-4, "volume {before} -> {after}");
        assert!(sim.h.iter().all(|h| h.is_finite() && *h >= 0.0));
    }

    #[test]
    fn a_dropped_bump_spreads_out() {
        let (water, field) = tank(96, 12.0, 1.0);
        let p = quiet(PipeParams::for_water(&water));
        let mut sim = PipeSim::new(&field);
        let c = center(&sim);
        bump(&mut sim, c, 4, 0.2);
        let peak = sim.height(c.x, c.y);
        for _ in 0..20 {
            sim.step(1.0 / 90.0, &p, &[]);
        }
        let mid = sim.height(c.x, c.y);
        assert!(mid < peak * 0.6, "the bump collapsed: {peak} -> {mid}");
        let ring: f32 = (0..32)
            .map(|k| {
                let a = k as f32 / 32.0 * std::f32::consts::TAU;
                let q = c + (Vec2::new(a.cos(), a.sin()) * 8.0).round().as_ivec2();
                sim.height(q.x, q.y)
            })
            .sum::<f32>()
            / 32.0;
        assert!(ring > 0.002, "a raised ring travelled out: {ring}");
        let speed = sim.vel.iter().map(|v| v.length()).fold(0.0, f32::max);
        assert!(speed > 0.01, "the water moves: {speed}");
    }

    #[test]
    fn waves_reflect_off_a_wall_and_never_pass_it() {
        let run = |wall: bool| {
            let (water, mut field) = tank(64, 8.0, 1.0);
            if wall {
                for z in 0..64 {
                    Arc::make_mut(&mut field.cells)[z * 64 + 44][3] = CELL_OBSTACLE;
                }
            }
            let p = quiet(PipeParams::for_water(&water));
            let mut sim = PipeSim::new(&field);
            let c = center(&sim);
            bump(&mut sim, c, 3, 0.2);
            let mut behind = 0.0f32;
            for _ in 0..60 {
                sim.step(1.0 / 90.0, &p, &[]);
                for z in 0..64 {
                    behind = behind.max(sim.height(50, z).abs());
                }
            }
            (sim.height(40, c.y), behind)
        };
        let (open_near, open_behind) = run(false);
        let (walled_near, walled_behind) = run(true);
        assert!(open_behind > 1e-3, "without a wall the wave gets there: {open_behind}");
        assert!(walled_behind < 1e-6, "nothing passes the wall: {walled_behind}");
        assert!((walled_near - open_near).abs() > 1e-3, "the reflection changes the water before it: {walled_near} vs {open_near}");
    }

    #[test]
    fn a_moving_body_raises_a_bow_and_leaves_a_trough() {
        let (water, field) = tank(96, 12.0, 1.0);
        let p = PipeParams::for_water(&water);
        let mut sim = PipeSim::new(&field);
        let speed = 1.5;
        let mut x = -3.0;
        for _ in 0..90 {
            x += speed / 90.0;
            let d = WaterDisplacer {
                center: Vec2::new(x, 0.0),
                half: Vec2::new(0.6, 0.3),
                axis: Vec2::X,
                thickness: 0.3,
                round: true,
                velocity: Vec2::new(speed, 0.0),
                rate: 0.0,
                momentum: 1.0,
                impulse: false,
            };
            sim.step(1.0 / 90.0, &p, &[d]);
        }
        let cell = sim.window.cell;
        let at = |lx: f32| {
            let w = (Vec2::new(lx, 0.0) / cell).floor().as_ivec2() - sim.window.origin;
            sim.height(w.x, w.y)
        };
        let bow = at(x + 0.8);
        let trough = at(x - 0.9);
        assert!(bow > 0.005, "water piles up ahead: {bow}");
        assert!(trough < -0.002, "and drops behind: {trough}");
        let w = (Vec2::new(x, 0.0) / cell).floor().as_ivec2() - sim.window.origin;
        assert!(sim.velocity(w.x, w.y).x < speed * 0.25, "the water under it isn't shoved along with it: {}", sim.velocity(w.x, w.y));
    }

    /// Highest water (simulation plus dispersive layer) ahead of a body
    /// pushed along +X for `secs` (the second half, past the start-up).
    fn bow(speed: f32, thickness: f32, depth: f32, secs: f32, momentum: f32, dispersion: f32) -> f32 {
        let (mut water, field) = tank(128, 16.0, depth);
        water.sim_dispersion = dispersion;
        water.sim_momentum = momentum;
        let p = PipeParams::for_water(&water);
        let mut sim = PipeSim::new(&field);
        sim.set_ewave(EwaveParams::for_water(&water, sim.window, depth));
        let mut x = -4.0;
        let mut peak = 0.0f32;
        let steps = (secs * 90.0) as usize;
        for step in 0..steps {
            x += speed / 90.0;
            let d = WaterDisplacer {
                center: Vec2::new(x, 0.0),
                half: Vec2::new(0.6, 0.4),
                axis: Vec2::X,
                thickness,
                round: true,
                velocity: Vec2::new(speed, 0.0),
                rate: 0.0,
                momentum: 1.0,
                impulse: false,
            };
            sim.step(1.0 / 90.0, &p, &[d]);
            if step < steps / 2 {
                continue;
            }
            let cell = sim.window.cell;
            for k in 0..10 {
                let w = (Vec2::new(x + 0.6 + k as f32 * cell, 0.0) / cell).floor().as_ivec2() - sim.window.origin;
                let e = sim.height(w.x, w.y) + sim.ewave().map_or(0.0, |l| l.state[sim.store(w)][0]);
                peak = peak.max(e);
            }
        }
        peak
    }

    #[test]
    fn a_slow_body_lifts_only_a_small_bow() {
        // About the v^2 / 2g of a real hull, not a bulge the body's own
        // depth: the water under it isn't shoved along with it.
        let b = bow(1.0, 0.3, 1.0, 3.0, 1.0, 1.0);
        assert!(b < 0.09, "1 m/s in 1 m of water: {b}");
        let b = bow(0.5, 0.3, 3.0, 3.0, 1.0, 1.0);
        assert!(b < 0.03, "0.5 m/s in 3 m of water: {b}");
    }

    /// Grid-scale roughness of the dispersive layer (each cell against its
    /// neighbours' mean, squared and summed) after a body crossed it, and
    /// its total energy (heights squared, summed).
    fn checker(smoothing: f32, fine_damping: f32) -> (f32, f32) {
        let (mut water, field) = tank(128, 16.0, 2.0);
        water.sim_ripple_smoothing = smoothing;
        water.sim_fine_damping = fine_damping;
        water.sim_wake_ripples = 0.35;
        let p = PipeParams::for_water(&water);
        let mut sim = PipeSim::new(&field);
        sim.set_ewave(EwaveParams::for_water(&water, sim.window, 2.0));
        let mut x = -4.0;
        for _ in 0..270 {
            // A frame-locked step: the body moves once a frame, the water
            // takes three substeps.
            x += 1.0 / 90.0;
            let d = WaterDisplacer { center: Vec2::new(x, 0.0), half: Vec2::splat(0.4), axis: Vec2::X, thickness: 0.3, round: true, velocity: Vec2::new(1.0, 0.0), rate: 0.0, momentum: 1.0, impulse: false };
            for _ in 0..3 {
                sim.step(1.0 / 270.0, &p, &[d]);
            }
        }
        let layer = sim.ewave().unwrap();
        let n = sim.window.n as i32;
        let h = |x: i32, z: i32| layer.state[sim.store(IVec2::new(x.rem_euclid(n), z.rem_euclid(n)))][0];
        let (mut sum, mut all) = (0.0f32, 0.0f32);
        for z in 0..n {
            for x in 0..n {
                let r = h(x, z) - 0.25 * (h(x + 1, z) + h(x - 1, z) + h(x, z + 1) + h(x, z - 1));
                sum += r * r;
                all += h(x, z) * h(x, z);
            }
        }
        (sum, all)
    }

    #[test]
    fn smoothing_and_fine_damping_clear_the_grid_checker() {
        // Smoothing alone must not trade the checker for more waves overall.
        let (raw, raw_all) = checker(0.0, 1.5);
        let (smooth, smooth_all) = checker(0.06, 1.5);
        let (tuned, _) = checker(0.06, 6.0);
        assert!(smooth < raw * 0.5 && smooth_all < raw_all, "smoothing: {smooth} vs {raw} (energy {smooth_all} vs {raw_all})");
        assert!(tuned < raw * 0.25, "smoothing and fine damping: {tuned} vs {raw}");
    }

    /// The water's motion: surface velocity squared, summed (the readback,
    /// simulation plus dispersive layer). Thick fluid barely moves; a dent
    /// in it just slowly flattens.
    fn motion(back: &[[f32; 4]]) -> f32 {
        back.iter().map(|b| b[1] * b[1] + b[2] * b[2]).sum()
    }

    /// The motion (`motion`) left `secs` after a push into a fluid of
    /// `preset`.
    fn ringing(preset: &str, secs: f32) -> f32 {
        let (mut water, field) = tank(128, 16.0, 1.5);
        crate::FLUID_PRESETS.iter().find(|f| f.name == preset).unwrap().apply(&mut water);
        let p = PipeParams::for_water(&water);
        let mut sim = PipeSim::new(&field);
        sim.set_ewave(EwaveParams::for_water(&water, sim.window, 1.5));
        let push = WaterDisplacer::from_push(&WaterPush { at: Vec2::ZERO, radius: 0.6, strength: 6.0 });
        let steps = (secs * 90.0) as usize;
        for k in 0..steps {
            let list = if k < 3 { vec![push] } else { Vec::new() };
            sim.step(1.0 / 90.0, &p, &list);
        }
        let (_, back) = sim.output();
        motion(&back)
    }

    #[test]
    fn the_ripple_mix_scales_the_dispersive_layer_and_zero_turns_it_off() {
        let (mut water, _) = tank(64, 8.0, 2.0);
        let window = choose_window(&water, Vec2::ZERO, None);
        water.sim_ripple_mix = 0.0;
        assert!(EwaveParams::for_water(&water, window, 2.0).is_none(), "off at 0");
        let surface = |mix: f32| {
            let (mut water, field) = tank(64, 8.0, 2.0);
            water.sim_ripple_mix = mix;
            let p = PipeParams::for_water(&water);
            let mut plain = PipeSim::new(&field);
            let mut sim = PipeSim::new(&field);
            sim.set_ewave(EwaveParams::for_water(&water, sim.window, 2.0));
            let push = WaterDisplacer::from_push(&WaterPush { at: Vec2::ZERO, radius: 0.5, strength: 6.0 });
            for k in 0..60 {
                let list = if k == 0 { vec![push] } else { Vec::new() };
                plain.step(1.0 / 90.0, &p, &list);
                sim.step(1.0 / 90.0, &p, &list);
            }
            let (a, _) = plain.output();
            let (b, _) = sim.output();
            a.iter().zip(&b).map(|(x, y)| (y[0] - x[0]).abs()).sum::<f32>()
        };
        let (full, half) = (surface(1.0), surface(0.5));
        assert!(full > 1e-3 && (half / full - 0.5).abs() < 0.05, "half the ripples show: {half} vs {full}");
    }

    /// Grid-scale roughness of the simulation (not the dispersive layer)
    /// `secs` after bodies churned it, at `smoothing`.
    fn chop(smoothing: f32, secs: f32) -> (f32, f32) {
        let (mut water, field) = tank(96, 12.0, 1.0);
        water.sim_chop_smoothing = smoothing;
        water.sim_dispersion = 0.0;
        let p = PipeParams::for_water(&water);
        let mut sim = PipeSim::new(&field);
        let mut x = -3.0;
        for k in 0..(secs * 90.0) as usize {
            let list: Vec<WaterDisplacer> = if k < 90 {
                x += 2.0 / 90.0;
                vec![WaterDisplacer { center: Vec2::new(x, 0.0), half: Vec2::splat(0.4), axis: Vec2::X, thickness: 0.3, round: false, velocity: Vec2::new(2.0, 0.0), rate: 0.0, momentum: 1.0, impulse: false }]
            } else {
                Vec::new()
            };
            sim.step(1.0 / 90.0, &p, &list);
        }
        let n = sim.window.n as i32;
        let h = |x: i32, z: i32| sim.height(x.clamp(0, n - 1), z.clamp(0, n - 1));
        let (mut rough, mut all) = (0.0f32, 0.0f32);
        for z in 0..n {
            for x in 0..n {
                let r = h(x, z) - 0.25 * (h(x + 1, z) + h(x - 1, z) + h(x, z + 1) + h(x, z - 1));
                rough += r * r;
                all += h(x, z) * h(x, z);
            }
        }
        (rough, all)
    }

    #[test]
    fn chop_smoothing_drains_short_waves_more_than_long_ones() {
        let (raw_rough, raw_all) = chop(0.0, 3.0);
        let (smooth_rough, smooth_all) = chop(0.6, 3.0);
        assert!(smooth_rough < raw_rough * 0.5, "short chop drained: {smooth_rough} vs {raw_rough}");
        // Long swells (the bulk of the energy) are kept far better.
        assert!(smooth_rough / smooth_all < raw_rough / raw_all, "the short share falls");
    }

    /// `ringing` on a pool like the user's (25 cm cells, 1.1 m deep) with the
    /// default settings.
    fn pool_ringing(preset: &str, scale: f32, secs: f32) -> f32 {
        let mut water = Water { size: [10.8, 10.4], depth: 1.1, waves: Vec::new(), ripple_resolution: 128, ripple_area: 32.0, ..Default::default() };
        crate::FLUID_PRESETS.iter().find(|f| f.name == preset).unwrap().apply(&mut water);
        water.fluid_scale = scale;
        let window = choose_window(&water, Vec2::ZERO, None);
        let field = SimField::build(&water, window, None);
        let p = PipeParams::for_water(&water);
        let mut sim = PipeSim::new(&field);
        sim.set_ewave(EwaveParams::for_water(&water, window, 1.1));
        let push = WaterDisplacer::from_push(&WaterPush { at: Vec2::ZERO, radius: 0.6, strength: 6.0 });
        for k in 0..(secs * 90.0) as usize {
            let list = if k < 3 { vec![push] } else { Vec::new() };
            sim.step(1.0 / 90.0, &p, &list);
        }
        let (_, back) = sim.output();
        motion(&back)
    }

    #[test]
    fn honey_staged_at_jar_scale_is_thick_in_a_pool() {
        let water = pool_ringing("Water", 1.0, 2.0);
        let honey = pool_ringing("Honey", 1.0, 2.0);
        let staged = pool_ringing("Honey", 50.0, 2.0);
        assert!(staged < water * 0.1 && staged < honey * 0.2, "1:50 honey settles like a jar's: {staged} vs honey {honey}, water {water}");
    }

    #[test]
    fn thick_fluids_keep_only_long_waves() {
        let water = Water::default();
        let mut honey = Water::default();
        crate::FLUID_PRESETS.iter().find(|f| f.name == "Honey").unwrap().apply(&mut honey);
        let k = |l: f32| std::f32::consts::TAU / l;
        for l in [0.05, 0.5, 8.0] {
            assert!(water.wave_share(k(l)) > 0.9, "water keeps {l} m waves");
        }
        assert!(honey.wave_share(k(0.15)) == 0.0, "no fine detail on honey");
        assert!(honey.wave_share(k(8.0)) > 0.5, "long waves survive even honey");
        honey.fluid_scale = 50.0;
        assert!(honey.wave_share(k(8.0)) < 0.05, "staged as a jar: none");
    }

    #[test]
    fn thick_fluids_do_not_splash() {
        let mut w = Water::default();
        assert!(w.splashiness() > 0.99, "water splashes freely: {}", w.splashiness());
        crate::FLUID_PRESETS.iter().find(|f| f.name == "Honey").unwrap().apply(&mut w);
        assert!(w.splashiness() < 0.15, "honey hardly: {}", w.splashiness());
    }

    #[test]
    fn thicker_fluids_settle_faster() {
        let (water, honey, mud) = (ringing("Water", 3.0), ringing("Honey", 3.0), ringing("Mud", 3.0));
        assert!(honey < water * 0.5, "honey settles: {honey} vs water {water}");
        assert!(mud < honey * 0.7, "mud settles faster still: {mud} vs honey {honey}");
    }

    #[test]
    fn waves_run_up_a_beach_and_drain_back() {
        // A beach rising from 1 m deep at x = -4 to 0.5 m above the water at
        // x = 4 (16 m tank), and a surge sent up it.
        let water = reference(Water { size: [16.0, 16.0], depth: 1.0, waves: Vec::new(), ripple_resolution: 64, ripple_area: 32.0, sim_dispersion: 0.0, ..Default::default() });
        let window = choose_window(&water, Vec2::ZERO, None);
        let n = window.n as usize;
        let bed: Vec<f32> = (0..n * n)
            .map(|i| {
                let x = window.lo().x + ((i % n) as f32 + 0.5) * window.cell;
                if (-4.0..=4.0).contains(&x) { 1.0 - (x + 4.0) / 8.0 * 1.5 } else if x < -4.0 { 1.0 } else { -0.5 }
            })
            .collect();
        let field = SimField::build_with_bed(&water, window, None, Some(&bed));
        let column = |x: f32| {
            let c = ((Vec2::new(x, 0.0) - window.lo()) / window.cell).floor().as_ivec2();
            (c.y as usize * n + c.x as usize, c)
        };
        let (land, _) = column(2.0);
        assert!(field.cells[land][0] < 0.0 && field.cells[land][3] == CELL_WATER, "dry beach, not a wall: {:?}", field.cells[land]);
        let p = PipeParams::for_water(&water);
        let mut sim = PipeSim::new(&field);
        // A dam break: the deep end raised 30 cm, let go.
        for z in 0..n {
            for x in 0..n {
                if window.lo().x + (x as f32 + 0.5) * window.cell < -5.0 {
                    let st = sim.store(IVec2::new(x as i32, z as i32));
                    sim.h[st] += 0.3;
                }
            }
        }
        let (_, at) = column(2.0);
        let wet = |sim: &PipeSim| sim.h[sim.store(at)];
        let mut most = 0.0f32;
        for _ in 0..900 {
            sim.step(1.0 / 90.0, &p, &[]);
            most = most.max(wet(&sim));
        }
        assert!(most > 0.01, "the swell ran up onto ground 0.125 m above the water: {most}");
        assert!(wet(&sim) < most * 0.5, "and drained back: {} of {most}", wet(&sim));
    }

    #[test]
    fn bodies_deeper_than_the_water_do_not_raise_a_spike() {
        // A ball resting on a couch in shallow water: together they are
        // thicker than the column, which holds no more than its depth.
        let (water, field) = tank(64, 8.0, 0.3);
        let p = PipeParams::for_water(&water);
        let mut sim = PipeSim::new(&field);
        let couch = WaterDisplacer { center: Vec2::ZERO, half: Vec2::new(1.5, 0.8), axis: Vec2::X, thickness: 0.3, round: false, velocity: Vec2::ZERO, rate: 0.0, momentum: 1.0, impulse: false };
        let ball = WaterDisplacer { center: Vec2::ZERO, half: Vec2::splat(0.5), axis: Vec2::X, thickness: 0.6, round: true, ..couch };
        // Dropped in at once they push a wave out; it must drain away, not
        // stand over them (an empty column under them can't let it go).
        for _ in 0..360 {
            sim.step(1.0 / 90.0, &p, &[couch, ball]);
        }
        let (tex, back) = sim.output();
        let peak = tex.iter().map(|t| t[0]).fold(0.0, f32::max).max(back.iter().map(|t| t[0]).fold(0.0, f32::max));
        assert!(peak < 0.08, "no standing spike: {peak}");
    }

    #[test]
    fn a_body_slides_across_the_grid_without_stepping() {
        // The occupancy a cell sees changes in small steps as a round body
        // moves past it, not the jump an ellipsoid's vertical rim gives as
        // it arrives (one ripple per cell crossed).
        let cell = 0.25;
        let jump = |sharp: bool| {
            let mut last: Option<f32> = None;
            let mut most = 0.0f32;
            for k in 0..200 {
                let d = WaterDisplacer { center: Vec2::new(-1.0 + k as f32 * 0.01, 0.0), half: Vec2::splat(0.5), axis: Vec2::X, thickness: 1.0, round: true, velocity: Vec2::ZERO, rate: 0.0, momentum: 1.0, impulse: false };
                let at = Vec2::new(cell * 0.5, cell * 0.5);
                let l = (at - d.center) / d.half;
                let occ = if sharp { (1.0 - l.dot(l)).max(0.0).sqrt() } else { d.splat_profile(at, cell) };
                if let Some(prev) = last {
                    most = most.max((occ - prev).abs());
                }
                last = Some(occ);
            }
            most
        };
        assert!(jump(false) < jump(true) * 0.25, "{} vs an ellipsoid's {}", jump(false), jump(true));
    }

    #[test]
    fn ripples_bounce_off_a_body_in_the_water() {
        let behind = |blocked: bool| {
            let water = reference(Water { size: [16.0, 16.0], depth: 2.0, waves: Vec::new(), ripple_resolution: 128, ripple_area: 32.0, sim_dispersion: 1.0, ..Default::default() });
            let window = choose_window(&water, Vec2::ZERO, None);
            let field = SimField::build(&water, window, None);
            let p = PipeParams::for_water(&water);
            let mut sim = PipeSim::new(&field);
            sim.set_ewave(EwaveParams::for_water(&water, window, 2.0));
            let wall = WaterDisplacer { center: Vec2::ZERO, half: Vec2::new(0.3, 9.0), axis: Vec2::X, thickness: 0.4, round: false, velocity: Vec2::ZERO, rate: 0.0, momentum: 1.0, impulse: false };
            let mut peak = 0.0f32;
            for k in 0..450 {
                let mut list = if blocked { vec![wall] } else { Vec::new() };
                if k == 20 {
                    list.push(WaterDisplacer::from_push(&WaterPush { at: Vec2::new(-2.0, 0.0), radius: 0.5, strength: 6.0 }));
                }
                sim.step(1.0 / 90.0, &p, &list);
                if k > 20 {
                    let cell = sim.window.cell;
                    let w = (Vec2::new(2.0, 0.0) / cell).floor().as_ivec2() - sim.window.origin;
                    peak = peak.max(sim.ewave().map_or(0.0, |l| l.state[sim.store(w)][0].abs()));
                }
            }
            peak
        };
        let (open, walled) = (behind(false), behind(true));
        assert!(open > 1e-5, "the ripples get across open water: {open}");
        assert!(walled < open * 0.5, "a body in the way stops most of them: {walled} vs {open}");
    }

    #[test]
    fn a_current_piles_up_in_front_of_a_post() {
        let water = Water { size: [10.0, 10.0], depth: 1.0, waves: Vec::new(), current: [1.0, 0.0, 0.0], ripple_resolution: 64, ripple_area: 20.0, ..Default::default() };
        let window = choose_window(&water, Vec2::ZERO, None);
        let mut field = SimField::build(&water, window, None);
        assert!(field.flowing);
        let c = 32usize;
        for z in c - 2..c + 2 {
            for x in c - 2..c + 2 {
                Arc::make_mut(&mut field.cells)[z * 64 + x][3] = CELL_OBSTACLE;
            }
        }
        let mut sim = PipeSim::new(&field);
        let p = PipeParams::for_water(&water);
        for _ in 0..120 {
            sim.step(1.0 / 90.0, &p, &[]);
        }
        let front = sim.height(c as i32 - 3, c as i32);
        let lee = sim.height(c as i32 + 2, c as i32);
        assert!(front > 0.005, "bow upstream: {front}");
        assert!(lee < -0.005, "drawn down behind: {lee}");
        assert!(sim.h.iter().all(|h| h.is_finite()));
    }

    #[test]
    fn moving_the_window_keeps_the_water_in_place() {
        let water = Water { body: WaterBody::Ocean, depth: 2.0, waves: Vec::new(), ripple_resolution: 64, ripple_area: 16.0, ..Default::default() };
        let w0 = choose_window(&water, Vec2::ZERO, None);
        assert!(!w0.closed);
        let mut sim = PipeSim::new(&SimField::build(&water, w0, None));
        bump(&mut sim, IVec2::new(40, 30), 3, 0.2);
        let world = w0.origin + IVec2::new(40, 30);
        let before = sim.height(40, 30);
        let w1 = SimWindow { origin: w0.origin + IVec2::new(5, -3), ..w0 };
        sim.set_field(&SimField::build(&water, w1, None));
        sim.move_to(w1);
        sim.step(0.0, &PipeParams::for_water(&water), &[]);
        let local = world - w1.origin;
        assert!((sim.height(local.x, local.y) - before).abs() < 1e-6);
        assert!(sim.height(63, 63).abs() < 1e-6 && sim.height(0, 0).abs() < 1e-6, "new cells start at rest");
    }

    #[test]
    fn deep_water_waves_travel_at_the_wave_depth() {
        let speed_for = |depth: f32| {
            let (water, field) = tank(128, 16.0, depth);
            let p = quiet(PipeParams { wave_depth: 1.0, ..PipeParams::for_water(&water) });
            let mut sim = PipeSim::new(&field);
            let c = center(&sim);
            bump(&mut sim, c, 3, 0.05);
            for _ in 0..30 {
                sim.step(1.0 / 90.0, &p, &[]);
            }
            // How far out the front has got.
            (c.x..128).rev().find(|&x| sim.height(x, c.y).abs() > 1e-4).unwrap_or(c.x) - c.x
        };
        let (shallow, deep) = (speed_for(1.0), speed_for(40.0));
        assert!((shallow - deep).abs() <= 2, "a 40 m column's waves run like 1 m ones: {shallow} vs {deep}");
    }

    #[test]
    fn small_lakes_are_covered_and_oceans_followed() {
        let lake = Water { size: [20.0, 10.0], ripple_area: 32.0, ripple_resolution: 250, ..Default::default() };
        let w = choose_window(&lake, Vec2::new(100.0, 0.0), None);
        assert!(w.closed && w.n == 248 && w.n.is_multiple_of(8));
        assert!((w.size() - 24.0).abs() < 0.2 && w.center().length() < w.cell, "{w:?}");
        let field = SimField::build(&lake, w, None);
        let corner = field.cells[0];
        assert_eq!(corner[3], CELL_SHORE, "outside the lake is shore");
        let mid = field.cells[(w.n / 2 * w.n + w.n / 2) as usize];
        assert_eq!(mid, [5.0, 0.0, 0.0, CELL_WATER]);
        let ocean = Water { body: WaterBody::Ocean, ripple_area: 32.0, ripple_resolution: 256, ..Default::default() };
        let a = choose_window(&ocean, Vec2::new(10.0, 4.0), None);
        assert!(!a.closed && (a.center() - Vec2::new(10.0, 4.0)).length() < a.cell);
        let b = choose_window(&ocean, Vec2::new(12.0, 4.0), Some(a));
        assert_eq!(a, b, "small moves keep the window");
        let c = choose_window(&ocean, Vec2::new(20.0, 4.0), Some(a));
        assert_ne!(a, c, "a quarter window away it moves");
    }

    #[test]
    fn displacers_come_from_the_submerged_part_of_a_box() {
        let corners: [Vec3; 8] = std::array::from_fn(|k| {
            Vec3::new(if k & 1 == 0 { -1.0 } else { 1.0 }, if k & 2 == 0 { -0.6 } else { 0.4 }, if k & 4 == 0 { -0.5 } else { 0.5 }) + Vec3::new(3.0, 0.0, 2.0)
        });
        let d = WaterDisplacer::from_corners(&corners, Vec3::X, 0.0, 3.0, Vec2::new(1.0, 0.0), false).unwrap();
        assert!((d.center - Vec2::new(3.0, 2.0)).length() < 1e-5);
        assert!((d.half - Vec2::new(1.0, 0.5)).length() < 1e-5);
        assert!((d.thickness - 0.6).abs() < 1e-5, "only below the surface: {}", d.thickness);
        assert!((d.volume() - 1.2).abs() < 1e-4);
        let shallow = WaterDisplacer::from_corners(&corners, Vec3::X, 0.0, 0.25, Vec2::ZERO, false).unwrap();
        assert!((shallow.thickness - 0.25).abs() < 1e-5, "capped to the reach");
        let above: [Vec3; 8] = corners.map(|c| c + Vec3::Y * 2.0);
        assert!(WaterDisplacer::from_corners(&above, Vec3::X, 0.0, 3.0, Vec2::ZERO, false).is_none());
        let turned = WaterDisplacer::from_corners(&corners, Vec3::new(0.0, 0.0, 1.0), 0.0, 3.0, Vec2::ZERO, false).unwrap();
        assert!((turned.half - Vec2::new(0.5, 1.0)).length() < 1e-5, "extents follow the axis");
        let tiny = WaterDisplacer { half: Vec2::splat(0.01), ..d }.fit_to_cell(0.2);
        assert!((tiny.volume() - WaterDisplacer { half: Vec2::splat(0.01), ..d }.volume()).abs() < 1e-6 && tiny.half.x >= 0.15);
        let cell = 0.05;
        let total: f32 = (-60..60)
            .flat_map(|z| (-60..60).map(move |x| (x, z)))
            .map(|(x, z)| d.profile(Vec2::new(3.0 + (x as f32 + 0.5) * cell, 2.0 + (z as f32 + 0.5) * cell), cell) * d.thickness * cell * cell)
            .sum();
        assert!((total - d.volume()).abs() < d.volume() * 0.03, "the splat holds its volume: {total} vs {}", d.volume());
    }

    #[test]
    fn the_dispersive_layer_adds_to_the_output() {
        let (water, mut field) = tank(64, 8.0, 2.0);
        for z in 0..64 {
            Arc::make_mut(&mut field.cells)[z * 64 + 50][3] = CELL_OBSTACLE;
        }
        let p = PipeParams::for_water(&water);
        let ew = EwaveParams::for_water(&water, field.window, field.max_depth).expect("a 64-cell window runs it");
        assert!(EwaveParams::for_water(&Water { sim_dispersion: 0.0, ..water.clone() }, field.window, 2.0).is_none());
        assert!(EwaveParams::for_water(&water, SimWindow { n: 48, ..field.window }, 2.0).is_none(), "needs a power of two");
        let mut plain = PipeSim::new(&field);
        let mut sim = PipeSim::new(&field);
        sim.set_ewave(Some(ew));
        for k in 0..60 {
            let d: Vec<WaterDisplacer> = if k == 0 { vec![WaterDisplacer::from_push(&WaterPush { at: Vec2::ZERO, radius: 0.4, strength: 6.0 })] } else { Vec::new() };
            plain.step(1.0 / 90.0, &p, &d);
            sim.step(1.0 / 90.0, &p, &d);
        }
        let (a, _) = plain.output();
        let (b, back) = sim.output();
        let diff = a.iter().zip(&b).map(|(x, y)| (x[0] - y[0]).abs()).fold(0.0, f32::max);
        assert!(diff > 1e-3, "the impact's wave train shows: {diff}");
        let layer = sim.ewave().unwrap();
        assert!((0..64).all(|z| layer.state[sim.store(IVec2::new(50, z))] == [0.0; 2]), "nothing in the wall");
        assert!(back.iter().all(|c| c.iter().all(|v| v.is_finite())));
        let orbital = a.iter().zip(&b).map(|(x, y)| (x[3] - y[3]).abs()).fold(0.0, f32::max);
        assert!(orbital > 1e-3, "the waves' orbital motion is in the velocity: {orbital}");
    }

    #[test]
    fn a_floating_body_settles_with_the_level_back_at_rest() {
        let (water, field) = tank(64, 8.0, 1.0);
        let p = PipeParams::for_water(&water);
        let mut sim = PipeSim::new(&field);
        let d = WaterDisplacer { center: Vec2::ZERO, half: Vec2::splat(0.5), axis: Vec2::X, thickness: 0.3, round: false, velocity: Vec2::ZERO, rate: 0.0, momentum: 1.0, impulse: false };
        for _ in 0..900 {
            sim.step(1.0 / 90.0, &p, &[d]);
        }
        let c = center(&sim);
        assert!(sim.height(c.x, c.y).abs() < 0.02, "the surface over it is level: {}", sim.height(c.x, c.y));
        assert!(sim.height(c.x + 20, c.y).abs() < 0.02);
    }

    /// Shallow open water and a sphere (half under) and a box (standing on
    /// the floor, through the surface) held still at `at`, as the editor
    /// feeds them.
    fn still_scene(n: u32, area: f32, dispersion: f32) -> Water {
        Water { body: WaterBody::Ocean, depth: 0.5, ripple_area: area, ripple_resolution: n, sim_dispersion: dispersion, ..Default::default() }
    }

    fn still_bodies(at: Vec2, cell: f32) -> Vec<WaterDisplacer> {
        let cube = |c: Vec3, h: Vec3| -> [Vec3; 8] {
            std::array::from_fn(|k| c + Vec3::new(if k & 1 == 0 { -h.x } else { h.x }, if k & 2 == 0 { -h.y } else { h.y }, if k & 4 == 0 { -h.z } else { h.z }))
        };
        let sphere = cube(Vec3::new(at.x - 0.7, 0.0, at.y), Vec3::splat(0.4));
        let block = cube(Vec3::new(at.x + 0.7, -0.2, at.y + 0.3), Vec3::new(0.3, 0.6, 0.3));
        vec![
            WaterDisplacer::from_corners(&sphere, Vec3::X, 0.0, 0.5, Vec2::ZERO, true).unwrap().fit_to_cell(cell),
            WaterDisplacer::from_corners(&block, Vec3::X, 0.0, 0.5, Vec2::ZERO, false).unwrap().fit_to_cell(cell),
        ]
    }

    /// The bodies eased in over the first frames (dropped into place), then held.
    fn eased(bodies: &[WaterDisplacer], frame: u32) -> Vec<WaterDisplacer> {
        let k = (frame as f32 / 20.0).min(1.0);
        bodies.iter().map(|d| WaterDisplacer { thickness: d.thickness * k, ..*d }).collect()
    }

    /// Wave energy in the window (per unit density, up to cell area): the
    /// pipe sim's potential and kinetic energy plus the dispersive layer's.
    fn wave_energy(sim: &PipeSim) -> f64 {
        let (tex, _) = sim.output();
        let mut e = 0.0f64;
        for (k, w) in sim.cells().enumerate() {
            let s = sim.store(w);
            e += GRAVITY as f64 * (tex[k][0] as f64).powi(2) + sim.h[s] as f64 * sim.vel[s].length_squared() as f64;
        }
        e
    }

    /// Largest surface offset inside the sponge border.
    fn inner_peak(sim: &PipeSim) -> f32 {
        let (tex, _) = sim.output();
        sim.cells().zip(&tex).filter(|(w, _)| sim.sponge_at(*w) == 0.0).map(|(_, t)| t[0].abs()).fold(0.0, f32::max)
    }

    #[test]
    fn still_bodies_leave_the_pipe_sim_flat() {
        let water = still_scene(64, 8.0, 0.0);
        let window = choose_window(&water, Vec2::ZERO, None);
        let field = SimField::build(&water, window, None);
        let mut sim = PipeSim::new(&field);
        assert!(EwaveParams::for_water(&water, window, field.max_depth).is_none());
        let p = PipeParams::for_water(&water);
        let bodies = still_bodies(Vec2::ZERO, window.cell);
        let mut most = 0.0f64;
        for f in 0..2700 {
            sim.step(1.0 / 90.0, &p, &eased(&bodies, f));
            most = most.max(wave_energy(&sim));
        }
        let left = wave_energy(&sim);
        assert!(most > 1e-2, "the bodies stirred it while easing in: {most}");
        assert!(left < most * 1e-7, "energy decays: {most} -> {left}");
        assert!(inner_peak(&sim) < 1e-4, "flat round the bodies: {}", inner_peak(&sim));
    }

    #[test]
    fn still_bodies_leave_the_dispersive_layer_flat() {
        let water = still_scene(64, 8.0, 1.0);
        let window = choose_window(&water, Vec2::ZERO, None);
        let p = EwaveParams::for_water(&water, window, 0.5).expect("a 64-cell window runs it");
        let n = window.n as usize;
        let sponge = window.sponge();
        let mut layer = EwaveLayer::new(n);
        let bodies = still_bodies(Vec2::ZERO, window.cell);
        let mut most = 0.0f64;
        for f in 0..2700 {
            let list = eased(&bodies, f);
            for z in 0..n {
                for x in 0..n {
                    let at = (window.origin + IVec2::new(x as i32, z as i32)).as_vec2() * window.cell + Vec2::splat(window.cell * 0.5);
                    let occ: f32 = list.iter().map(|d| d.thickness * d.profile(at, window.cell)).sum();
                    layer.inject(z * n + x, occ, 0.0, &p, 1.0, true);
                }
            }
            layer.propagate(1.0 / 90.0, window.cell, &p);
            for z in 0..n as i32 {
                for x in 0..n as i32 {
                    layer.mask_cell(z as usize * n + x as usize, false, sponge_depth(IVec2::new(x, z), n as i32, sponge), 0.0, 1.0 / 90.0);
                }
            }
            most = most.max(layer.energy(window.cell, &p));
        }
        let left = layer.energy(window.cell, &p);
        let peak = layer.state.iter().map(|s| s[0].abs()).fold(0.0, f32::max);
        assert!(most > 1e-6, "easing in made waves: {most}");
        assert!(left < most * 1e-7, "energy decays: {most} -> {left}");
        assert!(peak < 1e-4, "flat: {peak}");
    }

    #[test]
    fn still_bodies_stay_flat_while_the_window_follows_the_camera() {
        let water = still_scene(64, 8.0, 1.0);
        let mut feed = WaterSimFeed::default();
        let window = choose_window(&water, Vec2::ZERO, None);
        let (field, _) = feed.field(&water, window);
        let mut sim = PipeSim::new(&field);
        sim.set_ewave(EwaveParams::for_water(&water, window, field.max_depth));
        assert!(sim.ewave().is_some());
        let p = PipeParams::for_water(&water);
        let bodies = still_bodies(Vec2::new(-1.6, 0.0), window.cell);
        let (mut moves, mut late) = (0, 0.0f32);
        for f in 0..3600u32 {
            // Settle first, then the camera walks to and fro, the window
            // leaving the bodies behind and coming back over them.
            let cam = if f < 1800 || (f / 150) % 2 == 0 { Vec2::ZERO } else { Vec2::new(4.5, 0.0) };
            let w = choose_window(&water, cam, Some(sim.window));
            if w != sim.window {
                let (field, _) = feed.field(&water, w);
                sim.set_field(&field);
                sim.move_to(w);
                moves += 1;
            }
            sim.step(1.0 / 90.0, &p, &eased(&bodies, f));
            if f >= 1800 {
                late = late.max(inner_peak(&sim));
            }
        }
        assert!(moves >= 10, "the window moved: {moves}");
        assert!(late < 2e-3, "moving the window over still bodies makes no waves: {late}");
        assert!(wave_energy(&sim) < 1e-6, "{}", wave_energy(&sim));
    }
}

