#version 450
// citrus standard shader, fragment stage (phase 1: PBR/toon hybrid core).
// Feature toggles are specialization constants: each material's enabled
// feature set selects a pipeline variant; disabled features compile out.

layout(constant_id = 0) const bool FEAT_TOON = false;
layout(constant_id = 1) const bool FEAT_NORMAL_MAP = false;
layout(constant_id = 2) const bool FEAT_EMISSION = false;
layout(constant_id = 3) const uint ALPHA_MODE = 0u; // 0 opaque, 1 cutout, 2 blend

const int MAX_LIGHTS = 16;

struct Light {
    vec4 pos_kind;   // xyz world position, w = kind (0 dir, 1 point, 2 spot)
    vec4 dir_range;  // xyz travel direction (normalized), w = range
    vec4 color;      // rgb color*intensity, w = cos(outer half-angle)
    vec4 spot;       // x = cos(inner half-angle)
};

const int MAX_SHADOW_VIEWS = 12;
const int MAX_PROBE_VOLUMES = 4;

// One baked light-probe volume: world->local plus the grid layout (matches
// GpuProbeVolume on the CPU).
struct ProbeVolume {
    mat4 world_to_local; // local box spans -size/2 .. +size/2
    vec4 size_base;      // xyz = local box size, w = first probe index (sh_base)
    vec4 counts;         // xyz = probe counts per axis
};

layout(set = 0, binding = 0) uniform FrameData {
    mat4 view;
    mat4 proj;
    mat4 view_proj;
    vec4 camera_pos;
    vec4 light_dir;
    vec4 light_color;
    vec4 ambient;
    vec4 misc; // x = time, y = light count, z = shadow spacing, w = probe-volume count
    vec4 postfx0; // x tonemap mode, y exposure EV, z grade exposure, w contrast
    vec4 postfx1; // x saturation, y temperature, z tint, w grading enabled
    vec4 postfx2; // x vignette enabled, y intensity, z smoothness, w screen width
    vec4 postfx3; // xyz vignette color, w screen height
    vec4 cascade_splits; // far view-space distance of each directional cascade
    Light lights[MAX_LIGHTS];
    mat4 shadow_vp[MAX_SHADOW_VIEWS];
    ProbeVolume probe_volumes[MAX_PROBE_VOLUMES];
    vec4 debug; // x = lightmap-UV checker preview, y = gi debug, z = screen-GI active
    mat4 inv_view;
    mat4 inv_proj;
    vec4 ssr; // x = enabled, y = intensity, z = max view-space dist, w = roughness cutoff
    vec4 refl_center;  // xyz = probe box center, w = intensity (0 = no probe zone)
    vec4 refl_extents; // xyz = probe box half-extents, w = box-projection enabled
    vec4 fog_color;    // xyz = fog colour, w = density (0 = no fog)
    vec4 fog_params;   // x = height falloff, y = height ref, z = start distance
} frame;

// Bindings 5 (scene depth) and 6 (prev colour) feed the deferred SSR resolve
// pass now, not this shader; they stay in the set-0 layout but go unused here.
// Reflection probe: prefiltered environment cubemap (mirror at mip 0, rougher
// toward higher mips). Sampled by reflection direction + roughness.
layout(set = 0, binding = 7) uniform samplerCube u_env;
const float ENV_MAX_LOD = 6.0; // matches cube_mip_count(64) - 1

layout(set = 0, binding = 1) uniform sampler2DArrayShadow u_shadow;

