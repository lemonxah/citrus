#version 450
#ifdef MULTIVIEW
#extension GL_EXT_multiview : require
#endif
// citrus standard shader, fragment stage (phase 1: PBR/toon hybrid core).
// Feature toggles are specialization constants: each material's enabled
// feature set selects a pipeline variant; disabled features compile out.

layout(constant_id = 0) const bool FEAT_TOON = false;
layout(constant_id = 1) const bool FEAT_NORMAL_MAP = false;
layout(constant_id = 2) const bool FEAT_EMISSION = false;
layout(constant_id = 3) const uint ALPHA_MODE = 0u; // 0 opaque, 1 cutout, 2 blend
layout(constant_id = 4) const bool FEAT_WATER = false;

#include "standard_common.glsl"

// Water: copies of the opaque scene (depth, HDR colour) taken before the
// transparent draws; valid for this pass when frame.water.y > 0.5.
layout(set = 0, binding = 5) uniform sampler2D u_opaque_depth;
layout(set = 0, binding = 6) uniform sampler2D u_opaque_color;
// Reflection probe: prefiltered environment cubemap (mirror at mip 0, rougher
// toward higher mips). Sampled by reflection direction + roughness.
layout(set = 0, binding = 7) uniform samplerCube u_env;
// Second reflection probe, blended per pixel with u_env by box influence.
layout(set = 0, binding = 9) uniform samplerCube u_env_b;
// Third/fourth reflection probes.
layout(set = 0, binding = 11) uniform samplerCube u_env_c;
layout(set = 0, binding = 12) uniform samplerCube u_env_d;
// Planar reflections: per-plane mirrored scene renders, sampled at screen UV
// by surfaces whose push constant carries a planar layer (pc.emission.w >= 1).
layout(set = 0, binding = 8) uniform sampler2DArray u_planar;
// Each water body's ripple field by body slot (r height, gb local XZ
// gradient, with mips): the caustics on what lies under it.
layout(set = 0, binding = 10) uniform sampler2D u_water_ripples[MAX_WATER_BODIES];
// Max roughness mip = (mip count - 1) of the CURRENTLY bound cube, queried at
// runtime so it's correct whether u_env is the 64px skybox cube (7 mips) or a
// higher-res captured reflection-probe cube (e.g. 256px = 9 mips). A hardcoded
// constant under-blurred probe reflections (sampled mip 6 of 9 -> too sharp on
// rough surfaces, the "defined blob on a rough floor" artifact). See
// REFLECTION_PROBES_MATH.md 6.5.
#define ENV_MAX_LOD (float(textureQueryLevels(u_env) - 1))

layout(set = 0, binding = 1) uniform sampler2DArrayShadow u_shadow;

// Baked probe SH-L1: 4 coefficients (RGB in .xyz) per probe.
struct Probe { vec4 c[6]; }; // c[0..3]=radiance+dist SH, c[4]=dist² SH (Chebyshev)
layout(set = 0, binding = 2) readonly buffer Probes { Probe probes[]; };

// Baked lightmaps (static-object GI): one array layer per object, sampled by
// uv1. Each texel is incoming irradiance E; the caller applies albedo/PI.
layout(set = 0, binding = 3) uniform sampler2DArray u_lightmap;

// Screen-space GI gather (sparse screen probes). One texel per
// SGI_DIV×SGI_DIV screen block; rgb = indirect irradiance, a = probe camera
// distance (for depth-aware upsampling). Used in place of the coarse world-probe
// grid when active (frame.debug.z > 0.5). Sampled by screen UV.
layout(set = 0, binding = 4) uniform sampler2D u_screen_gi;
// Must match SCREEN_PROBE_DIV in lib.rs (probe grid = screen / SGI_DIV).
const float SGI_DIV = 4.0;

// Bilateral spatial filter + depth-aware upsample of the sparse screen probes:
// a wide (5×5 probe) edge-aware gather. Each probe is weighted by a spatial
// Gaussian (distance to the pixel in probe space) AND by how close its stored
// camera distance is to this fragment's, so it smooths the few-ray noise
// (a screen-probe spatial filter) while rejecting probes across a depth
// edge (no bleed/blockiness). This is the main grain reduction.
// `P` is the fragment's VIEW-space position and `Nv` its VIEW-space normal, used
// to make the depth-edge test PLANE-aware: each neighbour's distance is compared
// to what it WOULD be if it lay on this fragment's surface plane, so coplanar
// neighbours on a steep/grazing face (where raw camera distance varies a lot
// across the surface) are kept instead of rejected. Comparing against a flat
// camera-distance gave undersampled, noisy/blocky GI on tilted faces (the floor,
// facing the camera, looked fine; vertical cube faces did not).
// The screen-GI gather (screen_gi.comp) runs at FULL resolution, so this is a
// full-res bilateral DENOISE, not a probe-grid upsample: a small à-trous-style
// kernel that averages neighbouring GI samples on the same surface. The edge
// test is PLANE-aware (predict each tap's distance on this fragment's plane via
// its view ray ∩ the plane), so coplanar neighbours on a steep/grazing face are
// kept — the old version snapped to a coarse ¼-res lattice and looked smooth only
// on camera-facing surfaces (the floor), leaving tilted faces blocky/noisy.
vec3 screen_gi_upsample(vec2 suv, vec3 P, vec3 Nv) {
    vec2 screen = vec2(frame.postfx2.w, frame.postfx3.w);
    vec2 texel = 1.0 / max(screen, vec2(1.0)); // full-res pixel size in UV
    float frag_dist = length(P);
    const int R = 2;          // 5×5 taps
    const float STEP = 4.0;   // pixels between taps (≈17px-wide denoise)
    vec3 sum = vec3(0.0);
    float wsum = 0.0;
    // View rays through the taps: affine in NDC for a perspective inverse, so
    // build the centre ray + per-tap steps once instead of an inv_proj multiply
    // per tap (25 per fragment).
    vec2 cndc = suv * 2.0 - 1.0;
    cndc.y = -cndc.y;
    vec4 f0 = frame.inv_proj * vec4(cndc, 1.0, 1.0);
    vec4 fx = frame.inv_proj * vec4(cndc + vec2(2.0 * STEP * texel.x, 0.0), 1.0, 1.0);
    vec4 fy = frame.inv_proj * vec4(cndc - vec2(0.0, 2.0 * STEP * texel.y), 1.0, 1.0);
    vec3 r0 = f0.xyz / f0.w;
    vec3 rx = fx.xyz / fx.w - r0;
    vec3 ry = fy.xyz / fy.w - r0;
    for (int j = -R; j <= R; ++j) {
        for (int i = -R; i <= R; ++i) {
            vec2 off = vec2(i, j);
            vec2 uv = clamp(suv + off * STEP * texel, vec2(0.0), vec2(1.0));
            vec4 s = texture(u_screen_gi, uv);
            float sw = exp(-dot(off, off) * 0.5); // spatial Gaussian
            // Edge-reject test, kept permissive on CURVED/angled surfaces. Two
            // measures of how far this tap's stored distance (s.a) is from this
            // surface, both relative to fragment distance:
            //   - plane: distance to the fragment's tangent plane along the tap
            //     ray (handles steep/grazing FLAT faces, where camera distance
            //     varies a lot across a planar surface), and
            //   - radial: plain |s.a - frag_dist| (handles CURVED faces, where
            //     neighbours leave the tangent plane but stay close in depth).
            // Taking the MIN accepts a tap if EITHER holds, so a sphere keeps its
            // neighbours (was rejected by the plane test alone -> ~1 effective tap
            // -> raw ray noise on every angled surface) while a true depth edge,
            // far in both, is still rejected. Softer falloff (×5) widens the kept
            // set so high-intensity GI isn't left grainy on curvature.
            vec3 dir = normalize(r0 + rx * off.x + ry * off.y);
            float denom = dot(Nv, dir);
            float pred = abs(denom) > 1e-4 ? length(dir * (dot(Nv, P) / denom)) : frag_dist;
            float inv_fd = 1.0 / max(frag_dist, 0.5);
            float dd_plane = abs(s.a - pred) * inv_fd;
            float dd_radial = abs(s.a - frag_dist) * inv_fd;
            float dd = min(dd_plane, dd_radial);
            float dw = exp(-dd * 3.5); // curvature-tolerant edge reject (softened for spheres)
            float w = sw * dw + 1e-6;
            sum += s.rgb * w;
            wsum += w;
        }
    }
    return wsum > 1e-6 ? sum / wsum : texture(u_screen_gi, suv).rgb;
}

// Diffuse irradiance / PI from the probe's SH-L1 radiance coefficients, in
// direction n. The bake stores radiance projected onto SH (Y0 = 0.282095,
// Y1 = 0.488603; sh1~y, sh2~z, sh3~x). Converting radiance SH to Lambertian
// irradiance applies the cosine-lobe band factors A0 = PI, A1 = 2PI/3; dividing
// by PI (the diffuse BRDF) leaves the L1 band scaled by 2/3
// (0.488603 * 2/3 = 0.325735). The constant term then matches the flat ambient.
vec3 sh_eval(uint i, vec3 n) {
    return probes[i].c[0].rgb * 0.282095
         + 0.325735 * (probes[i].c[1].rgb * n.y
                     + probes[i].c[2].rgb * n.z
                     + probes[i].c[3].rgb * n.x);
}

// RADIANCE (not irradiance) from the probe SH-L1 in direction `dir`: the raw L1
// band reconstruction (no cosine-lobe factor), used for the FluxVoxel specular-
// from-volume term. Very low-frequency, so only a rough-glossy approximation.
vec3 sh_radiance(uint i, vec3 dir) {
    return probes[i].c[0].rgb * 0.282095
         + 0.488603 * (probes[i].c[1].rgb * dir.y
                     + probes[i].c[2].rgb * dir.z
                     + probes[i].c[3].rgb * dir.x);
}

// Expected distance to geometry from probe `i` in world direction `dir`,
// reconstructed from the SH-L1 stored in the coeffs' .w lanes (raw band factors,
// no cosine convolution). Zero (the bake path) means "no visibility data".
float dist_at(uint i, vec3 dir) {
    return probes[i].c[0].w * 0.282095
         + 0.488603 * (probes[i].c[1].w * dir.y
                     + probes[i].c[2].w * dir.z
                     + probes[i].c[3].w * dir.x);
}
// Mean SQUARED distance (second moment), from the dist² SH in c[4]. Pairs with
// dist_at for the two-moment Chebyshev visibility (matches the gather).
float dist2_at(uint i, vec3 dir) {
    return probes[i].c[4].x * 0.282095
         + 0.488603 * (probes[i].c[4].y * dir.y
                     + probes[i].c[4].z * dir.z
                     + probes[i].c[4].w * dir.x);
}

// Normalized box coords (0..1 across the volume) for a world point.
vec3 volume_coords(ProbeVolume v, vec3 world_pos) {
    vec3 local = (v.world_to_local * vec4(world_pos, 1.0)).xyz;
    vec3 size = max(v.size_base.xyz, vec3(1e-4));
    return (local + size * 0.5) / size;
}

