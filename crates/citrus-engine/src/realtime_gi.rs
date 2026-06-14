//! Realtime-GI driver: continuously re-traces light probes from the realtime
//! lights (reusing the bake path tracer in `probes_only` mode) so un-baked
//! surfaces show live indirect bounce. Shared by the editor and the game
//! runtime so a shipped game lights the same way the editor previews.
//!
//! It only re-traces when the inputs change (lights/objects/settings), then lets
//! the accumulated SH settle over a few updates and goes idle — so a static
//! scene does no GPU work and never hitches.

use std::hash::{Hash, Hasher};

use citrus_render::Renderer;

use crate::scene::{BakeGather, LoadedScene};

/// How many blended re-traces to run after an input change (temporal settle).
/// Paired with the low default temporal blend (~0.12) this gives a gentle
/// ~2-3s ease-in to the converged GI, then idles.
const SETTLE_UPDATES: u32 = 96;

/// Probe-grid layout (`world_to_local`, size, counts, sh_base) for an upload.
type VolUpload = (glam::Mat4, [f32; 3], [u32; 3], u32);

#[derive(Default)]
pub struct RealtimeGiState {
    /// Temporally-accumulated probe SH (blended toward each new trace).
    accum: Vec<citrus_render::ProbeSh>,
    /// Grid dimensions the accumulation was built for (a change forces a reset).
    counts: [usize; 3],
    /// Whether our probes are currently uploaded (so we can clear on disable).
    active: bool,
    /// Seconds until the next allowed trace.
    timer: f32,
    /// Hash of the last-traced inputs; unchanged + settled = skip the trace.
    hash: u64,
    /// Remaining blended re-traces after a change; 0 = converged, skip work.
    settle: u32,
    /// True while the inputs are actively changing (a light/emitter is moving).
    /// Drives the snap-to-latest response so bounce light tracks in realtime;
    /// false lets it settle smoothly toward the converged value.
    moving: bool,
    /// Monotonic per-trace seed so software-GI samples jitter each update and
    /// temporal accumulation averages out the noise.
    seed: u32,
    /// Hash of the geometry the cached GPU GDF was built from. The GDF (3D
    /// distance/index textures) is re-uploaded only when this changes, so a static
    /// scene keeps a high-resolution GDF for free instead of rebuilding per trace.
    gdf_hash: Option<u64>,
    /// Latest finished trace (intensity applied) — the per-frame ease target.
    target: Vec<citrus_render::ProbeSh>,
    /// Volume layout to upload alongside `target` / `accum`.
    target_vols: Vec<VolUpload>,
    /// One-shot activation diagnostic guard.
    logged: bool,
    /// In-flight software march on a background thread (so moving objects in
    /// Play mode don't hitch the frame). Carries the volume layout to upload
    /// with the result.
    job: Option<(std::thread::JoinHandle<Vec<citrus_render::ProbeSh>>, Vec<VolUpload>, [usize; 3])>,
}