// Baked probe SH-L1: 4 coefficients (RGB in .xyz) per probe.
struct Probe { vec4 c[5]; }; // c[0..3]=radiance+dist SH, c[4]=dist² SH (Chebyshev)
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
            vec2 ndc = uv * 2.0 - 1.0;
            ndc.y = -ndc.y;
            vec4 farp = frame.inv_proj * vec4(ndc, 1.0, 1.0);
            vec3 dir = normalize(farp.xyz / farp.w);
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
    vec3 center = -v.world_to_local[3].xyz; // meta stores translate(-center)
    vec3 cell = size / max(vec3(cnt - 1), vec3(1.0));
    vec3 g = clamp(t, vec3(0.0), vec3(1.0)) * vec3(cnt - 1);
    ivec3 g0 = ivec3(floor(g));
    vec3 f = g - vec3(g0);
    // Smoothstep (Hermite) the interpolation factor so the trilinear blend is
    // C1-continuous across cell boundaries. Plain linear trilinear has a kink in
    // its gradient at every boundary, which reads as visible facets/"squares" on
    // a smooth gradient; this removes that without needing a finer grid.
    f = f * f * (3.0 - 2.0 * f);

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
        vec3 ppos = center + (gn - 0.5) * size;
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
            vis = max(variance / (variance + dd * dd), 0.25);
        } else {
            vis = clamp(1.0 - (pd - md) / max(band, 1e-3), 0.25, 1.0);
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
//   - near the OUTERMOST cascade's boundary we instead drop coverage toward 0 so
//     the GI fades smoothly into ambient rather than hard-stopping at the box.
float sample_probes(vec3 world_pos, vec3 n, out vec3 irradiance) {
    int vcount = int(frame.misc.w + 0.5);
    for (int vi = 0; vi < vcount && vi < MAX_PROBE_VOLUMES; ++vi) {
        ProbeVolume v = frame.probe_volumes[vi];
        vec3 t = volume_coords(v, world_pos);
        if (any(lessThan(t, vec3(0.0))) || any(greaterThan(t, vec3(1.0)))) {
            continue;
        }
        vec3 fine = sample_volume(v, world_pos, n);
        // Distance to the nearest box face in normalized units (0 at the face,
        // 0.5 at the center).
        vec3 edge = min(t, vec3(1.0) - t);
        float e = min(edge.x, min(edge.y, edge.z));
        bool has_next = (vi + 1 < vcount && vi + 1 < MAX_PROBE_VOLUMES);
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
        // Outermost cascade: wide, gentle fade-to-ambient so GI melts into the
        // ambient term over a broad ring instead of stopping at a visible line.
        irradiance = fine;
        return smoothstep(0.0, 0.2, e);
    }
    irradiance = vec3(0.0);
    return 0.0;
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
        float depth = -(frame.view * vec4(world_pos, 1.0)).z;
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

// Extended per-material params (beyond the 128-byte push block).
layout(set = 1, binding = 4) uniform MaterialFx {
    vec4 rim;    // rgb rim colour, w rim power
    vec4 toon;   // x rim strength, y ramp smoothness, z emission pulse, w _
    vec4 scroll; // xy base UV scroll, zw emission UV scroll
    vec4 matcap; // xyz matcap layer strengths, w _
    vec4 matcap_blend; // xyz blend modes (0 add / 1 mul / 2 replace), w _
    vec4 albedo_st;   // xy tiling (scale), zw offset
    vec4 normal_st;
    vec4 orm_st;
    vec4 emission_st;
    vec4 orm_invert;  // x ao, y roughness, z metallic: 1 = invert sampled value
    vec4 parallax;    // x = displacement scale (0 = off)
} fx;

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

void main() {
    // Default the SSR G-buffer to "not a reflector"; the reflection block below
    // overwrites it for lit surfaces. Keeps early debug returns valid.
    o_gbuf = vec4(0.0);
    // Lightmap-UV checker preview: visualise per-object texel density. params1.w
    // carries this object's lightmap resolution (texels/side) in preview mode; a
    // fixed 8-texel cell means low-res objects show big squares, high-res small.
    if (frame.debug.x > 0.5) {
        float res = pc.params1.w;
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
        vec3 to_eye = frame.camera_pos.xyz - v_world_pos;
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

    vec3 V = normalize(frame.camera_pos.xyz - v_world_pos);
    float NdotV = max(dot(N, V), 1e-4);
    vec3 f0 = mix(vec3(0.04), albedo.rgb, metallic);
    vec3 diffuse_color = albedo.rgb * (1.0 - metallic);

    // Accumulate every active scene light. The CPU guarantees at least one
    // (a directional fallback when the scene has no light objects).
    int light_count = int(frame.misc.y + 0.5);
    vec3 color = vec3(0.0);
    for (int i = 0; i < light_count && i < MAX_LIGHTS; ++i) {
        Light light = frame.lights[i];
        int kind = int(light.pos_kind.w + 0.5);

        // Direction to the light + distance-based attenuation.
        vec3 L;
        float attenuation = 1.0;
        if (kind == 0) {
            L = normalize(-light.dir_range.xyz);
        } else {
            vec3 to_light = light.pos_kind.xyz - v_world_pos;
            float dist = length(to_light);
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

        vec3 H = normalize(V + L);
        float NdotL = max(dot(N, L), 0.0);
        float NdotH = max(dot(N, H), 0.0);
        float VdotH = max(dot(V, H), 0.0);

        // Cook-Torrance specular + energy-conserving Lambertian diffuse: the
        // Fresnel term F is the specular reflectance kS, so the diffuse keeps
        // only the remaining energy kD = (1 - F). (Metallic is already folded
        // into diffuse_color, which is 0 for pure metals.)
        vec3 F = f_schlick(VdotH, f0);
        vec3 spec = d_ggx(NdotH, roughness * roughness)
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

        color += lit * radiance * shadow_factor(light, v_world_pos, N, L);
    }

    // Indirect diffuse (as irradiance/PI), in priority order:
    //   1. baked lightmap for static objects (params1.w = array layer, >= 0),
    //   2. baked probe SH where a volume covers this fragment,
    //   3. flat scene ambient.
    vec3 indirect;
    if (pc.params1.w >= 0.0) {
        int layer = int(pc.params1.w + 0.5);
        indirect = texture(u_lightmap, vec3(v_uv1, float(layer))).rgb / PI;
    } else if (frame.debug.z > 0.5) {
        // Screen-space GI: depth-aware upsample of the sparse screen probes.
        // Emissive surfaces are in the depth prepass again, so they sample their
        // OWN GI here (their emission still dominates the look).
        vec2 suv = gl_FragCoord.xy / vec2(frame.postfx2.w, frame.postfx3.w);
        // View-space position + normal so the upsample can reject only true depth
        // edges, keeping coplanar neighbours on steep/grazing faces (no GI noise).
        vec3 Pv = (frame.view * vec4(v_world_pos, 1.0)).xyz;
        vec3 Nv = normalize(mat3(frame.view) * N);
        indirect = screen_gi_upsample(suv, Pv, Nv);
    } else {
        // Probe GI where a cascade covers the fragment, fading to flat ambient
        // at the outermost cascade's edge (coverage < 1) and beyond it (0).
        vec3 probe_irr;
        float cov = sample_probes(v_world_pos, N, probe_irr);
        indirect = mix(frame.ambient.rgb, probe_irr, cov);
    }
    // Energy-conserving ambient: split indirect into diffuse (non-reflected) and
    // a cheap specular share so metals/smooth surfaces pick up environment colour
    // instead of reading flat. Uses the probe/lightmap irradiance as a stand-in
    // for environment radiance (no reflection probes yet). Approximate, but it
    // keeps metals lively. Smoother surfaces concentrate the spec response.
    vec3 F_amb = f_schlick_rough(NdotV, f0, roughness);
    vec3 kd_amb = vec3(1.0) - F_amb;
    color += kd_amb * indirect * diffuse_color * ao;
    float spec_amb = mix(1.0, 0.25, roughness); // rough surfaces scatter it away
    // Specular environment radiance, in priority order:
    //   1. SSR hit (on-screen, smooth/metallic), then
    //   2. the prefiltered reflection-probe cube (roughness -> mip), then
    //   3. the indirect-irradiance stand-in (no skybox -> neutral black cube).
    vec3 R = reflect(-V, N);
    // Box-projected parallax (Unity-style) when a reflection-probe zone covers
    // this fragment: reproject the reflection ray onto the probe's box so flat
    // surfaces line up with the captured environment instead of treating it as
    // infinitely distant. `refl_center.w` = probe intensity (0 = no zone).
    float probe_intensity = frame.refl_center.w;
    if (probe_intensity > 0.0 && frame.refl_extents.w > 0.5) {
        vec3 bmin = frame.refl_center.xyz - frame.refl_extents.xyz;
        vec3 bmax = frame.refl_center.xyz + frame.refl_extents.xyz;
        vec3 invR = 1.0 / R;
        vec3 t1 = (bmax - v_world_pos) * invR;
        vec3 t2 = (bmin - v_world_pos) * invR;
        vec3 tmax = max(t1, t2);
        float dist = min(min(tmax.x, tmax.y), tmax.z);
        vec3 hit = v_world_pos + R * dist;
        R = hit - frame.refl_center.xyz;
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
    vec3 spec_env = textureLod(u_env, R, roughness * ENV_MAX_LOD).rgb * env_scale;
    // Reflectance weight that multiplies environment radiance. The resolve pass
    // re-derives env radiance from this same cube and swaps in the SSR hit:
    //   final += reflectance * conf * (ssr_radiance - env_radiance)
    // so SSR exactly replaces the env reflection where it has a confident hit and
    // leaves the env reflection untouched (this colour, fogged below) elsewhere.
    // Split-sum environment BRDF instead of a raw Fresnel*spec term: bounded,
    // roughness-correct specular that doesn't blow out to a harsh mirror band at
    // grazing angles on rough/dielectric surfaces.
    vec2 env_ab = env_brdf_approx(roughness, NdotV);
    vec3 reflectance = (f0 * env_ab.x + vec3(env_ab.y)) * ao;
    color += reflectance * spec_env;
    // SSR G-buffer: octahedral view-space normal (rg) + roughness (b) + a scalar
    // reflectivity (a, the env-reflection weight's luminance). The resolve pass
    // uses the stored normal directly for an exact reflection ray.
    vec3 Nv = normalize(mat3(frame.view) * N);
    float reflectivity = dot(reflectance, vec3(0.2126, 0.7152, 0.0722));
    o_gbuf = vec4(oct_encode(Nv), roughness, reflectivity);

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
        vec3 vn = normalize(mat3(frame.view) * N);
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

    // GI debug views (frame.debug.y): 1 = world normals, 2 = indirect/GI term
    // only (isolates the probe-grid blockiness on screen).
    int gi_dbg = int(frame.debug.y + 0.5);
    if (gi_dbg == 1) {
        o_color = vec4(N * 0.5 + 0.5, 1.0);
        return;
    } else if (gi_dbg == 2) {
        o_color = vec4(indirect, 1.0);
        return;
    }

    // Fog is now a raymarched volumetric medium in the deferred resolve pass
    // (ssr_resolve.frag) — a real participating medium that hangs + drifts in the
    // air, not a per-surface colour tint — so it's no longer applied here.

    color = apply_postfx(color, gl_FragCoord.xy);
    float alpha = (ALPHA_MODE == 2u) ? albedo.a : 1.0;
    o_color = vec4(color, alpha);
}