// DDGI-style 8-corner blend of probe irradiance within one volume. Each corner's
// trilinear weight is modulated by:
//   - visibility: the probe's stored directional distance vs. the actual probe→
//     fragment distance (a soft Chebyshev-lite test). A probe occluded from the
//     fragment (e.g. on the far side of a wall) is down-weighted, killing leaks,
//   - front-facing: probes roughly behind the surface contribute less.
// Falls back to plain trilinear where no visibility data exists (bake path).
vec3 sample_volume(ProbeVolume v, vec3 world_pos, vec3 n) {
    vec3 t = volume_coords(v, world_pos);
    ivec3 cnt = ivec3(v.counts.xyz + 0.5);
    vec3 size = max(v.size_base.xyz, vec3(1e-4));
    vec3 center = -(transpose(mat3(v.world_to_local)) * v.world_to_local[3].xyz); // volume origin (rotation-aware)
    vec3 cell = size / max(vec3(cnt - 1), vec3(1.0));
    vec3 g = clamp(t, vec3(0.0), vec3(1.0)) * vec3(cnt - 1);
    ivec3 g0 = ivec3(floor(g));
    vec3 f = g - vec3(g0);
    // Smootherstep (quintic) the interpolation factor so the trilinear blend is
    // C2-continuous across cell boundaries. Plain linear trilinear kinks the
    // gradient at every boundary, and even smoothstep (cubic, C1) leaves a jump in
    // the SECOND derivative there — which the eye still reads as faint grid lines
    // (Mach banding) between voxels. The quintic is C2, so the cell edges blend
    // away entirely without needing a finer grid.
    f = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);

    vec3 sum = vec3(0.0);
    float wsum = 0.0;
    for (int i = 0; i < 8; ++i) {
        ivec3 off = ivec3(i & 1, (i >> 1) & 1, (i >> 2) & 1);
        ivec3 gc = clamp(g0 + off, ivec3(0), cnt - 1);
        vec3 tw = mix(vec3(1.0) - f, f, vec3(off));
        float w = tw.x * tw.y * tw.z;
        uint idx = uint(v.size_base.w) + uint(gc.x + gc.y * cnt.x + gc.z * cnt.x * cnt.y);
        // Probe world position from its grid coords.
        vec3 gn = vec3(gc) / max(vec3(cnt - 1), vec3(1.0));
        vec3 ppos = center + transpose(mat3(v.world_to_local)) * ((gn - 0.5) * size) + probes[idx].c[5].xyz;
        vec3 to_frag = world_pos - ppos;
        float pd = length(to_frag);
        vec3 dir = pd > 1e-5 ? to_frag / pd : n;
        float md = max(dist_at(idx, dir), 0.0);
        float md2 = max(dist2_at(idx, dir), 0.0);
        // Visibility (DDGI two-moment Chebyshev, matching the gather): smooth
        // occlusion from the distance variance, so it darkens the bounce behind
        // an occluder as a soft gradient. md==0 → no data; md2==0 → baked sidecar
        // (single-moment smoothstep fallback). Keeps a small floor so shadowed
        // faces still gather soft fill instead of crushing to black.
        float band = 3.0 * max(cell.x, max(cell.y, cell.z));
        float vis;
        if (md <= 1e-4 || pd <= md) {
            vis = 1.0;
        } else if (md2 > 1e-4) {
            float variance = max(md2 - md * md, 1e-4);
            float dd = pd - md;
            // Higher floor (0.5) = gentler occlusion, so any residual per-probe
            // moment variation reads as a soft contact-shadow, not hard grid dots.
            vis = max(variance / (variance + dd * dd), 0.5);
        } else {
            vis = clamp(1.0 - (pd - md) / max(band, 1e-3), 0.5, 1.0);
        }
        // Front-facing weight (probe should be on the surface's lit side). Kept
        // gentle + a generous floor so shadowed/back faces still gather soft fill
        // from surrounding probes instead of crushing to black. Leak prevention
        // is the visibility term's job, not this one.
        float nw = clamp(dot(-dir, n) * 0.5 + 0.5, 0.0, 1.0);
        nw = nw * 0.65 + 0.35;
        w *= vis * nw;
        sum += w * sh_eval(idx, n);
        wsum += w;
    }
    return wsum > 1e-5 ? max(sum / wsum, vec3(0.0)) : vec3(0.0);
}

// Trilinearly-interpolated baked irradiance at world_pos for normal n, plus a
// coverage weight in [0,1] for the caller to blend against ambient. Volumes are
// ordered finest-first (cascades): the first that contains the point wins, but
//   - near an INNER cascade's boundary we cross-fade into the next (coarser)
//     cascade so the resolution change isn't a visible seam (coverage stays 1),
//   - the OUTERMOST cascade keeps full coverage everywhere inside its box and
//     fades to ambient over one probe cell OUTSIDE it (DDGI). Fading inside the
//     box starved the surfaces volumes are built for: a floor resting on the
//     bottom face got ~2% of its GI, leaving only the probe dots visible.

// How far a point (volume coords `t`) lies outside volume `v`, in probe cells.
float volume_outside_cells(ProbeVolume v, vec3 t) {
    vec3 cells = max(v.counts.xyz - 1.0, vec3(1.0));
    vec3 out_g = max(max(-t, t - 1.0), vec3(0.0)) * cells;
    return max(out_g.x, max(out_g.y, out_g.z));
}

// `world_pos` moved onto the nearest point of volume `v`'s box.
vec3 volume_clamp(ProbeVolume v, vec3 world_pos) {
    vec3 half_size = max(v.size_base.xyz, vec3(1e-4)) * 0.5;
    vec3 local = clamp((v.world_to_local * vec4(world_pos, 1.0)).xyz, -half_size, half_size);
    mat3 local_to_world = transpose(mat3(v.world_to_local));
    return local_to_world * (local - v.world_to_local[3].xyz);
}

float sample_probes(vec3 world_pos, vec3 n, out vec3 irradiance) {
    int vcount = int(frame.misc.w + 0.5);
    for (int vi = 0; vi < vcount && vi < MAX_PROBE_VOLUMES; ++vi) {
        ProbeVolume v = frame.probe_volumes[vi];
        vec3 t = volume_coords(v, world_pos);
        bool has_next = (vi + 1 < vcount && vi + 1 < MAX_PROBE_VOLUMES);
        float outside = volume_outside_cells(v, t);
        if (outside > 0.0 && (has_next || outside >= 1.0)) {
            continue;
        }
        if (outside > 0.0) {
            irradiance = sample_volume(v, volume_clamp(v, world_pos), n);
            return 1.0 - smoothstep(0.0, 1.0, outside);
        }
        vec3 fine = sample_volume(v, world_pos, n);
        // Distance to the nearest box face in normalized units (0 at the face,
        // 0.5 at the center).
        vec3 edge = min(t, vec3(1.0) - t);
        float e = min(edge.x, min(edge.y, edge.z));
        if (has_next) {
            // Wide, smooth cross-fade into the coarser cascade hides the band.
            float inner = smoothstep(0.0, 0.22, e);
            if (inner < 1.0) {
                ProbeVolume v2 = frame.probe_volumes[vi + 1];
                vec3 coarse = sample_volume(v2, world_pos, n);
                fine = mix(coarse, fine, inner);
            }
            irradiance = fine;
            return 1.0;
        }
        irradiance = fine;
        return 1.0;
    }
    irradiance = vec3(0.0);
    return 0.0;
}

// FluxVoxel specular-from-volume (VXGI-style glossy approximation): trilinearly
// sample the probe volume's RADIANCE in the reflection direction `dir`, so a
// metallic/rough surface receives the emissive + voxel-light bounce held in the
// volume — not just the reflection cube. Coarse (SH-L1 is very low frequency), so
// it's only used for rough-glossy surfaces. Returns false when `world_pos` is
// outside every volume. No cascade cross-fade / visibility — a glossy fill term.
bool volume_radiance(vec3 world_pos, vec3 dir, out vec3 rad) {
    int vcount = int(frame.misc.w + 0.5);
    for (int vi = 0; vi < vcount && vi < MAX_PROBE_VOLUMES; ++vi) {
        ProbeVolume v = frame.probe_volumes[vi];
        vec3 t = volume_coords(v, world_pos);
        if (any(lessThan(t, vec3(0.0))) || any(greaterThan(t, vec3(1.0)))) {
            continue;
        }
        ivec3 cnt = ivec3(v.counts.xyz + 0.5);
        vec3 g = clamp(t, vec3(0.0), vec3(1.0)) * vec3(cnt - 1);
        ivec3 g0 = ivec3(floor(g));
        vec3 f = g - vec3(g0);
        f = f * f * f * (f * (f * 6.0 - 15.0) + 10.0); // smootherstep (C2): blend voxel edges
        vec3 sum = vec3(0.0);
        float wsum = 0.0;
        for (int i = 0; i < 8; ++i) {
            ivec3 off = ivec3(i & 1, (i >> 1) & 1, (i >> 2) & 1);
            ivec3 gc = clamp(g0 + off, ivec3(0), cnt - 1);
            vec3 tw = mix(vec3(1.0) - f, f, vec3(off));
            float w = tw.x * tw.y * tw.z;
            uint idx = uint(v.size_base.w) + uint(gc.x + gc.y * cnt.x + gc.z * cnt.x * cnt.y);
            sum += w * max(sh_radiance(idx, dir), vec3(0.0));
            wsum += w;
        }
        rad = wsum > 1e-5 ? sum / wsum : vec3(0.0);
        return true;
    }
    return false;
}

// Smooth probe irradiance in normal `n` — pure trilinear `sh_eval`, NO Chebyshev /
// front-facing weighting. Used to add realtime voxel GI on top of a LIGHTMAP: the
// bake already carries the static occlusion, so the per-probe Chebyshev variation
// (which draws the grid as fixed dots on bright baked surfaces) isn't wanted here.
vec3 sample_irradiance_smooth(vec3 world_pos, vec3 n) {
    int vcount = int(frame.misc.w + 0.5);
    for (int vi = 0; vi < vcount && vi < MAX_PROBE_VOLUMES; ++vi) {
        ProbeVolume v = frame.probe_volumes[vi];
        vec3 t = volume_coords(v, world_pos);
        // Outermost volume: a one-cell margin outside the box (as sample_probes).
        bool has_next = (vi + 1 < vcount && vi + 1 < MAX_PROBE_VOLUMES);
        float outside = volume_outside_cells(v, t);
        if (outside > 0.0 && (has_next || outside >= 1.0)) {
            continue;
        }
        float fade = 1.0 - smoothstep(0.0, 1.0, outside);
        ivec3 cnt = ivec3(v.counts.xyz + 0.5);
        vec3 g = clamp(t, vec3(0.0), vec3(1.0)) * vec3(cnt - 1);
        ivec3 g0 = ivec3(floor(g));
        vec3 f = g - vec3(g0);
        // Plain trilinear: smoothstep-style weights flatten near each probe, so
        // every probe owns a plateau and the field reads as a grid.
        mat3 local_to_world = transpose(mat3(v.world_to_local));
        vec3 center = -(local_to_world * v.world_to_local[3].xyz);
        vec3 size = max(v.size_base.xyz, vec3(1e-4));
        vec3 sum = vec3(0.0);
        float wsum = 0.0;
        for (int i = 0; i < 8; ++i) {
            ivec3 off = ivec3(i & 1, (i >> 1) & 1, (i >> 2) & 1);
            ivec3 gc = clamp(g0 + off, ivec3(0), cnt - 1);
            vec3 tw = mix(vec3(1.0) - f, f, vec3(off));
            float w = tw.x * tw.y * tw.z;
            // Backface weight (DDGI): a probe behind this surface (under a floor
            // that sits just above the volume's bottom layer) sees only the
            // back of it, so it must not darken the surface it can't see.
            vec3 gn = vec3(gc) / max(vec3(cnt - 1), vec3(1.0));
            vec3 to_probe = center + local_to_world * ((gn - 0.5) * size) - world_pos;
            // A step across the surface plane, not an angle falloff: weighting
            // front probes by angle changes non-linearly between probes and
            // shows each one as a bump.
            float facing = dot(to_probe, to_probe) > 1e-8 ? smoothstep(-0.15, 0.05, dot(normalize(to_probe), n)) : 1.0;
            w *= facing + 1e-3;
            uint idx = uint(v.size_base.w) + uint(gc.x + gc.y * cnt.x + gc.z * cnt.x * cnt.y);
            sum += w * sh_eval(idx, n);
            wsum += w;
        }
        return wsum > 1e-5 ? max(sum / wsum, vec3(0.0)) * fade : vec3(0.0);
    }
    return vec3(0.0);
}