impl RealtimeGiState {
    /// Tick the realtime-GI update. Call once per frame with the frame delta.
    pub fn update(&mut self, renderer: &mut Renderer, scene: &mut LoadedScene, dt: f32) {
        let gi = scene.environment.realtime_gi;
        let on = gi.enabled && scene.baked.is_none();
        if !on {
            if self.active {
                renderer.set_baked_probes(&[], &[]);
                self.accum.clear();
                self.target.clear();
                self.counts = [0; 3];
                self.active = false;
                self.settle = 0;
                self.logged = false;
            }
            self.job = None; // detach any in-flight march; its result is dropped
            return;
        }
        // Hardware (ray-query) mode needs RT cores; software (SDF) runs anywhere.
        if gi.mode == citrus_assets::GiMode::RayQuery && !renderer.supports_baking() {
            return;
        }

        // 1) Collect a finished async software march, if any.
        let mut fresh: Option<(Vec<citrus_render::ProbeSh>, Vec<VolUpload>, [usize; 3])> = None;
        if let Some((handle, _, _)) = &self.job
            && handle.is_finished()
        {
            let (handle, vols, counts) = self.job.take().unwrap();
            if let Ok(probes) = handle.join() {
                fresh = Some((probes, vols, counts));
            }
        }

        // 2) Kick off a new trace on the cadence, when inputs changed / settling.
        self.timer -= dt;
        if self.timer <= 0.0 && self.job.is_none() {
            // Floor 0 → trace every frame (the GPU march is cheap); the user's
            // Update Interval throttles it back up to save GPU. Static scenes stop
            // tracing once settled regardless, so 0 only costs while things move.
            self.timer = gi.update_interval.max(0.0);
            if let Some(gather) = scene.gather_realtime_gi() {
                let hash = hash_inputs(&gather, &gi);
                let changed = hash != self.hash || !self.active;
                if changed {
                    self.hash = hash;
                    self.settle = SETTLE_UPDATES;
                }
                // Track motion so the ingest below snaps to the latest trace
                // while things move, then settles smoothly once they stop.
                self.moving = changed;
                if self.settle > 0 {
                    self.settle -= 1;
                    self.seed = self.seed.wrapping_add(0x9E37_79B9);
                    let vols = vol_uploads(&gather);
                    let counts = gather.probe_volumes[0].counts;
                    if gi.mode == citrus_assets::GiMode::Software {
                        let (insts, scene_size) = scene.software_gi_inputs(&gather);
                        // GDF bounds/dims over the coarsest cascade (full-scene box).
                        let coarsest = gather.probe_volumes.last().unwrap();
                        let center = -coarsest.world_to_local.w_axis.truncate();
                        let size = glam::Vec3::from(coarsest.size);
                        let (gmin, gmax) = (center - size * 0.5, center + size * 0.5);
                        let voxel = (size.max_element() / 64.0).max(0.05);
                        let dims = [
                            ((size.x / voxel).round() as u32 + 1).clamp(8, 64),
                            ((size.y / voxel).round() as u32 + 1).clamp(8, 64),
                            ((size.z / voxel).round() as u32 + 1).clamp(8, 64),
                        ];
                        let eps = (scene_size * 0.004).max(1e-3);
                        let max_dist = scene_size * 1.5 + 1.0;
                        // `vols`/`counts` go to whichever path produces the trace,
                        // exactly once — GPU march (fresh) or CPU thread (job).
                        let mut pending = Some((vols, counts));
                        if renderer.gi_gpu_available() {
                            // The GDF is geometry-only: rebuild + re-upload it only
                            // when the geometry/materials/bounds hash changes, not
                            // every trace, so a static scene keeps a sharp cached
                            // field for free while lights/emitters move.
                            let gh = hash_gdf_inputs(&insts, gmin, gmax, dims);
                            if self.gdf_hash != Some(gh) {
                                let gdf = crate::sw_gi::build_gdf(&insts, gmin, gmax, dims);
                                let materials: Vec<_> = insts
                                    .iter()
                                    .map(|i| citrus_render::GpuGiMaterial {
                                        albedo: i.albedo,
                                        emission: i.emission,
                                    })
                                    .collect();
                                renderer.gi_set_gdf(
                                    &gdf.dist,
                                    &gdf.index,
                                    gdf.dims,
                                    gdf.min.to_array(),
                                    gdf.max.to_array(),
                                    &materials,
                                );
                                self.gdf_hash = Some(gh);
                            }
                            // Emissive instances → sphere area-lights for NEE, so
                            // their GI fill is sampled directly (smooth) instead of
                            // via blotchy random ray hits.
                            let emitters: Vec<_> = crate::sw_gi::emitter_spheres(&insts)
                                .into_iter()
                                .map(|(center, radius, emission)| citrus_render::GpuGiEmitter {
                                    center,
                                    radius,
                                    emission,
                                })
                                .collect();
                            let march = citrus_render::GpuGiMarch {
                                lights: &gather.lights,
                                emitters: &emitters,
                                probes: &gather.probes,
                                samples: gi.samples,
                                bounces: gi.bounces,
                                sky: gather.sky_color,
                                eps,
                                max_dist,
                                seed: self.seed,
                            };
                            if let Some(out) = renderer.gi_march(&march) {
                                let (vols, counts) = pending.take().unwrap();
                                fresh = Some((out, vols, counts));
                            }
                        }
                        // No GPU compute (or the march failed) → CPU march thread.
                        if let Some((vols, counts)) = pending.take() {
                            let lights = gather.lights.clone();
                            let probes = gather.probes.clone();
                            let (sky, samples, seed) = (gather.sky_color, gi.samples, self.seed);
                            let bounces = gi.bounces;
                            let handle = std::thread::spawn(move || {
                                crate::sw_gi::march_probes(
                                    &insts, &lights, &probes, sky, samples, scene_size, bounces,
                                    seed,
                                )
                            });
                            self.job = Some((handle, vols, counts));
                        }
                    } else {
                        // Hardware ray-query: synchronous (GPU async is a follow-up).
                        let input = citrus_render::BakeInput {
                            instances: &gather.instances,
                            lights: &gather.lights,
                            probes: &gather.probes,
                            sky_color: gather.sky_color,
                            bounces: gi.bounces.clamp(0, 8),
                            samples: gi.samples.clamp(1, 1024),
                            probes_only: true,
                        };
                        match renderer.bake_lighting(&input) {
                            Ok(o) => fresh = Some((o.probes, vols, counts)),
                            Err(e) => {
                                tracing::warn!("realtime GI update failed: {e:#}");
                                scene.environment.realtime_gi.enabled = false;
                                return;
                            }
                        }
                    }
                }
            }
        }

        // 3) Ingest a fresh trace as the new ease target (apply intensity here).
        if let Some((mut probes, vols, counts)) = fresh {
            // Spatially denoise each cascade's grid first — this cancels the
            // blotchy per-probe Monte-Carlo variance with no temporal lag, so the
            // "Responsiveness" EMA below can run snappy without trading back into
            // noise. The software grids are coarser, so blur harder: a wider
            // kernel also softens the trilinear cell facets (the "squares"). The
            // flat probe array is N concatenated cascade grids, so blur each
            // sub-grid by its own (counts, sh_base) layout.
            let iters = if gi.mode == citrus_assets::GiMode::Software { 6 } else { 2 };
            for (_, _, c, base) in &vols {
                let cnt = [c[0] as usize, c[1] as usize, c[2] as usize];
                let n = cnt[0] * cnt[1] * cnt[2];
                let start = *base as usize;
                if start + n <= probes.len() {
                    crate::sw_gi::blur_probe_grid(&mut probes[start..start + n], cnt, iters);
                }
            }
            let k = gi.intensity.max(0.0);
            if (k - 1.0).abs() > 1e-3 {
                for p in &mut probes {
                    for b in 0..4 {
                        for c in 0..3 {
                            p.coeffs[b][c] *= k;
                        }
                    }
                }
            }
            // Grid changed (or first result) → resize via the full upload (which
            // re-points descriptors) and snap both buffers to it.
            if self.target.len() != probes.len() || self.counts != counts {
                self.counts = counts;
                self.target = probes.clone();
                self.accum = probes;
                renderer.set_baked_probes(&self.accum, &vols);
                self.active = true;
            } else {
                // Blend each new trace into the target as an exponential moving
                // average. Two regimes, because noise and motion-tracking want
                // opposite rates and (now that each trace is spatially denoised)
                // we no longer need the EMA to fight spatial noise:
                //   - Moving: a light/emitter is moving, so snap toward the
                //     latest trace (rate = `Responsiveness`) — the bounce tracks
                //     in realtime.
                //   - Static: nothing is moving; average gently at a fixed low
                //     rate so the residual per-trace variance converges smoothly
                //     instead of flickering. This is independent of
                //     `Responsiveness`, so cranking it up never makes a still
                //     scene shimmer.
                let alpha = if self.moving {
                    // Map Responsiveness across the full range. The GPU traces are
                    // clean (high-spp + spatially denoised), so at max we can snap
                    // straight to the latest trace — a moving emitter's bounce
                    // reaches full brightness in one trace instead of fading in
                    // over several. Static scenes use the gentle rate below, so
                    // this never makes a still scene shimmer.
                    0.2 + 0.8 * gi.temporal_blend.clamp(0.0, 1.0)
                } else {
                    // Static: a low blend rate widens the temporal average over many
                    // independent-seed traces, which is what actually cancels the
                    // blotchy per-probe variance around bright emitters (a fixed-
                    // alpha EMA caps variance reduction at ~alpha/(2-alpha), so a
                    // smaller alpha = a cleaner converged fill). SETTLE_UPDATES is
                    // sized to let this reach steady state before idling.
                    0.03
                };
                for (t, f) in self.target.iter_mut().zip(&probes) {
                    for b in 0..4 {
                        for c in 0..3 {
                            t.coeffs[b][c] += (f.coeffs[b][c] - t.coeffs[b][c]) * alpha;
                        }
                        // Visibility moments blend alongside the radiance SH.
                        t.dist[b] += (f.dist[b] - t.dist[b]) * alpha;
                    }
                }
            }
            self.target_vols = vols;
            if !self.logged {
                self.logged = true;
                let n = self.target.len().max(1) as f32;
                let avg = self
                    .target
                    .iter()
                    .map(|p| {
                        0.2126 * p.coeffs[0][0] + 0.7152 * p.coeffs[0][1] + 0.0722 * p.coeffs[0][2]
                    })
                    .sum::<f32>()
                    / n;
                tracing::info!(
                    "realtime GI active ({:?}): {} probes, avg L0 luminance {:.3}",
                    gi.mode,
                    self.target.len(),
                    avg
                );
            }
        }

        // 4) Per-frame ease of the uploaded probes toward the (already EMA-
        // smoothed) target, so the result glides between trace updates instead
        // of stepping. Fixed short time-constant — variance smoothing lives in
        // the EMA above; this is purely visual continuity. Cheap in-place upload.
        if self.target.is_empty() || self.accum.len() != self.target.len() {
            return;
        }
        // Glide near-instantly while moving (track the latest trace) and gently
        // when settling (smooth out residual variance).
        let tau = if self.moving { 0.015 } else { 0.08 };
        let f = 1.0 - (-dt / tau).exp();
        let mut max_delta = 0.0f32;
        for (acc, tgt) in self.accum.iter_mut().zip(&self.target) {
            for b in 0..4 {
                for c in 0..3 {
                    let d = tgt.coeffs[b][c] - acc.coeffs[b][c];
                    acc.coeffs[b][c] += d * f;
                    max_delta = max_delta.max(d.abs());
                }
                // Ease the visibility moments too (not counted in max_delta — the
                // radiance settling already keeps the upload alive while it eases).
                acc.dist[b] += (tgt.dist[b] - acc.dist[b]) * f;
            }
        }
        // Skip the upload once fully converged + static (nothing to push).
        if max_delta > 1e-5 {
            if !renderer.update_probe_sh(&self.accum) {
                renderer.set_baked_probes(&self.accum, &self.target_vols);
            }
        }
        self.active = true;
    }
}

