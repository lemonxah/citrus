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
    /// Set externally (e.g. a material edit) to force the next trace even when
    /// the input hash is unchanged. Cleared the moment it's consumed.
    force: bool,
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
    /// Persistent, un-blurred temporal accumulation of per-cascade traces. Each
    /// frame only one cascade is re-traced (round-robin) and blended in here;
    /// `target` is its spatially-filtered view. Lumen-style: never re-trace the
    /// whole field at once — amortize across frames + accumulate temporally.
    raw: Vec<citrus_render::ProbeSh>,
    /// Which cascade to trace next (round-robin over the probe volumes).
    trace_cursor: usize,
    /// Latest finished trace (intensity applied) — the per-frame ease target.
    target: Vec<citrus_render::ProbeSh>,
    /// Volume layout to upload alongside `target` / `accum`.
    target_vols: Vec<VolUpload>,
    /// One-shot activation diagnostic guard.
    logged: bool,
    /// In-flight software march on a background thread (so moving objects in
    /// Play mode don't hitch the frame). Carries the volume layout to upload
    /// with the result.
    /// In-flight CPU march (cascade `usize`) on a background thread.
    job: Option<(
        std::thread::JoinHandle<Vec<citrus_render::ProbeSh>>,
        Vec<VolUpload>,
        [usize; 3],
        usize,
    )>,
    /// Volume layout + counts + cascade index for an in-flight async GPU march
    /// (collected via `gi_march_poll` next frame, so it never blocks the frame).
    gpu_pending: Option<(Vec<VolUpload>, [usize; 3], usize)>,
}

impl RealtimeGiState {
    /// Force the next trace even if the input hash is unchanged. Use after an
    /// edit that affects bounce light but may not be captured by the hash
    /// (e.g. a material reassignment or a non-hashed property change).
    pub fn invalidate(&mut self) {
        self.force = true;
    }

    /// Tick the realtime-GI update. Call once per frame with the frame delta.
    pub fn update(&mut self, renderer: &mut Renderer, scene: &mut LoadedScene, dt: f32) {
        // Flux is the realtime GI path: always run the Software (SDF/GDF) march
        // that feeds the screen-space gather, regardless of any legacy mode an
        // older scene file carried. (Baking is separate, via the Baker's Man.)
        let mut gi = scene.environment.realtime_gi;
        gi.mode = citrus_assets::GiMode::Software;
        // The GDF + emitter feed Flux samples must refresh every frame so moving
        // emitters track without lag (the field is no longer user-facing).
        gi.update_interval = 0.0;
        let on = gi.enabled && scene.baked.is_none();
        // Always drain a finished async GPU march (even when off, so toggling GI
        // off mid-flight can't wedge a re-enable — the in-flight march is freed).
        let gpu_fresh = if renderer.gi_marching() {
            renderer.gi_march_poll()
        } else {
            None
        };
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
            self.job = None; // detach any in-flight CPU march; its result is dropped
            self.gpu_pending = None; // drop the polled GPU result above
            return;
        }
        // Push the runtime Flux trace params (Environment tab → Flux GI). Cheap;
        // a changed setting re-traces. Quality preset → samples/probe.
        renderer.set_flux_settings(citrus_render::FluxSettings {
            samples: gi.quality.samples(),
            bounces: gi.bounces.clamp(1, 4),
            march_distance: gi.march_distance.max(0.0),
            firefly_clamp: gi.firefly_clamp.max(0.5),
            smoothing: gi.smoothing.clamp(0.0, 1.0),
            intensity: gi.intensity.max(0.0),
        });
        // Hardware (ray-query) mode needs RT cores; software (SDF) runs anywhere.
        if gi.mode == citrus_assets::GiMode::RayQuery && !renderer.supports_baking() {
            return;
        }