// Returns 1.0 fully lit, 0.0 fully shadowed. `light.spot` packs
// (cos_inner, shadow_base_layer, bias, view_count); base < 0 = no shadow.
// `nrm` is the surface normal and `ldir` the direction to the light (both
// unit), used to slope-scale the depth bias so grazing receivers don't alias.
float shadow_factor(Light light, vec3 world_pos, vec3 nrm, vec3 ldir) {
    int base = int(light.spot.y);
    if (base < 0) {
        return 1.0;
    }
    float bias = light.spot.z;
    // spot.w packs the shadow view count; its sign is the filter mode
    // (positive = soft/PCF, negative = hard/single tap).
    bool soft = light.spot.w >= 0.0;
    int vcount = int(abs(light.spot.w) + 0.5);
    int kind = int(light.pos_kind.w + 0.5);
    int sub = 0;
    if (kind == 0 && vcount > 1) {
        // Cascaded directional. View-space depth gives a starting cascade, but
        // the sphere-fit cascades don't align exactly with the depth slices, so
        // a boundary fragment can project just outside the chosen cascade's map
        // and read the lit border -> a bright seam. Walk up to the first cascade
        // whose projected uv sits inside [margin, 1-margin] (margin = PCF kernel
        // radius, so taps never reach the border). Coarser cascades enclose the
        // finer ones, so this always converges.
        float depth = -(VIEW_MATRIX * vec4(world_pos, 1.0)).z;
        int start = vcount - 1;
        for (int c = 0; c < vcount && c < 4; ++c) {
            if (depth < frame.cascade_splits[c]) { start = c; break; }
        }
        float margin = frame.misc.z * float(2 + 1); // PCF radius R=2, +1 slack
        sub = vcount - 1;
        for (int c = start; c < vcount && c < 4; ++c) {
            vec4 cc = frame.shadow_vp[base + c] * vec4(world_pos, 1.0);
            if (cc.w <= 0.0) { sub = c; continue; }
            vec2 cuv = (cc.xy / cc.w) * 0.5 + 0.5;
            sub = c;
            if (cuv.x >= margin && cuv.x <= 1.0 - margin
                && cuv.y >= margin && cuv.y <= 1.0 - margin) {
                break;
            }
        }
    } else if (vcount == 6) {
        // Point light: pick the cube face from the dominant axis.
        vec3 fl = world_pos - light.pos_kind.xyz;
        vec3 a = abs(fl);
        if (a.x >= a.y && a.x >= a.z) sub = fl.x > 0.0 ? 0 : 1;
        else if (a.y >= a.z) sub = fl.y > 0.0 ? 2 : 3;
        else sub = fl.z > 0.0 ? 4 : 5;
    }
    int layer = base + sub;
    vec4 lc = frame.shadow_vp[layer] * vec4(world_pos, 1.0);
    if (lc.w <= 0.0) {
        return 1.0;
    }
    vec3 proj = lc.xyz / lc.w;
    vec2 uv = proj.xy * 0.5 + 0.5;
    // Slope-scaled depth bias: grazing receivers (small NdotL) need a larger
    // offset to avoid self-shadow aliasing. A small constant floor keeps flat
    // surfaces clean even when the per-light bias is left at its default. The
    // bias only pushes the receiver away from the light (acne), never toward it
    // (which would re-introduce a leak).
    float ndotl = clamp(dot(nrm, ldir), 0.0, 1.0);
    float slope = sqrt(max(1.0 - ndotl * ndotl, 0.0)) / max(ndotl, 0.2);
    float depth_bias = (bias + 0.0008) * (1.0 + slope);
    float ref = proj.z - depth_bias;
    // Beyond the far plane there is no occluder. For directional/spot, a uv
    // outside the map means outside the light's frustum -> unshadowed. Point
    // lights tile 6 overlapping faces, so a fragment is always covered by its
    // selected face; per-tap clamping below keeps PCF off the atlas border
    // (which reads as lit and would draw a seam between faces).
    bool is_point = (vcount == 6);
    if (ref > 1.0) {
        return 1.0;
    }
    if (!is_point && (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0)) {
        return 1.0;
    }
    // Hard shadows: a single depth comparison (sharp, aliased edge).
    if (!soft) {
        return texture(u_shadow, vec4(clamp(uv, vec2(0.0), vec2(1.0)), float(layer), ref));
    }
    // Soft shadows, 5x5 PCF: average many hardware-PCF taps to soften the
    // shadow edge. Tap spacing (softness / shadow_resolution) comes from the
    // CPU so it tracks the runtime shadow resolution + softness setting.
    float spacing = frame.misc.z;
    const int R = 2;
    float sum = 0.0;
    for (int dx = -R; dx <= R; ++dx) {
        for (int dy = -R; dy <= R; ++dy) {
            vec2 off = vec2(float(dx), float(dy)) * spacing;
            vec2 suv = clamp(uv + off, vec2(0.0), vec2(1.0));
            sum += texture(u_shadow, vec4(suv, float(layer), ref));
        }
    }
    float n = float((2 * R + 1) * (2 * R + 1));
    return sum / n;
}

layout(set = 1, binding = 0) uniform sampler2D t_albedo;
layout(set = 1, binding = 1) uniform sampler2D t_normal;
layout(set = 1, binding = 2) uniform sampler2D t_orm; // R occlusion, G roughness, B metallic
layout(set = 1, binding = 3) uniform sampler2D t_emission;


// Split AO / Roughness / Metallic maps. Each multiplies the matching packed-ORM
// channel; the 1×1 default is white, so an unassigned slot is a no-op.
layout(set = 1, binding = 13) uniform sampler2D t_ao;
layout(set = 1, binding = 14) uniform sampler2D t_roughness;
layout(set = 1, binding = 15) uniform sampler2D t_metallic;
// Height / displacement map for parallax occlusion mapping (white = flat).
layout(set = 1, binding = 16) uniform sampler2D t_displacement;

// Extended texture slots (bindings 5-12).
layout(set = 1, binding = 5) uniform sampler2D t_opacity;
layout(set = 1, binding = 6) uniform sampler2D t_emission_mask;
layout(set = 1, binding = 7) uniform sampler2D t_matcap0;
layout(set = 1, binding = 8) uniform sampler2D t_matcap0_mask;
layout(set = 1, binding = 9) uniform sampler2D t_matcap1;
layout(set = 1, binding = 10) uniform sampler2D t_matcap1_mask;
layout(set = 1, binding = 11) uniform sampler2D t_matcap2;
layout(set = 1, binding = 12) uniform sampler2D t_matcap2_mask;

layout(push_constant) uniform Push {
    // The model matrix comes from the instance stream; column 0 here holds the
    // 128-bit mask of lights that can reach this draw (uint bits).
    mat4 model;
    vec4 base_color;
    vec4 emission;
    vec4 params0; // metallic, roughness, toon_steps, pbr_toon_blend
    vec4 params1; // alpha_cutoff, normal_strength, occlusion_strength, _
} pc;

layout(location = 0) in vec3 v_world_pos;
layout(location = 1) in vec3 v_normal;
layout(location = 2) in vec2 v_uv;
layout(location = 3) in vec4 v_color;
layout(location = 4) in vec4 v_tangent;
layout(location = 5) in vec2 v_uv1;
// Baked-lightmap layer (or the lightmap-UV preview's resolution), see standard.vert.
layout(location = 6) flat in float v_lightmap;

layout(location = 0) out vec4 o_color;
// Deferred-SSR G-buffer (gbuf pipeline variants only): the specular reflectance
// weight (Fresnel*ao*spec, the factor multiplying environment radiance) in rgb,
// surface roughness in alpha. The fullscreen SSR resolve pass reads this to do
// current-frame screen-space reflections without the forward pass's 1-frame lag.
layout(location = 1) out vec4 o_gbuf;

const float PI = 3.14159265359;

// Narkowicz ACES filmic tonemap: rolls HDR highlights off to [0,1] so bright
// surfaces (a close point light, stacked ambient + baked bounce) show detail
// instead of clipping to flat white. Operates in linear; the sRGB swapchain
// applies gamma on write.
vec3 tonemap_aces(vec3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), 0.0, 1.0);
}

// Combine one matcap layer with the colour beneath it. `t` is the layer's
// mask*strength weight; `mode` selects 0 add / 1 multiply / 2 replace.
vec3 blend_matcap(vec3 base, vec3 mc, float t, float mode) {
    if (mode > 1.5) return mix(base, mc, t);          // replace
    if (mode > 0.5) return mix(base, base * mc, t);   // multiply
    return base + mc * t;                             // add
}

// Per-pixel post from the blended Volume profile: exposure → grading → tonemap →
// vignette. (Chromatic aberration + bloom need a fullscreen pass; follow-up.)
vec3 apply_postfx(vec3 color, vec2 fragcoord) {
    // HDR output (debug.w): the fullscreen post pass does exposure/grade/tonemap/
    // bloom/vignette, so surfaces output linear radiance unchanged.
    if (frame.debug.w > 0.5) {
        return color;
    }
    color *= exp2(frame.postfx0.y);
    if (frame.postfx1.w > 0.5) {
        color *= exp2(frame.postfx0.z);
        float temp = frame.postfx1.y, tint = frame.postfx1.z;
        color.r *= 1.0 + temp * 0.2 + tint * 0.1;
        color.g *= 1.0 - tint * 0.2;
        color.b *= 1.0 - temp * 0.2 + tint * 0.1;
        color = (color - 0.18) * frame.postfx0.w + 0.18;
        float l = dot(color, vec3(0.2126, 0.7152, 0.0722));
        color = mix(vec3(l), color, frame.postfx1.x);
        color = max(color, vec3(0.0));
    }
    int mode = int(frame.postfx0.x + 0.5);
    if (mode == 1) {
        color = color / (color + vec3(1.0));
    } else if (mode == 2) {
        color = tonemap_aces(color);
    }
    if (frame.postfx2.x > 0.5) {
        vec2 uv = fragcoord / vec2(max(frame.postfx2.w, 1.0), max(frame.postfx3.w, 1.0));
        float dist = length(uv - 0.5) * 1.41421356;
        float sm = max(frame.postfx2.z, 1e-3);
        float mask = clamp((dist - (1.0 - sm)) / sm, 0.0, 1.0) * frame.postfx2.y;
        color = mix(color, frame.postfx3.xyz, mask);
    }
    return color;
}

// Radius (m) every punctual light is treated as for specular highlights.
const float LIGHT_SOURCE_RADIUS = 0.05;

float d_ggx(float NdotH, float a) {
    float a2 = a * a;
    float d = NdotH * NdotH * (a2 - 1.0) + 1.0;
    return a2 / max(PI * d * d, 1e-6);
}

float g_smith(float NdotV, float NdotL, float roughness) {
    float r = roughness + 1.0;
    float k = (r * r) / 8.0;
    float gv = NdotV / (NdotV * (1.0 - k) + k);
    float gl = NdotL / (NdotL * (1.0 - k) + k);
    return gv * gl;
}