/// Probe-volume layout tuples for `set_baked_probes`.
fn vol_uploads(gather: &BakeGather) -> Vec<VolUpload> {
    gather
        .probe_volumes
        .iter()
        .map(|v| {
            (
                v.world_to_local,
                v.size,
                [v.counts[0] as u32, v.counts[1] as u32, v.counts[2] as u32],
                v.sh_base as u32,
            )
        })
        .collect()
}

/// Hash the realtime-GI inputs (light positions/colors, object transforms, GI
/// settings) so an unchanged scene can skip the probe re-trace. f32s fold by bit
/// pattern — exact, which is fine since we only ask "did anything change".
fn hash_inputs(gather: &BakeGather, gi: &citrus_assets::RealtimeGi) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let f = |x: f32, h: &mut std::collections::hash_map::DefaultHasher| x.to_bits().hash(h);
    for l in &gather.lights {
        for v in [
            l.position.x,
            l.position.y,
            l.position.z,
            l.direction.x,
            l.direction.y,
            l.direction.z,
            l.color[0],
            l.color[1],
            l.color[2],
            l.range,
        ] {
            f(v, &mut h);
        }
    }
    for inst in &gather.instances {
        for v in inst.transform.to_cols_array() {
            f(v, &mut h);
        }
    }
    for v in [
        gather.sky_color[0],
        gather.sky_color[1],
        gather.sky_color[2],
        gi.intensity,
        gi.probe_spacing,
        gi.temporal_blend,
    ] {
        f(v, &mut h);
    }
    gi.bounces.hash(&mut h);
    gi.samples.hash(&mut h);
    h.finish()
}

/// Hash only the inputs the cached GDF is built from — per-instance geometry
/// (world→local transform, world scale, mesh SDF identity) and materials
/// (albedo/emission), plus the field bounds and resolution. Lights/probes/sky
/// are NOT included: they change per trace but don't affect the distance field,
/// so the GDF survives a moving light and is re-uploaded only when geometry does.
fn hash_gdf_inputs(
    insts: &[crate::sw_gi::SdfInstance],
    min: glam::Vec3,
    max: glam::Vec3,
    dims: [u32; 3],
) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let f = |x: f32, h: &mut std::collections::hash_map::DefaultHasher| x.to_bits().hash(h);
    for inst in insts {
        for v in inst.inv.to_cols_array() {
            f(v, &mut h);
        }
        f(inst.scale, &mut h);
        for v in inst.albedo.iter().chain(inst.emission.iter()) {
            f(*v, &mut h);
        }
        // Mesh SDF identity: same Arc → same field, so pointer is enough.
        (std::sync::Arc::as_ptr(&inst.sdf) as usize).hash(&mut h);
    }
    for v in min.to_array().iter().chain(max.to_array().iter()) {
        f(*v, &mut h);
    }
    dims.hash(&mut h);
    h.finish()
}