        // 1) Collect a finished async march, if any — CPU thread or GPU. The
        // tuple carries which cascade was traced (round-robin amortization).
        let mut fresh: Option<(Vec<citrus_render::ProbeSh>, Vec<VolUpload>, [usize; 3], usize)> =
            None;
        if let Some((handle, _, _, _)) = &self.job
            && handle.is_finished()
        {
            let (handle, vols, counts, k) = self.job.take().unwrap();
            if let Ok(probes) = handle.join() {
                fresh = Some((probes, vols, counts, k));
            }
        }
        if let Some(probes) = gpu_fresh
            && let Some((vols, counts, k)) = self.gpu_pending.take()
        {
            fresh = Some((probes, vols, counts, k));
        }

        // 2) Kick off a new trace on the cadence, when inputs changed / settling.
        // Never start one while a march (CPU thread or GPU) is still in flight.
        self.timer -= dt;
        if self.timer <= 0.0 && self.job.is_none() && !renderer.gi_marching() {
            // Cadence floor. The software GPU march is cheap, so it can trace
            // every frame (floor 0). RayQuery re-bakes the whole probe grid
            // *synchronously on the main thread* (~ms × grid size), so a moving
            // emitter/light would block every frame — floor it to ~10 Hz so a
            // continuously animated GI input can't tank the framerate. Static
            // scenes settle and stop tracing regardless of mode.
            let interval_floor = if gi.mode == citrus_assets::GiMode::RayQuery {
                0.1
            } else {
                0.0
            };
            self.timer = gi.update_interval.max(interval_floor);
            if let Some(gather) = scene.gather_realtime_gi() {
                let hash = hash_inputs(&gather, &gi);
                let changed = hash != self.hash || !self.active || self.force;
                self.force = false;
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
                    // Amortize: trace only one cascade this frame (round-robin),
                    // so per-frame GI work is bounded regardless of total probe
                    // count. The ingest blends just this cascade into the
                    // persistent field and re-filters it.
                    let cascades = gather.probe_volumes.len().max(1);
                    let k = self.trace_cursor % cascades;
                    self.trace_cursor = (k + 1) % cascades;
                    let cbase = gather.probe_volumes[k].sh_base;
                    let cn = {
                        let c = gather.probe_volumes[k].counts;
                        c[0] * c[1] * c[2]
                    };
                    let cend = (cbase + cn).min(gather.probes.len());
                    let cascade_probes = gather.probes[cbase..cend].to_vec();
                    if gi.mode == citrus_assets::GiMode::Software {
                        let (insts, scene_size) = scene.software_gi_inputs(&gather);
                        // GDF bounds/dims over the coarsest cascade (full-scene box).
                        let coarsest = gather.probe_volumes.last().unwrap();
                        let center = -coarsest.world_to_local.w_axis.truncate();
                        let size = glam::Vec3::from(coarsest.size);
                        let (gmin, gmax) = (center - size * 0.5, center + size * 0.5);
                        // 128³ field (was 64³): finer distance + nearest-instance
                        // index → smoother sphere occlusion and emitter footprints
                        // (less GDF blockiness). Cached (rebuilt only when static
                        // geometry changes), so the larger CPU build is one-time.
                        let gres = gi.gdf_resolution.clamp(16, 256);
                        let voxel = (size.max_element() / gres as f32).max(0.03);
                        let dims = [
                            ((size.x / voxel).round() as u32 + 1).clamp(8, gres),
                            ((size.y / voxel).round() as u32 + 1).clamp(8, gres),
                            ((size.z / voxel).round() as u32 + 1).clamp(8, gres),
                        ];
                        let eps = (scene_size * 0.004).max(1e-3);
                        let max_dist = scene_size * 1.5 + 1.0;
                        // `vols`/`counts` go to whichever path produces the trace,
                        // exactly once — GPU march (fresh) or CPU thread (job).
                        let mut pending = Some((vols, counts));
                        if renderer.gi_gpu_available() {
                            // The GDF is built from STATIC geometry only, so a
                            // moving dynamic object (a player, physics body) never
                            // triggers the costly 64³ field rebuild — the prime
                            // cause of the play-mode framerate drop. Dynamic
                            // emitters still light the scene via analytic NEE
                            // (emitter spheres) below. Fall back to all instances
                            // when a scene has no static geometry at all.
                            //
                            // Emissive objects are excluded from the field: their
                            // coarse padded SDF box stamps a square halo + dark
                            // false-shadow blotches into the bounce, and NEE
                            // occlusion against that blocky shell combs/blotches
                            // badly. Their light reaches the scene via the analytic
                            // NEE spheres instead. (Leak through a solid emissive
                            // box is the accepted trade-off until a finer field
                            // allows clean occlusion.)
                            let gdf_insts: Vec<_> = insts
                                .iter()
                                .filter(|i| i.static_geometry && i.emission.iter().all(|&v| v == 0.0))
                                .cloned()
                                .collect();
                            let gdf_insts = if gdf_insts.is_empty() {
                                insts
                                    .iter()
                                    .filter(|i| i.emission.iter().all(|&v| v == 0.0))
                                    .cloned()
                                    .collect()
                            } else {
                                gdf_insts
                            };
                            // Rebuild + re-upload the GDF only when the static
                            // geometry/materials/bounds hash changes, not every
                            // trace, so a static scene keeps a sharp cached field
                            // for free while lights/emitters/dynamics move.
                            let gh = hash_gdf_inputs(&gdf_insts, gmin, gmax, dims);
                            if self.gdf_hash != Some(gh) {
                                let gdf = crate::sw_gi::build_gdf(&gdf_insts, gmin, gmax, dims);
                                let materials: Vec<_> = gdf_insts
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
                            // Feed the same emitter spheres to the screen-space GI
                            // gather (analytic NEE → smooth emitter bounce).
                            renderer.gi_set_emitters(&emitters);
                            // Mark active so the change-detect can settle: a static
                            // scene stops re-feeding, a moving emitter (hash change)
                            // resumes it. Without this the Flux-only path (no probe
                            // ingest to set it) would re-feed every frame forever.
                            self.active = true;
                            // Flux drives the main view from the GDF + emitters set
                            // above. The legacy world-probe DDGI march is ONLY for
                            // the in-game camera / off-screen fallback, so skip it
                            // entirely when the fallback is Off — that's the "only
                            // Flux runs" path (and kills the redundant march cost).
                            let run_probes =
                                gi.probe_fallback != citrus_assets::ProbeFallback::Off;
                            let march = citrus_render::GpuGiMarch {
                                lights: &gather.lights,
                                emitters: &emitters,
                                probes: &cascade_probes,
                                samples: gi.samples,
                                bounces: gi.bounces,
                                sky: gather.sky_color,
                                eps,
                                max_dist,
                                seed: self.seed,
                            };
                            // Submit asynchronously; the result is collected by
                            // the poll in step 1 next frame, so the (potentially
                            // heavy) march never blocks the frame.
                            if run_probes && renderer.gi_march_begin(&march) {
                                let (vols, counts) = pending.take().unwrap();
                                self.gpu_pending = Some((vols, counts, k));
                            }
                        }
                        // No GPU compute (or the march failed) → CPU march thread.
                        // Also gated: Off = Flux-only, no world-probe fallback.
                        let run_probes = gi.probe_fallback != citrus_assets::ProbeFallback::Off;
                        if run_probes
                            && let Some((vols, counts)) = pending.take()
                        {
                            let lights = gather.lights.clone();
                            let probes = cascade_probes.clone();
                            let (sky, samples, seed) = (gather.sky_color, gi.samples, self.seed);
                            let bounces = gi.bounces;
                            let handle = std::thread::spawn(move || {
                                crate::sw_gi::march_probes(
                                    &insts, &lights, &probes, sky, samples, scene_size, bounces,
                                    seed,
                                )
                            });
                            self.job = Some((handle, vols, counts, k));
                        }
                    } else {
                        // Hardware ray-query: synchronous (RayQuery uses a single
                        // cascade, so this traces the whole grid; software is the
                        // amortized realtime path).
                        let input = citrus_render::BakeInput {
                            instances: &gather.instances,
                            lights: &gather.lights,
                            probes: &cascade_probes,
                            sky_color: gather.sky_color,
                            bounces: gi.bounces.clamp(0, 8),
                            samples: gi.samples.clamp(1, 1024),
                            probes_only: true,
                        };
                        match renderer.bake_lighting(&input) {
                            Ok(o) => fresh = Some((o.probes, vols, counts, k)),
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

        // 3) Ingest the freshly-traced cascade: temporally blend it into the
        // persistent `raw` field, then spatially filter that cascade into
        // `target`. Only one cascade is touched per frame (amortized).
        if let Some((mut probes, vols, counts, k)) = fresh {
            // Total probes across all cascades (the persistent field size).
            let total: usize = vols
                .iter()
                .map(|(_, _, c, _)| (c[0] * c[1] * c[2]) as usize)
                .sum();
            // Resize (or first result) → allocate persistent buffers + re-point
            // the GPU descriptors via the full upload, then fill this cascade.
            let resized = self.raw.len() != total || self.counts != counts;
            if resized {
                self.counts = counts;
                self.raw = vec![citrus_render::ProbeSh::default(); total];
                self.target = vec![citrus_render::ProbeSh::default(); total];
                self.accum = vec![citrus_render::ProbeSh::default(); total];
                renderer.set_baked_probes(&self.accum, &vols);
                self.active = true;
            }
            // Apply intensity to the fresh cascade.
            let intensity = gi.intensity.max(0.0);
            if (intensity - 1.0).abs() > 1e-3 {
                for p in &mut probes {
                    for b in 0..4 {
                        for c in 0..3 {
                            p.coeffs[b][c] *= intensity;
                        }
                    }
                }
            }
            // Locate this cascade's slice in the flat field.
            let (cnt, base) = {
                let (_, _, c, b) = &vols[k.min(vols.len().saturating_sub(1))];
                ([c[0] as usize, c[1] as usize, c[2] as usize], *b as usize)
            };
            let n = cnt[0] * cnt[1] * cnt[2];
            if base + n <= self.raw.len() && probes.len() == n {
                // Temporal blend into raw. Snap on resize / while moving; average
                // gently when static so residual variance converges smoothly.
                let alpha = if resized {
                    1.0
                } else if self.moving {
                    0.2 + 0.8 * gi.temporal_blend.clamp(0.0, 1.0)
                } else {
                    0.03
                };
                for (t, f) in self.raw[base..base + n].iter_mut().zip(&probes) {
                    for b in 0..4 {
                        for c in 0..3 {
                            t.coeffs[b][c] += (f.coeffs[b][c] - t.coeffs[b][c]) * alpha;
                        }
                        t.dist[b] += (f.dist[b] - t.dist[b]) * alpha;
                    }
                }
                // Spatially filter this cascade (from raw → target). Recomputed
                // from raw each time so repeated frames don't over-blur. Software
                // grids are coarser → blur harder (also softens trilinear facets).
                let iters = if gi.mode == citrus_assets::GiMode::Software { 6 } else { 2 };
                let mut filtered = self.raw[base..base + n].to_vec();
                crate::sw_gi::blur_probe_grid(&mut filtered, cnt, iters);
                self.target[base..base + n].copy_from_slice(&filtered);
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
    for (i, inst) in gather.instances.iter().enumerate() {
        let is_static = gather.instance_static.get(i).copied().unwrap_or(true);
        let emissive = inst.emission.iter().any(|&v| v != 0.0);
        // A dynamic, non-emissive object doesn't affect the GI (it's excluded
        // from the static GDF and casts no light), so skip it — otherwise a prop
        // falling/jittering under physics would re-trace the probes every frame
        // and never let a Play-mode scene settle.
        if !is_static && !emissive {
            continue;
        }
        // Quantize to ~1mm so a resting body's solver jitter doesn't re-trace.
        for v in inst.transform.to_cols_array() {
            f((v * 1024.0).round() / 1024.0, &mut h);
        }
        // Material emission + albedo affect the bounce, so editing an emitter's
        // strength/colour (or a surface's albedo) must re-trace the GI — not just
        // moving the object. Without these the GI only updated on a transform change.
        for v in inst.emission.iter().chain(inst.albedo.iter()) {
            f(*v, &mut h);
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
    // The march backend (software SDF vs hardware ray-query) produces different
    // results, so toggling it must re-trace even when nothing else changed.
    gi.mode.hash(&mut h);
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