vec3 f_schlick(float VdotH, vec3 f0) {
    return f0 + (1.0 - f0) * pow(clamp(1.0 - VdotH, 0.0, 1.0), 5.0);
}

// Karis' analytic split-sum environment BRDF (the "EnvBRDFApprox"): the
// scale+bias for the prefiltered environment specular, so reflections are
// roughness- and view-correct and DON'T spike to a harsh mirror at grazing the
// way a raw Fresnel term does. Returns (scale, bias): spec = f0*scale + bias.
vec2 env_brdf_approx(float roughness, float NoV) {
    const vec4 c0 = vec4(-1.0, -0.0275, -0.572, 0.022);
    const vec4 c1 = vec4(1.0, 0.0425, 1.04, -0.04);
    vec4 r = roughness * c0 + c1;
    float a004 = min(r.x * r.x, exp2(-9.28 * NoV)) * r.x + r.y;
    return vec2(-1.04, 1.04) * a004 + r.zw;
}

// Octahedral encode of a unit vector into [0,1]^2 (lossless enough at 16f). The
// SSR G-buffer stores the view-space shading normal this way so the resolve pass
// gets the EXACT normal (no depth reconstruction = no bent reflection rays).
vec2 oct_encode(vec3 n) {
    n /= (abs(n.x) + abs(n.y) + abs(n.z));
    vec2 e = n.z >= 0.0
        ? n.xy
        : (1.0 - abs(n.yx)) * vec2(n.x >= 0.0 ? 1.0 : -1.0, n.y >= 0.0 ? 1.0 : -1.0);
    return e * 0.5 + 0.5;
}

// Roughness-aware Fresnel for the ambient/indirect term (rougher surfaces
// reflect less at grazing angles, so more energy stays in the diffuse).
vec3 f_schlick_rough(float cosT, vec3 f0, float rough) {
    vec3 fmax = max(vec3(1.0 - rough), f0);
    return f0 + (fmax - f0) * pow(clamp(1.0 - cosT, 0.0, 1.0), 5.0);
}

// Parallax occlusion mapping: march the tangent-space view ray through the
// height field and return the UV of the first intersection, giving a flat
// surface apparent depth without extra geometry. `uv` is the (tiled) start
// coordinate, `vt` the tangent-space view direction (surface->eye), `scale` the
// max UV shift at full height. Height map convention: white = peak (depth 0).
vec2 parallax_uv(vec2 uv, vec3 vt, float scale) {
    const int MAX_L = 32;
    // More layers at grazing angles (where the parallax shift is largest).
    float n = mix(float(MAX_L), 8.0, clamp(abs(vt.z), 0.0, 1.0));
    float layer_d = 1.0 / n;
    vec2 step_uv = (vt.xy / max(abs(vt.z), 0.1)) * scale / n;
    vec2 cur = uv;
    float cur_d = 0.0;
    float h = 1.0 - texture(t_displacement, cur).r;
    for (int i = 0; i < MAX_L; ++i) {
        if (cur_d >= h) break;
        cur -= step_uv;
        h = 1.0 - texture(t_displacement, cur).r;
        cur_d += layer_d;
    }
    // Soft occlusion: interpolate between the last two steps across the surface.
    vec2 prev = cur + step_uv;
    float after = h - cur_d;
    float before = (1.0 - texture(t_displacement, prev).r) - (cur_d - layer_d);
    float w = after / (after - before);
    return mix(cur, prev, clamp(w, 0.0, 1.0));
}

// Box-projected parallax (Unity-style) for one reflection probe: reproject the
// reflection ray onto the probe's box so flat surfaces line up with the
// captured environment instead of treating it as infinitely distant.
// Only surfaces INSIDE the box are projected: for a surface outside it the
// ray-box hit lands on the wrong face, so the raw direction is used instead
// (REFLECTION_PROBES_MATH.md 6.4). The box center may differ from the capture
// point (Lagarde); the re-vector anchors on the capture position. A small
// tolerance keeps surfaces flush with a box face (a mirror on the wall, the
// floor) counted as inside.
vec3 probe_dir(vec3 R, vec3 capture, vec3 center, vec3 extents, bool project) {
    if (!project) return R;
    vec3 bmin = center - extents;
    vec3 bmax = center + extents;
    vec3 eps = extents * 0.02 + 0.02;
    bool inside = all(greaterThanEqual(v_world_pos, bmin - eps))
               && all(lessThanEqual(v_world_pos, bmax + eps));
    if (!inside) return R;
    vec3 invR = 1.0 / R;
    vec3 t1 = (bmax - v_world_pos) * invR;
    vec3 t2 = (bmin - v_world_pos) * invR;
    vec3 tmax = max(t1, t2);
    // A fragment just outside the box (eps tolerance) can produce a negative
    // exit distance; clamp so the hit never lands behind it.
    float dist = max(min(min(tmax.x, tmax.y), tmax.z), 0.0);
    return v_world_pos + R * dist - capture;
}

// Probe influence: 0 outside the box, ramping to 1 across the `blend` shell
// just inside its faces (the Reflection Probe's blend distance).
float probe_influence(vec3 p, vec3 center, vec3 extents, float blend) {
    vec3 d = extents - abs(p - center);
    float inner = min(d.x, min(d.y, d.z));
    if (blend <= 1e-4) return inner >= 0.0 ? 1.0 : 0.0;
    return clamp(inner / blend, 0.0, 1.0);
}

// Gradient noise with analytic derivatives: (value, d/dx, d/dy).
vec2 water_hash(vec2 p) {
    p = vec2(dot(p, vec2(127.1, 311.7)), dot(p, vec2(269.5, 183.3)));
    return fract(sin(p) * 43758.5453) * 2.0 - 1.0;
}

vec3 water_noised(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    vec2 u = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    vec2 du = 30.0 * f * f * (f * (f - 2.0) + 1.0);
    vec2 ga = water_hash(i);
    vec2 gb = water_hash(i + vec2(1.0, 0.0));
    vec2 gc = water_hash(i + vec2(0.0, 1.0));
    vec2 gd = water_hash(i + vec2(1.0, 1.0));
    float va = dot(ga, f);
    float vb = dot(gb, f - vec2(1.0, 0.0));
    float vc = dot(gc, f - vec2(0.0, 1.0));
    float vd = dot(gd, f - vec2(1.0, 1.0));
    float k = va - vb - vc + vd;
    float v = va + u.x * (vb - va) + u.y * (vc - va) + u.x * u.y * k;
    vec2 d = ga + u.x * (gb - ga) + u.y * (gc - ga) + u.x * u.y * (ga - gb - gc + gd)
           + du * (u.yx * k + vec2(vb, vc) - va);
    return vec3(v, d);
}

// Two drifting ripple layers, slopes only.
vec2 water_ripple_slope(vec2 p, float t, float scale) {
    vec2 p1 = p * scale * 0.9 + vec2(t * 0.05, t * 0.031);
    vec2 p2 = p * scale * 2.3 + vec2(-t * 0.043, t * 0.067);
    return water_noised(p1).yz * 0.6 + water_noised(p2).yz * 0.4;
}

// Ripples tilting the wave normal, carried along the world `flow` (m/s) by
// two half-period-offset phases that cross-fade (a flow map), faded out
// where a pixel covers more than a ripple (no sparkle in the distance).
vec3 water_ripples(vec3 N, vec2 p, float t, float strength, float scale, vec2 flow) {
    const float PERIOD = 2.0;
    float ph0 = fract(t / PERIOD);
    float ph1 = fract(t / PERIOD + 0.5);
    float w0 = 1.0 - abs(1.0 - 2.0 * ph0);
    vec2 g0 = water_ripple_slope(p - flow * (ph0 * PERIOD), t, scale);
    vec2 g1 = water_ripple_slope(p - flow * (ph1 * PERIOD) + vec2(0.37, 0.71), t, scale);
    vec2 g = mix(g1, g0, w0);
    float fade = 1.0 - smoothstep(0.35, 1.2, length(fwidth(p * scale * 2.3)));
    return normalize(N - vec3(g.x, 0.0, g.y) * (strength * 0.25 * fade));
}

// Foam cells: the edges of a jittered cell grid (second-nearest minus nearest
// feature distance), so foam reads as a lacy network of bubbles rather than
// smooth noise. 1 on the bubble walls.
float water_bubbles(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    float f1 = 8.0;
    float f2 = 8.0;
    for (int y = -1; y <= 1; ++y) {
        for (int x = -1; x <= 1; ++x) {
            vec2 g = vec2(x, y);
            vec2 o = water_hash(i + g) * 0.5 + 0.5;
            float d = length(g + o - f);
            if (d < f1) {
                f2 = f1;
                f1 = d;
            } else if (d < f2) {
                f2 = d;
            }
        }
    }
    return 1.0 - smoothstep(0.0, 0.18, f2 - f1);
}

// Foam texture: two bubble scales carried along `flow` (m/s) by a flow map's
// two cross-fading phases; `density` fills the cells in so thick foam reads
// solid and thin foam stays lacy.
float water_foam_pattern(vec2 p, float t, vec2 flow, float density) {
    const float PERIOD = 3.0;
    float ph0 = fract(t / PERIOD);
    float ph1 = fract(t / PERIOD + 0.5);
    float w0 = 1.0 - abs(1.0 - 2.0 * ph0);
    vec2 a = p - flow * (ph0 * PERIOD);
    vec2 b = p - flow * (ph1 * PERIOD) + vec2(3.7, 1.3);
    float f0 = water_bubbles(a * 3.0) * 0.6 + water_bubbles(a * 7.0 + 1.7) * 0.4;
    float f1 = water_bubbles(b * 3.0) * 0.6 + water_bubbles(b * 7.0 + 1.7) * 0.4;
    return clamp(mix(mix(f1, f0, w0), 1.0, density * density), 0.0, 1.0);
}

// View-space distance to the opaque scene behind framebuffer pixel `frag`.
float water_scene_dist(vec2 frag) {
    ivec2 size = textureSize(u_opaque_depth, 0);
    float d = texelFetch(u_opaque_depth, clamp(ivec2(frag), ivec2(0), size - 1), 0).r;
    if (d >= 1.0) {
        return 1e6;
    }
    vec2 suv = frag / vec2(max(frame.postfx2.w, 1.0), max(frame.postfx3.w, 1.0));
    vec4 p = frame.inv_proj * vec4(suv.x * 2.0 - 1.0, 1.0 - suv.y * 2.0, d, 1.0);
    return abs(p.w) > 1e-8 ? length(p.xyz / p.w) : 1e6;
}

// Slope (local XZ gradient) and, when `want_hess`, Hessian (xx, zz, xz) of
// body `s`'s surface over its local point `xz`, band-limited to `foot`
// metres: the Gerstner sum, the simulated field and the fine scrolling
// ripples (world `wxz`), the same surface the water shader draws.
void water_surface_slope(int s, vec2 xz, vec2 wxz, float foot, bool want_hess, out vec2 g, out vec3 h) {
    g = vec2(0.0);
    h = vec3(0.0);
    float t = frame.water.x;
    int n = min(int(frame.water_bodies[s].look1.z + 0.5), MAX_WATER_WAVES);
    for (int i = 0; i < n; ++i) {
        vec4 a = frame.water_bodies[s].wave_a[i];
        float amp = frame.water_bodies[s].wave_b[i].y * water_band(a.z, foot);
        float phase = a.z * dot(a.xy, xz) - a.w * t;
        g += (amp * a.z * cos(phase)) * a.xy;
        h -= (amp * a.z * a.z * sin(phase)) * vec3(a.x * a.x, a.y * a.y, a.x * a.y);
    }
    vec4 r = frame.water_bodies[s].ripple;
    vec2 uv = water_ripple_uv(s, xz);
    float fade = water_ripple_fade(uv);
    if (fade > 0.0) {
        float size = float(textureSize(u_water_ripples[s], 0).x);
        float lod = water_ripple_lod(foot, r.z, size);
        g += textureLod(u_water_ripples[s], uv, lod).gb * fade;
        if (want_hess) {
            // Central differences of the gradient a texel of that mip apart.
            float e = exp2(floor(lod)) / size;
            vec2 px = textureLod(u_water_ripples[s], uv + vec2(e, 0.0), lod).gb;
            vec2 nx = textureLod(u_water_ripples[s], uv - vec2(e, 0.0), lod).gb;
            vec2 pz = textureLod(u_water_ripples[s], uv + vec2(0.0, e), lod).gb;
            vec2 nz = textureLod(u_water_ripples[s], uv - vec2(0.0, e), lod).gb;
            h += fade * vec3(px.x - nx.x, pz.y - nz.y, 0.5 * (px.y - nx.y + pz.x - nz.x)) / (2.0 * e * r.z);
        }
    }
    vec4 look0 = frame.water_bodies[s].look0;
    float scale = max(look0.z, 0.01);
    float k = look0.y * 0.25 * (1.0 - smoothstep(0.35, 1.2, foot * scale * 2.3));
    if (k > 0.0) {
        vec2 g0 = water_ripple_slope(wxz, t, scale);
        g += g0 * k;
        if (want_hess) {
            float e = 0.05 / scale;
            vec2 gx = water_ripple_slope(wxz + vec2(e, 0.0), t, scale);
            vec2 gz = water_ripple_slope(wxz + vec2(0.0, e), t, scale);
            h += (k / e) * vec3(gx.x - g0.x, gz.y - g0.y, 0.5 * (gx.y - g0.y + gz.x - g0.x));
        }
    }
}

// Caustic focusing through body `s`'s surface: light crossing it at local
// `entry` bends by the slope there and lands `kappa` x slope further on, so a
// receiver gets 1/det(I + kappa H) of it, read at the surface point whose
// light lands here. Averaged over the receiver that keeps its brightness.
float water_focus(int s, vec2 entry, vec2 to_world, float kappa, float foot) {
    vec2 g;
    vec3 h;
    water_surface_slope(s, entry, entry + to_world, foot, false, g, h);
    vec2 src = entry - kappa * g;
    water_surface_slope(s, src, src + to_world, foot, true, g, h);
    float det = (1.0 + kappa * h.x) * (1.0 + kappa * h.y) - kappa * kappa * h.z * h.z;
    return min(inversesqrt(det * det + 0.01), 8.0);
}

// Light gain from caustics at world `p` for one light (1 = none). `travel`
// is a directional light's direction of travel; a point/spot light is at
// `light_pos` (`point`).
//   Under a surface, lit from above: light refracted into the water focuses
// where the surface curves, bent by (1 - 1/1.33) of the slope over the
// slanted depth. Caustic strength (bounds.w) scales the focusing.
//   Above a pool's surface (extra.x > 0), lit from a light under it: the
// light leaves the water bent the other way, by (1.33 - 1) of the slope over
// its path through the air, and throws the moving pattern on walls and
// ceiling; extra.x scales that light (1 physical), and the Fresnel loss at
// the surface takes the grazing rays.
float water_caustic_gain(vec3 p, vec3 travel, vec3 light_pos, bool point) {
    int n = min(int(frame.water.z + 0.5), MAX_WATER_BODIES);
    float pix = length(fwidth(p));
    for (int s = 0; s < n; ++s) {
        vec4 b = frame.water_bodies[s].bounds;
        mat4 to_local = frame.water_bodies[s].to_local;
        vec3 l = (to_local * vec4(p, 1.0)).xyz;
        vec3 lp = (to_local * vec4(light_pos, 1.0)).xyz;
        vec2 to_world = p.xz - l.xz;
        float depth = b.z - l.y;
        bool inside = b.x <= 0.0 || (abs(l.x) <= b.x && abs(l.z) <= b.y);
        if (depth > 0.005 && inside) {
            if (b.w <= 0.0 || (point && lp.y < b.z)) {
                return 1.0;
            }
            vec3 dir = point ? normalize(l - lp) : normalize(mat3(to_local) * travel);
            vec3 tr = refract(dir, vec3(0.0, 1.0, 0.0), 1.0 / 1.33);
            if (tr.y > -0.05) {
                tr = vec3(0.0, -1.0, 0.0);
            }
            float slant = depth / -tr.y;
            float kappa = b.w * slant * (1.0 - 1.0 / 1.33);
            vec2 entry = l.xz - tr.xz * slant;
            // Deeper floors see the pattern softer (lights have a size).
            float foot = max(pix, 0.02 + 0.03 * depth);
            return mix(1.0, water_focus(s, entry, to_world, kappa, foot), exp(-depth * 0.18));
        }
        float strength = frame.water_bodies[s].extra.x;
        float under = b.z - lp.y;
        if (!point || strength <= 0.0 || depth > 0.0 || under <= 0.005) {
            continue;
        }
        if (b.x > 0.0 && (abs(lp.x) > b.x || abs(lp.z) > b.y)) {
            continue;
        }
        // Where the bent ray leaves the water: 1.33 sin(water angle) = sin(air
        // angle) along the line from the light to here.
        float rise = -depth;
        vec2 across = l.xz - lp.xz;
        float reach = length(across);
        float lo = 0.0;
        float hi = reach;
        for (int k = 0; k < 10; ++k) {
            float r = 0.5 * (lo + hi);
            float sw = r / sqrt(r * r + under * under);
            float sa = (reach - r) / sqrt((reach - r) * (reach - r) + rise * rise);
            if (1.33 * sw > sa) {
                hi = r;
            } else {
                lo = r;
            }
        }
        float r = 0.5 * (lo + hi);
        vec2 entry = lp.xz + (reach > 1e-5 ? across / reach : vec2(0.0)) * r;
        if (b.x > 0.0 && (abs(entry.x) > b.x || abs(entry.y) > b.y)) {
            return 0.0;
        }
        float air = sqrt((reach - r) * (reach - r) + rise * rise);
        float cos_air = rise / max(air, 1e-4);
        float fresnel = 0.02 + 0.98 * pow(1.0 - clamp(cos_air, 0.0, 1.0), 5.0);
        // A point source's spreading beam dilutes the focusing far away.
        float kappa = 0.33 * air * (1.33 * under) / (1.33 * under + air);
        float foot = max(pix * 0.5, 0.01 + 0.01 * air);
        return strength * (1.0 - fresnel) * water_focus(s, entry, to_world, kappa, foot);
    }
    return 1.0;
}

void main() {
    // Default the SSR G-buffer to "not a reflector"; the reflection block below
    // overwrites it for lit surfaces. Keeps early debug returns valid.
    o_gbuf = vec4(0.0);
    // Lightmap-UV checker preview: visualise per-object texel density. params1.w
    // carries this object's lightmap resolution (texels/side) in preview mode; a
    // fixed 8-texel cell means low-res objects show big squares, high-res small.
    if (frame.debug.x > 0.5) {
        float res = v_lightmap;
        if (res < 1.0) {
            o_color = vec4(0.12, 0.12, 0.14, 1.0); // not lightmapped (non-static)
        } else {
            vec2 c = floor(v_uv1 * res / 8.0);
            float check = mod(c.x + c.y, 2.0);
            o_color = vec4(mix(vec3(0.12, 0.12, 0.15), vec3(0.95, 0.55, 0.2), check), 1.0);
        }
        return;
    }

    vec3 N = normalize(v_normal);
    if (!gl_FrontFacing) {
        N = -N;
    }

    // Parallax occlusion mapping displaces the mesh UV along the tangent-space
    // view ray through the height field, so every map below samples the same
    // apparent surface point. Off (scale 0) leaves the UV untouched. The shift
    // is computed in albedo-tiled space then mapped back to mesh UV so the
    // per-map tiling/offset below stays coherent.
    vec2 v_uv_d = v_uv;
    if (fx.parallax.x > 0.0) {
        vec3 Tg = normalize(v_tangent.xyz - N * dot(v_tangent.xyz, N));
        vec3 Bg = cross(N, Tg) * v_tangent.w;
        vec3 to_eye = CAMERA_POS.xyz - v_world_pos;
        vec3 vt = normalize(vec3(dot(to_eye, Tg), dot(to_eye, Bg), dot(to_eye, N)));
        vec2 a0 = v_uv * fx.albedo_st.xy + fx.albedo_st.zw;
        vec2 a1 = parallax_uv(a0, vt, fx.parallax.x);
        v_uv_d = v_uv + (a1 - a0) / max(fx.albedo_st.xy, vec2(1e-4));
    }

    // Per-texture UV transform (tiling*uv + offset) plus the animated scroll, on
    // the parallax-displaced mesh UV. Albedo's UV is the shared `uv` used by the
    // opacity + mask samplers below; normal/orm/emission get their own tiling.
    vec2 uv = v_uv_d * fx.albedo_st.xy + fx.albedo_st.zw + fx.scroll.xy * frame.misc.x;
    vec2 uv_normal = v_uv_d * fx.normal_st.xy + fx.normal_st.zw;
    vec2 uv_orm = v_uv_d * fx.orm_st.xy + fx.orm_st.zw;
    vec2 uv_emission = v_uv_d * fx.emission_st.xy + fx.emission_st.zw;

    vec4 albedo = texture(t_albedo, uv) * pc.base_color * v_color;
    albedo.a *= texture(t_opacity, uv).r; // dedicated opacity map (default white)
    if (ALPHA_MODE == 1u && albedo.a < pc.params1.x) {
        discard;
    }

    if (FEAT_NORMAL_MAP) {
        vec3 T = normalize(v_tangent.xyz - N * dot(v_tangent.xyz, N));
        vec3 B = cross(N, T) * v_tangent.w;
        vec3 tn = texture(t_normal, uv_normal).xyz * 2.0 - 1.0;
        tn.xy *= pc.params1.y;
        N = normalize(mat3(T, B, N) * normalize(tn));
    }

    vec3 orm = texture(t_orm, uv_orm).rgb;
    // Split AO/Roughness/Metallic maps multiply their packed-ORM channel (white
    // default = no-op), each with an optional invert (e.g. smoothness->roughness).
    float ao_s = texture(t_ao, uv_orm).r;
    float rough_s = texture(t_roughness, uv_orm).r;
    float metal_s = texture(t_metallic, uv_orm).r;
    ao_s = mix(ao_s, 1.0 - ao_s, fx.orm_invert.x);
    rough_s = mix(rough_s, 1.0 - rough_s, fx.orm_invert.y);
    metal_s = mix(metal_s, 1.0 - metal_s, fx.orm_invert.z);
    float ao = mix(1.0, orm.r * ao_s, pc.params1.z);
    // Toon rim strength now comes from the FX block (so occlusion stays usable).
    float rim_strength = FEAT_TOON ? fx.toon.x : 0.0;
    float roughness = clamp(orm.g * rough_s * pc.params0.y, 0.045, 1.0);
    float metallic = clamp(orm.b * metal_s * pc.params0.x, 0.0, 1.0);

    // Water: wave normal plus ripples, a body coloured by depth absorption,
    // the opaque scene refracted through it, and shore + crest foam.
    vec3 water_behind = vec3(0.0);
    float water_see = 0.0;
    float water_alpha = 1.0;
    float water_foam = 0.0;
    if (FEAT_WATER) {
        int ws = clamp(int(fx.water.x + 0.5), 0, MAX_WATER_BODIES - 1);
        vec4 shallow = frame.water_bodies[ws].shallow;
        vec4 deep = frame.water_bodies[ws].deep;
        vec4 look0 = frame.water_bodies[ws].look0;
        vec4 look1 = frame.water_bodies[ws].look1;
        float wt = frame.water.x;
        // Directions from the water's local space to world (normals by the
        // inverse transpose of local-to-world), and its rest "up".
        mat3 to_world_n = transpose(mat3(frame.water_bodies[ws].to_local));
        vec3 up = normalize(to_world_n * vec3(0.0, 1.0, 0.0));
        // The full surface per pixel at its rest point (v_uv): the mesh only
        // carries what its spacing can hold. Everything is band-limited to
        // the pixel's footprint, and what is too fine to show roughens the
        // highlight instead of sparkling.
        vec2 rest = v_uv;
        float foot = max(max(length(dFdx(rest)), length(dFdy(rest))), 1e-4);
        float calm = 1.0 - v_color.b * v_color.b;
        // The waves (not the simulated layer) also calm over the shallows
        // near a shore (the vertex shader's water_shore).
        float wcalm = calm * clamp(v_color.a, 0.0, 1.0);
        vec3 wd;
        vec3 wn;
        float crest_n;
        float lost;
        water_waves(ws, rest, wt, foot, wd, wn, crest_n, lost);
        wn = normalize(mix(vec3(0.0, 1.0, 0.0), wn, wcalm));
        // The simulated layer at the footprint's mip (r height, gb slope).
        vec2 ruv = water_ripple_uv(ws, rest);
        float rfade = water_ripple_fade(ruv) * calm;
        vec4 rip = texture(t_displacement, ruv) * rfade;
        vec3 nl = normalize(vec3(wn.x / max(wn.y, 0.05) - rip.g, 1.0, wn.z / max(wn.y, 0.05) - rip.b));
        N = normalize(to_world_n * nl);
        if (!gl_FrontFacing) {
            N = -N;
        }
        // World-space flow (m/s), decoded by the vertex shader.
        vec2 flow = v_color.rg;
        N = water_ripples(N, v_world_pos.xz, wt, look0.y, max(look0.z, 0.01), flow);
        float surface_h = wd.y * wcalm + rip.r;
        roughness = clamp(look0.x, 0.02, 1.0);
        metallic = 0.0;
        ao = 1.0;
        vec3 Vw = normalize(CAMERA_POS.xyz - v_world_pos);
        float clarity = clamp(deep.w, 0.0, 1.0);
        float shore = 0.0;
        float contact_depth = 1e4;
        float transmit = 0.35;
        if (frame.water.y > 0.5) {
            vec3 cam = CAMERA_POS.xyz;
            vec3 I = -Vw;
            vec2 screen = vec2(max(frame.postfx2.w, 1.0), max(frame.postfx3.w, 1.0));
            float surf_dist = length((VIEW_MATRIX * vec4(v_world_pos, 1.0)).xyz);
            float flat_path = max(water_scene_dist(gl_FragCoord.xy) - surf_dist, 0.0);
            // Behind the surface: down from above, up from below.
            vec3 into = gl_FrontFacing ? -up : up;
            float thick = flat_path * max(dot(I, into), 0.0);
            // Refraction: the view ray bent by Snell's law through the
            // surface normal (look.refraction scales the bend, 1 physical),
            // followed to the bottom: the thickness here first, then the
            // depth re-read where the bent ray lands. A lookup that lands on
            // something out of the water, off screen, or a ray reflected
            // whole falls back to the straight view.
            vec2 rp = gl_FragCoord.xy;
            float path = flat_path;
            vec3 T = refract(I, N, gl_FrontFacing ? 1.0 / 1.33 : 1.33);
            float bend = max(look1.y, 0.0);
            if (bend > 0.0 && dot(T, T) > 0.5 && thick > 1e-4) {
                T = normalize(mix(I, T, bend));
                float down = max(dot(T, into), 0.05);
                float len = thick / down;
                vec2 hit = rp;
                float keep = 0.0;
                for (int k = 0; k < 2; ++k) {
                    vec4 c = VIEW_PROJ * vec4(v_world_pos + T * len, 1.0);
                    if (c.w <= 1e-4) {
                        keep = 0.0;
                        break;
                    }
                    vec2 suv = vec2(c.x / c.w * 0.5 + 0.5, 0.5 - c.y / c.w * 0.5);
                    vec2 edge = min(suv, 1.0 - suv);
                    keep = smoothstep(0.0, 0.04, min(edge.x, edge.y));
                    if (keep <= 0.0) {
                        break;
                    }
                    hit = suv * screen;
                    float dq = water_scene_dist(hit);
                    vec3 q = v_world_pos + T * len;
                    vec3 behind_p = cam + normalize(q - cam) * min(dq, 1e4);
                    float below = dot(behind_p - v_world_pos, into);
                    if (below <= 0.0) {
                        keep = 0.0;
                        break;
                    }
                    len = below / down;
                }
                rp = mix(rp, hit, keep);
                path = mix(flat_path, len, keep);
            }
            path = min(path, 1e4);
            transmit = exp(-path * 3.0 / max(shallow.w, 0.01));
            vec3 behind = texture(u_opaque_color, rp / vec2(textureSize(u_opaque_color, 0))).rgb;
            water_behind = behind * mix(shallow.rgb, vec3(1.0), transmit);
            water_see = transmit * clarity;
            // Soft where the surface meets something: over a few pixels or
            // the mesh's spacing, whichever is wider, so the waterline on a
            // body never shows the triangles it cuts.
            float contact = clamp(max(2.0 * foot, 0.1 * v_uv1.y), 0.03, 0.3);
            water_alpha = smoothstep(0.0, contact, thick);
            shore = 1.0 - smoothstep(0.0, max(look0.w, 1e-3), thick);
            contact_depth = thick;
        } else {
            water_alpha = 1.0 - clarity * 0.4;
        }
        float height = clamp(surface_h / max(look1.w, 1e-4), -1.0, 1.0);
        float crest = smoothstep(0.35, 0.95, crest_n * wcalm) * smoothstep(0.1, 0.7, height) * look1.x;
        // Contact foam in two layers: dense against whatever the water
        // touches, patchy lace further out, the band lapping in and out.
        float lap = 0.85 + 0.15 * sin(wt * 1.3 + dot(v_world_pos.xz, vec2(0.7, 0.4)));
        float fw = max(look0.w, 1e-3) * lap;
        float dense = 1.0 - smoothstep(0.0, fw * 0.35, contact_depth);
        float lace = 1.0 - smoothstep(fw * 0.35, fw, contact_depth);
        // Whitewater where the simulated water runs fast for its depth
        // (Froude number u / sqrt(g h) past ~0.8: plunge pools, hard wakes),
        // not ordinary sloshing in shallow water.
        float wave_speed = sqrt(9.81 * clamp(contact_depth, 0.1, 5.0));
        float white = smoothstep(0.8, 1.6, rip.a / wave_speed);
        // Churned water (waterfall plunges, whirlpools): thick in the middle,
        // breaking into lace toward the edge, carried outward.
        float churn = 0.0;
        vec2 churn_flow = vec2(0.0);
        for (int k = 0; k < MAX_WATER_FOAM; ++k) {
            vec4 spot = frame.water_bodies[ws].foam[k];
            if (spot.z <= 0.0) {
                continue;
            }
            vec2 d = rest - spot.xy;
            float r = length(d) / spot.z;
            float c = spot.w * (1.0 - smoothstep(0.25, 1.0, r));
            if (c > churn) {
                churn = c;
                vec2 out_dir = r > 1e-3 ? d / (r * spot.z) : vec2(0.0);
                churn_flow = (transpose(mat3(frame.water_bodies[ws].to_local)) * vec3(out_dir.x, 0.0, out_dir.y)).xz * (0.5 + 0.5 * r);
            }
        }
        churn = clamp(churn, 0.0, 1.0);
        flow += churn_flow * churn;
        float density = max(max(dense, churn * 0.9), max(white * 0.8, crest * 0.6));
        float cover = max(max(max(dense, lace * 0.7), max(crest, white)), churn);
        float bubbles = water_foam_pattern(v_world_pos.xz, wt, flow, density);
        water_foam = clamp(cover * smoothstep(0.25, 0.75, bubbles * (0.6 + 0.4 * cover) + cover * 0.25), 0.0, 1.0);
        // The body's Foam amount (ripple.w): 0 turns it all off.
        water_foam *= clamp(frame.water_bodies[ws].ripple.w, 0.0, 2.0);
        water_foam = min(water_foam, 1.0);
        albedo = vec4(mix(mix(deep.rgb, shallow.rgb, transmit), vec3(0.92), water_foam), 1.0);
        water_see *= 1.0 - water_foam;
        roughness = mix(roughness, 0.6, water_foam);
        // Specular anti-aliasing: the normal's spread over the pixel and the
        // slope of the waves too fine to draw widen the lobe (GGX alpha^2 +
        // 2 sigma^2), so small bright highlights don't flicker into specks.
        vec3 dndx = dFdx(N);
        vec3 dndy = dFdy(N);
        float spread = 0.25 * (dot(dndx, dndx) + dot(dndy, dndy));
        float a = roughness * roughness;
        float a2 = clamp(a * a + min(2.0 * spread, 0.18) + 2.0 * lost * wcalm, 0.0, 1.0);
        roughness = clamp(sqrt(sqrt(a2)), 0.02, 1.0);
    }

    vec3 V = normalize(CAMERA_POS.xyz - v_world_pos);
    float NdotV = max(dot(N, V), 1e-4);
    vec3 f0 = mix(vec3(0.04), albedo.rgb, metallic);
    vec3 diffuse_color = albedo.rgb * (1.0 - metallic);
    if (FEAT_WATER) {
        // The see-through share comes from the refraction instead.
        f0 = vec3(0.02);
        diffuse_color *= 1.0 - water_see;
    }

    // Accumulate every active scene light. The CPU guarantees at least one
    // (a directional fallback when the scene has no light objects).
    int light_count = int(frame.misc.y + 0.5);
    // Surfaces under water catch caustic light (seen from above too), and
    // lights under a pool throw it out onto what's above.
    bool caustics = !FEAT_WATER && frame.water.z > 0.5;
    vec3 color = vec3(0.0);
    bool has_lightmap = v_lightmap >= 0.0;
    uvec4 light_mask = floatBitsToUint(pc.model[0]);
    vec3 voxel_emit = vec3(0.0); // FluxVoxel moving emitters (indirect, kind 3)
    for (int w = 0; w < 4; ++w) {
        uint bits = light_mask[w];
        while (bits != 0u) {
            int i = w * 32 + findLSB(bits);
            bits &= bits - 1u;
            if (i >= light_count || i >= MAX_LIGHTS) {
                bits = 0u;
                continue;
            }
            Light light = frame.lights[i];
            int kind = int(light.pos_kind.w + 0.5);
            if (kind == 3) {
                // The voxel grid's injection kernel per pixel: a probe grid
                // shows its cells under an orb's sharp falloff.
                vec3 d = light.pos_kind.xyz - v_world_pos;
                float dist = length(d);
                float f = clamp(1.0 - dist / max(light.dir_range.w, 1e-3), 0.0, 1.0);
                vec3 Le = dist > 1e-4 ? d / dist : N;
                voxel_emit += light.color.rgb * (f * f * max(0.11141 + 0.25465 * dot(N, Le), 0.0));
                continue;
            }
            // Baked/Mixed light (flagged spot.y<0 & spot.w>0): its energy is already
            // in the lightmap, so skip it for lightmapped (static) surfaces, but DO
            // apply it to non-lightmapped objects (dynamic / added-after-bake) so they
            // aren't black in a baked scene. Realtime lights are never skipped.
            bool light_baked = light.spot.y < 0.0 && light.spot.w > 0.5;
            bool baked_here = light_baked && has_lightmap;
            if (baked_here && !caustics) {
                continue;
            }

            // Direction to the light + distance-based attenuation.
            vec3 L;
            float attenuation = 1.0;
            // Lights are small spheres, not points: their angular size widens
            // the specular lobe (Karis 2013 sphere-light roughness), which keeps
            // a mirror-smooth highlight finite instead of overflowing half floats.
            float a_spec = roughness * roughness;
            float a_light = a_spec + 0.00465;
            if (kind == 0) {
                L = normalize(-light.dir_range.xyz);
            } else {
                vec3 to_light = light.pos_kind.xyz - v_world_pos;
                float dist = length(to_light);
                a_light = min(a_spec + LIGHT_SOURCE_RADIUS / (2.0 * max(dist, 1e-3)), 1.0);
                L = (dist > 1e-5) ? to_light / dist : N;
                // Smooth inverse-square-ish falloff clamped to the range.
                float range = max(light.dir_range.w, 1e-3);
                float t = clamp(1.0 - dist / range, 0.0, 1.0);
                attenuation = (t * t) / (1.0 + dist * dist);
                if (kind == 2) {
                    // Spot cone: full inside the inner angle, smooth to the outer.
                    float cos_dir = dot(normalize(light.dir_range.xyz), -L);
                    float cos_outer = light.color.w;
                    float cos_inner = light.spot.x;
                    attenuation *= smoothstep(cos_outer, cos_inner, cos_dir);
                }
            }

            vec3 radiance = light.color.rgb * attenuation;
            if (radiance == vec3(0.0)) {
                continue;
            }
            float caustic_gain = caustics ? water_caustic_gain(v_world_pos, light.dir_range.xyz, light.pos_kind.xyz, kind != 0) : 1.0;
            // A baked light is already in the lightmap: only the caustics'
            // change to it is added here.
            if (baked_here) {
                if (abs(caustic_gain - 1.0) < 1e-3) {
                    continue;
                }
                caustic_gain -= 1.0;
            }

            // Facing away: every term below is scaled by N·L (the toon ramp too),
            // so skip the BRDF and the shadow-map lookups.
            float NdotL = dot(N, L);
            if (NdotL <= 0.0) {
                continue;
            }
            vec3 H = normalize(V + L);
            float NdotH = max(dot(N, H), 0.0);
            float VdotH = max(dot(V, H), 0.0);

            // Cook-Torrance specular + energy-conserving Lambertian diffuse: the
            // Fresnel term F is the specular reflectance kS, so the diffuse keeps
            // only the remaining energy kD = (1 - F). (Metallic is already folded
            // into diffuse_color, which is 0 for pure metals.)
            vec3 F = f_schlick(VdotH, f0);
            float lobe = a_spec / a_light;
            vec3 spec = d_ggx(NdotH, a_light) * (lobe * lobe)
                * g_smith(NdotV, NdotL, roughness)
                * F
                / max(4.0 * NdotV * NdotL, 1e-4);
            vec3 kd = vec3(1.0) - F;
            vec3 lit = (kd * diffuse_color / PI + spec) * NdotL;

            if (FEAT_TOON) {
                // Cel ramp: quantize N·L into `steps` bands with a soft edge at each
                // boundary (a clean toon terminator without hard aliasing). `steps`
                // controls band count, params0.w blends toon<->smooth-PBR.
                float steps = max(pc.params0.z, 2.0);
                float scaled = clamp(NdotL, 0.0, 1.0) * steps;
                float lower = floor(scaled);
                float sm = clamp(fx.toon.y, 0.01, 0.49);
                float edge = smoothstep(0.5 - sm, 0.5 + sm, scaled - lower);
                float banded = (lower + edge) / steps;
                // Specular as a crisp toon highlight, only on lit bands.
                vec3 lit_toon = (kd * diffuse_color / PI) * banded
                    + spec * smoothstep(0.0, 0.02, banded);
                lit = mix(lit, lit_toon, clamp(pc.params0.w, 0.0, 1.0));
            }

            color += lit * radiance * shadow_factor(light, v_world_pos, N, L) * caustic_gain;
        }
    }

    // Indirect diffuse (as irradiance/PI), in priority order:
    //   1. baked lightmap for static objects (params1.w = array layer, >= 0),
    //   2. baked probe SH where a volume covers this fragment,
    //   3. flat scene ambient.
    vec3 indirect;
    vec3 flux_gi = vec3(0.0); // realtime screen GI part (view mode 6)
    if (has_lightmap) {
        int layer = int(v_lightmap + 0.5);
        indirect = texture(u_lightmap, vec3(v_uv1, float(layer))).rgb / PI;
        // MIXED LIGHTING: a lightmapped (static-baked) surface ALSO receives the
        // realtime FluxVoxel probe GI, added on top of the baked bounce. The bake
        // can't contain dynamic/movable emitters, so without this an emissive orb
        // lights ONLY metals (via reflections) and never the baked floor/couch.
        // Use the SMOOTH (non-occluded) probe sample — the lightmap already carries
        // the static occlusion, and the DDGI Chebyshev's per-probe variation showed
        // as fixed dots on the bright baked surface. Returns 0 with no realtime GI.
        // debug.z < -0.5 = static bake capture: realtime GI (dynamic emitters'
        // bounce) must not bake into the reflection cube.
        if (frame.debug.z > -0.5) {
            indirect += sample_irradiance_smooth(v_world_pos, N);
        }
        // Mixed lighting (debug.z > 1.5): Flux gathers only what the bake can't
        // hold (moving emitters, realtime lights) and it adds on top.
        if (frame.debug.z > 1.5) {
            vec2 suv = gl_FragCoord.xy / vec2(frame.postfx2.w, frame.postfx3.w);
            vec3 Pv = (VIEW_MATRIX * vec4(v_world_pos, 1.0)).xyz;
            vec3 Nv = normalize(mat3(VIEW_MATRIX) * N);
            flux_gi = screen_gi_upsample(suv, Pv, Nv);
            indirect += flux_gi;
        }
    } else if (frame.debug.z > 0.5) {
        // Screen-space GI: depth-aware upsample of the sparse screen probes.
        // Emissive surfaces are in the depth prepass again, so they sample their
        // OWN GI here (their emission still dominates the look).
        vec2 suv = gl_FragCoord.xy / vec2(frame.postfx2.w, frame.postfx3.w);
        // View-space position + normal so the upsample can reject only true depth
        // edges, keeping coplanar neighbours on steep/grazing faces (no GI noise).
        vec3 Pv = (VIEW_MATRIX * vec4(v_world_pos, 1.0)).xyz;
        vec3 Nv = normalize(mat3(VIEW_MATRIX) * N);
        flux_gi = screen_gi_upsample(suv, Pv, Nv);
        indirect = flux_gi;
        // Mixed lighting: the gather is dynamic-only, so a non-lightmapped object
        // keeps the baked probes underneath it.
        if (frame.debug.z > 1.5) {
            vec3 occ_irr;
            float cov = sample_probes(v_world_pos, N, occ_irr);
            indirect += mix(frame.ambient.rgb, sample_irradiance_smooth(v_world_pos, N), cov);
        }
    } else {
        // Probe GI where a cascade covers the fragment, fading to flat ambient
        // at the outermost cascade's edge (coverage < 1) and beyond it (0).
        // Use the SAME smooth, full-strength voxel sample as the lightmap path (no
        // per-probe Chebyshev dots/dimming) so a non-baked floor lights up just like
        // a baked one. `sample_probes` is still called for its coverage value (the
        // cascade fade-to-ambient at the volume edge); FluxVoxel's analytic radiance
        // has no occlusion to lose, and the SDF/RT march already bakes occlusion into
        // the probe values, so dropping the Chebyshev here only removes interpolation
        // leak-guarding — a fair trade for the smooth, even result.
        if (frame.debug.z < -0.5) {
            // Static bake capture: no realtime probe GI, flat ambient only.
            indirect = frame.ambient.rgb;
        } else {
            vec3 occ_irr;
            float cov = sample_probes(v_world_pos, N, occ_irr);
            indirect = mix(frame.ambient.rgb, sample_irradiance_smooth(v_world_pos, N), cov);
        }
    }
    // Energy-conserving ambient: split indirect into diffuse (non-reflected) and
    // a cheap specular share so metals/smooth surfaces pick up environment colour
    // instead of reading flat. Uses the probe/lightmap irradiance as a stand-in
    // for environment radiance (no reflection probes yet). Approximate, but it
    // keeps metals lively. Smoother surfaces concentrate the spec response.
    indirect += voxel_emit;
    vec3 F_amb = f_schlick_rough(NdotV, f0, roughness);
    vec3 kd_amb = vec3(1.0) - F_amb;
    color += kd_amb * indirect * diffuse_color * ao;
    // Baked lights' caustics take light away in the dark bands.
    color = max(color, vec3(0.0));
    float spec_amb = mix(1.0, 0.25, roughness); // rough surfaces scatter it away
    // Specular environment radiance, in priority order:
    //   1. SSR hit (on-screen, smooth/metallic), then
    //   2. the prefiltered reflection-probe cube (roughness -> mip), then
    //   3. the indirect-irradiance stand-in (no skybox -> neutral black cube).
    vec3 R = reflect(-V, N);
    // Footprint mip: a smooth curved mirror (a chrome sphere) spans many cube
    // texels per pixel, and sampling mip 0 there aliases into sparkle (fine
    // floor detail, small bright lights). Measured on the raw reflection
    // vector, before box projection, whose derivatives jump at box edges.
    float env_aa_lod = textureQueryLod(u_env, R).x;
    // Box-projected parallax (Unity-style) when a reflection-probe zone covers
    // this fragment: reproject the reflection ray onto the probe's box so flat
    // surfaces line up with the captured environment instead of treating it as
    // infinitely distant. `refl_center.w` = probe intensity (0 = no zone).
    float probe_intensity = frame.refl_center.w;
    vec3 R_raw = R;
    if (probe_intensity > 0.0) {
        R = probe_dir(R, frame.refl_center.xyz, frame.refl_box.xyz, frame.refl_extents.xyz, frame.refl_extents.w > 0.5);
    }
    // No reflection-probe zone → the fallback is the skybox cube, scaled by the
    // skylight/env intensity (frame.ambient.w). So turning ambient/skylight off
    // stops metals mirroring the sky and the scene goes black with no lights;
    // default (intensity 1) is unchanged. A probe zone uses its own intensity.
    float env_scale = probe_intensity > 0.0 ? probe_intensity : frame.ambient.w;
    // The env cube ALWAYS carries the environment (skybox texture/cube, or the
    // procedural sky when none is set), so this is the reflection's base at any
    // angle — never the dark diffuse-GI stand-in. The fullscreen SSR resolve pass
    // (deferred, current-frame) overlays screen-space reflections on top of this.
    vec3 spec_env = textureLod(u_env, R, max(roughness * ENV_MAX_LOD, env_aa_lod)).rgb * env_scale;
    // Multi-probe blend: slot 0 is the fallback, additional slots blend in by
    // their box influence. This supports 3+ overlapping reflection probes.
    float pw0 = probe_influence(v_world_pos, frame.refl_box.xyz, frame.refl_extents.xyz, frame.refl_blend.x);
    float pw1 = probe_influence(v_world_pos, frame.refl2_box.xyz, frame.refl2_extents.xyz, frame.refl_blend.y);
    float pw2 = probe_influence(v_world_pos, frame.refl3_box.xyz, frame.refl3_extents.xyz, frame.refl_blend2.x);
    float pw3 = probe_influence(v_world_pos, frame.refl4_box.xyz, frame.refl4_extents.xyz, frame.refl_blend2.y);
    float wsum = pw0;
    vec3 mix_spec = vec3(0.0);
    if (probe_intensity > 0.0) {
        mix_spec += spec_env * pw0;
    }
    float probe2_intensity = frame.refl2_center.w;
    if (probe2_intensity > 0.0 && pw1 > 0.0) {
        vec3 R2 = probe_dir(R_raw, frame.refl2_center.xyz, frame.refl2_box.xyz, frame.refl2_extents.xyz, frame.refl2_extents.w > 0.5);
        float lod2 = max(roughness * float(textureQueryLevels(u_env_b) - 1), env_aa_lod);
        mix_spec += textureLod(u_env_b, R2, lod2).rgb * probe2_intensity * pw1;
        wsum += pw1;
    }
    float probe3_intensity = frame.refl3_center.w;
    if (probe3_intensity > 0.0 && pw2 > 0.0) {
        vec3 R3 = probe_dir(R_raw, frame.refl3_center.xyz, frame.refl3_box.xyz, frame.refl3_extents.xyz, frame.refl3_extents.w > 0.5);
        float lod3 = max(roughness * float(textureQueryLevels(u_env_c) - 1), env_aa_lod);
        mix_spec += textureLod(u_env_c, R3, lod3).rgb * probe3_intensity * pw2;
        wsum += pw2;
    }
    float probe4_intensity = frame.refl4_center.w;
    if (probe4_intensity > 0.0 && pw3 > 0.0) {
        vec3 R4 = probe_dir(R_raw, frame.refl4_center.xyz, frame.refl4_box.xyz, frame.refl4_extents.xyz, frame.refl4_extents.w > 0.5);
        float lod4 = max(roughness * float(textureQueryLevels(u_env_d) - 1), env_aa_lod);
        mix_spec += textureLod(u_env_d, R4, lod4).rgb * probe4_intensity * pw3;
        wsum += pw3;
    }
    if (wsum > 1e-4) {
        spec_env = mix_spec / wsum;
    }
    if (probe_intensity < 0.0) {
        // Reflection-cube capture: no reflections of reflections (no probe
        // feedback); diffuse GI stands in for the environment.
        spec_env = indirect;
    }
    // Planar reflection: this surface has its own mirrored scene render
    // (pc.emission.w = layer+1 in the integer part, roughness fade cutoff in
    // the fraction). Rendered with the same camera/viewport convention as this
    // pass, so the reflection for THIS pixel sits at the same screen UV — a
    // true mirror with per-pixel parallax, replacing the probe/env on smooth
    // surfaces and fading back to it as roughness rises. frame.refl_box.w = 0
    // in the planar/capture/preview passes themselves (no recursion).
    bool planar_mirror = pc.emission.w >= 1.0 && frame.refl_box.w > 0.5;
    if (planar_mirror) {
        float planar_layer = floor(pc.emission.w) - 1.0;
        float cutoff = max(fract(pc.emission.w), 0.05);
        vec2 suv = gl_FragCoord.xy / vec2(frame.postfx2.w, frame.postfx3.w);
        vec3 planar_col = texture(u_planar, vec3(suv, planar_layer)).rgb;
        float w = 1.0 - smoothstep(cutoff * 0.5, cutoff, roughness);
        spec_env = mix(spec_env, planar_col, w);
    }
    // Reflectance weight that multiplies environment radiance. The resolve pass
    // re-derives env radiance from this same cube and swaps in the SSR hit:
    //   final += reflectance * conf * (ssr_radiance - env_radiance)
    // so SSR exactly replaces the env reflection where it has a confident hit and
    // leaves the env reflection untouched (this colour, fogged below) elsewhere.
    // Split-sum environment BRDF instead of a raw Fresnel*spec term: bounded,
    // roughness-correct specular that doesn't blow out to a harsh mirror band at
    // grazing angles on rough/dielectric surfaces.
    vec2 env_ab = env_brdf_approx(roughness, NdotV);
    // Reflection strength (fx.parallax.y, default 1) scales the env reflection
    // here AND the gbuffer reflectivity below so the deferred SSR/RT resolve
    // tracks it. Reflection *technique* (Probes / SSR / Ray-traced) is a GLOBAL
    // scene setting (Environment → Reflections), NOT a per-material toggle — UE
    // does the same (reflection method is a post-process/project setting, not a
    // material checkbox), so the old per-material SSR/RT gate was removed.
    float refl_strength = fx.parallax.y;
    vec3 reflectance = (f0 * env_ab.x + vec3(env_ab.y)) * ao * refl_strength;
    if (FEAT_WATER) {
        reflectance = f_schlick(NdotV, vec3(0.02)) * refl_strength * (1.0 - 0.8 * water_foam);
    }
    color += reflectance * spec_env;
    // FluxVoxel specular-from-volume (frame.fog_params.w = voxel_specular flag):
    // sample the volume in the reflection direction so SMOOTH METALS pick up the
    // local emissive/voxel bounce the (coarse) reflection cube misses. This term is
    // VIEW-DEPENDENT (R = reflect(-V, N)), so it must NOT drive a rough/dielectric
    // floor — there the emissive arrives as VIEW-INDEPENDENT diffuse GI (above), so
    // the floor glow looks the same from every angle instead of riding the camera.
    // Gate by metallic·(1-roughness): ~0 for dielectric OR rough, only smooth metals
    // get it. (Earlier `reflectance·vrad·roughness` lit rough floors at grazing
    // Fresnel → the "GI only on the camera side" artifact.)
    // It is a FALLBACK reflection source, not an extra layer: where a probe
    // zone or a planar mirror already reflects the scene, adding it showed
    // every emissive twice (sharp from the probe, a smeared voxel streak).
    if (frame.fog_params.w > 0.5 && probe_intensity == 0.0 && !planar_mirror) {
        float spec_w = metallic * (1.0 - roughness);
        if (spec_w > 0.001) {
            vec3 vrad;
            if (volume_radiance(v_world_pos, R, vrad)) {
                color += f0 * vrad * spec_w;
            }
        }
    }
    // SSR G-buffer: octahedral view-space normal (rg) + roughness (b) + a scalar
    // reflectivity (a, the env-reflection weight's luminance). The resolve pass
    // uses the stored normal directly for an exact reflection ray. The global
    // reflection mode decides whether the resolve runs at all.
    vec3 Nv = normalize(mat3(VIEW_MATRIX) * N);
    float reflectivity = dot(reflectance, vec3(0.2126, 0.7152, 0.0722));
    // Packed for the 10:10:10:2 target: roughness (6 bits) in b's high bits,
    // reflectivity (6 bits) split over b's low 4 and a's 2. Unpacked with an
    // exact texel fetch in the resolves (never filtered).
    uint r6 = uint(round(clamp(roughness, 0.0, 1.0) * 63.0));
    uint f6 = uint(round(clamp(reflectivity, 0.0, 1.0) * 63.0));
    o_gbuf = vec4(oct_encode(Nv), float((r6 << 4) | (f6 & 15u)) / 1023.0, float(f6 >> 4) / 3.0);
    if (FEAT_WATER) {
        // Transparent: the resolve's depth is the floor below, not the surface.
        o_gbuf = vec4(0.0);
    }

    if (FEAT_EMISSION) {
        // Emission uses its own tiling/offset, can scroll, pulse over time, and
        // be masked (the mask shares the emission UV transform).
        vec2 euv = uv_emission + fx.scroll.zw * frame.misc.x;
        float pulse = fx.toon.z > 0.0 ? (0.5 + 0.5 * sin(frame.misc.x * fx.toon.z)) : 1.0;
        float emask = texture(t_emission_mask, uv_emission).r;
        color += texture(t_emission, euv).rgb * pc.emission.rgb * pulse * emask;
    }

    // Matcaps (Poiyomi-style): sample 3 view-space sphere-mapped layers, each
    // scaled by its mask + strength and combined by its per-layer blend mode.
    // Default matcaps are black with strength 0 so unused layers cost nothing.
    if (fx.matcap.x > 0.0 || fx.matcap.y > 0.0 || fx.matcap.z > 0.0) {
        vec3 vn = normalize(mat3(VIEW_MATRIX) * N);
        vec2 muv = vn.xy * 0.5 + 0.5;
        color = blend_matcap(color, texture(t_matcap0, muv).rgb,
                             texture(t_matcap0_mask, uv).r * fx.matcap.x, fx.matcap_blend.x);
        color = blend_matcap(color, texture(t_matcap1, muv).rgb,
                             texture(t_matcap1_mask, uv).r * fx.matcap.y, fx.matcap_blend.y);
        color = blend_matcap(color, texture(t_matcap2, muv).rgb,
                             texture(t_matcap2_mask, uv).r * fx.matcap.z, fx.matcap_blend.z);
    }

    // Toon rim light (Poiyomi-style): a Fresnel edge glow in the rim colour,
    // with tunable power + strength from the FX block.
    if (FEAT_TOON && rim_strength > 0.0) {
        float rim = pow(1.0 - NdotV, max(fx.rim.w, 0.1));
        color += rim * rim_strength * fx.rim.rgb;
    }

    // Viewport render modes (frame.debug.y): 1 = world normals, 2 = indirect/GI
    // term only, 3 = unlit (raw albedo, no lighting/post), 4 = reflection cube
    // (every surface a perfect mirror of u_env at R — reveals the captured cube's
    // orientation directly, no BRDF/roughness), 5 = reflection direction R as RGB,
    // 6 = the realtime Flux screen GI alone (no bake underneath).
    int gi_dbg = int(frame.debug.y + 0.5);
    if (gi_dbg == 1) {
        o_color = vec4(N * 0.5 + 0.5, 1.0);
        return;
    } else if (gi_dbg == 2) {
        o_color = vec4(indirect, 1.0);
        return;
    } else if (gi_dbg == 3) {
        o_color = vec4(albedo.rgb, 1.0);
        return;
    } else if (gi_dbg == 4) {
        // Perfect-mirror sample of the reflection cube (sharp mip 0), no shading.
        o_color = vec4(textureLod(u_env, R, 0.0).rgb * env_scale, 1.0);
        return;
    } else if (gi_dbg == 5) {
        // Reflection direction visualized: +X red, +Y green, +Z blue.
        o_color = vec4(normalize(R) * 0.5 + 0.5, 1.0);
        return;
    } else if (gi_dbg == 6) {
        o_color = vec4(flux_gi, 1.0);
        return;
    }

    // Fog is now a raymarched volumetric medium in the deferred resolve pass
    // (ssr_resolve.frag) — a real participating medium that hangs + drifts in the
    // air, not a per-surface colour tint — so it's no longer applied here.

    if (FEAT_WATER) {
        color += water_behind * water_see * (1.0 - f_schlick(NdotV, vec3(0.02)).x);
    }

    color = apply_postfx(color, gl_FragCoord.xy);
    float alpha = (ALPHA_MODE == 2u) ? albedo.a : 1.0;
    if (FEAT_WATER) {
        alpha = water_alpha;
    }
    // Never hand NaN / Inf to the HDR target: bloom and TAA would spread it.
    if (any(isnan(color)) || any(isinf(color))) {
        color = vec3(0.0);
    }
    o_color = vec4(min(color, vec3(64000.0)), alpha);
}
