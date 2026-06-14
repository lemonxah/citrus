#version 450
// citrus standard shader — fragment stage (phase 1: PBR/toon hybrid core).
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
    vec4 cascade_splits; // far view-space distance of each directional cascade
    Light lights[MAX_LIGHTS];
    mat4 shadow_vp[MAX_SHADOW_VIEWS];
    ProbeVolume probe_volumes[MAX_PROBE_VOLUMES];
} frame;

layout(set = 0, binding = 1) uniform sampler2DArrayShadow u_shadow;

// Baked probe SH-L1: 4 coefficients (RGB in .xyz) per probe.
struct Probe { vec4 c[4]; };
layout(set = 0, binding = 2) readonly buffer Probes { Probe probes[]; };

// Baked lightmaps (static-object GI): one array layer per object, sampled by
// uv1. Each texel is incoming irradiance E; the caller applies albedo/PI.
layout(set = 0, binding = 3) uniform sampler2DArray u_lightmap;

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

// Sample one probe by integer grid coords (x fastest, then y, then z).
vec3 probe_at(ProbeVolume v, ivec3 g, ivec3 cnt, vec3 n) {
    g = clamp(g, ivec3(0), cnt - 1);
    uint idx = uint(v.size_base.w) + uint(g.x + g.y * cnt.x + g.z * cnt.x * cnt.y);
    return sh_eval(idx, n);
}

// Trilinearly-interpolated baked irradiance at world_pos for normal n. Returns
// false when no probe volume contains the point (caller falls back to ambient).
bool sample_probes(vec3 world_pos, vec3 n, out vec3 irradiance) {
    int vcount = int(frame.misc.w + 0.5);
    for (int vi = 0; vi < vcount && vi < MAX_PROBE_VOLUMES; ++vi) {
        ProbeVolume v = frame.probe_volumes[vi];
        vec3 local = (v.world_to_local * vec4(world_pos, 1.0)).xyz;
        vec3 size = max(v.size_base.xyz, vec3(1e-4));
        vec3 t = (local + size * 0.5) / size; // 0..1 across the box
        if (any(lessThan(t, vec3(0.0))) || any(greaterThan(t, vec3(1.0)))) {
            continue;
        }
        ivec3 cnt = ivec3(v.counts.xyz + 0.5);
        vec3 g = t * vec3(cnt - 1);
        ivec3 g0 = ivec3(floor(g));
        vec3 f = g - vec3(g0);
        // 8-corner trilinear blend of SH-evaluated irradiance.
        vec3 c000 = probe_at(v, g0 + ivec3(0,0,0), cnt, n);
        vec3 c100 = probe_at(v, g0 + ivec3(1,0,0), cnt, n);
        vec3 c010 = probe_at(v, g0 + ivec3(0,1,0), cnt, n);
        vec3 c110 = probe_at(v, g0 + ivec3(1,1,0), cnt, n);
        vec3 c001 = probe_at(v, g0 + ivec3(0,0,1), cnt, n);
        vec3 c101 = probe_at(v, g0 + ivec3(1,0,1), cnt, n);
        vec3 c011 = probe_at(v, g0 + ivec3(0,1,1), cnt, n);
        vec3 c111 = probe_at(v, g0 + ivec3(1,1,1), cnt, n);
        vec3 x00 = mix(c000, c100, f.x);
        vec3 x10 = mix(c010, c110, f.x);
        vec3 x01 = mix(c001, c101, f.x);
        vec3 x11 = mix(c011, c111, f.x);
        vec3 y0 = mix(x00, x10, f.y);
        vec3 y1 = mix(x01, x11, f.y);
        irradiance = max(mix(y0, y1, f.z), vec3(0.0));
        return true;
    }
    return false;
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
    int vcount = int(light.spot.w + 0.5);
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
    // 5x5 PCF: average many hardware-PCF taps to soften the shadow edge.
    // Tap spacing (softness / shadow_resolution) comes from the CPU so it
    // tracks the runtime shadow resolution + softness setting.
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

const float PI = 3.14159265359;

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

// Roughness-aware Fresnel for the ambient/indirect term (rougher surfaces
// reflect less at grazing angles, so more energy stays in the diffuse).
vec3 f_schlick_rough(float cosT, vec3 f0, float rough) {
    vec3 fmax = max(vec3(1.0 - rough), f0);
    return f0 + (fmax - f0) * pow(clamp(1.0 - cosT, 0.0, 1.0), 5.0);
}

void main() {
    vec4 albedo = texture(t_albedo, v_uv) * pc.base_color * v_color;
    if (ALPHA_MODE == 1u && albedo.a < pc.params1.x) {
        discard;
    }

    vec3 N = normalize(v_normal);
    if (!gl_FrontFacing) {
        N = -N;
    }
    if (FEAT_NORMAL_MAP) {
        vec3 T = normalize(v_tangent.xyz - N * dot(v_tangent.xyz, N));
        vec3 B = cross(N, T) * v_tangent.w;
        vec3 tn = texture(t_normal, v_uv).xyz * 2.0 - 1.0;
        tn.xy *= pc.params1.y;
        N = normalize(mat3(T, B, N) * normalize(tn));
    }

    vec3 orm = texture(t_orm, v_uv).rgb;
    float ao = mix(1.0, orm.r, pc.params1.z);
    float roughness = clamp(orm.g * pc.params0.y, 0.045, 1.0);
    float metallic = clamp(orm.b * pc.params0.x, 0.0, 1.0);

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
            float steps = max(pc.params0.z, 2.0);
            float banded = floor(clamp(NdotL, 0.0, 0.999) * steps) / (steps - 1.0);
            vec3 lit_toon = (kd * diffuse_color / PI) * banded
                + spec * NdotL * step(0.001, banded);
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
    } else if (!sample_probes(v_world_pos, N, indirect)) {
        indirect = frame.ambient.rgb;
    }
    // Energy-conserving ambient diffuse: keep only the non-reflected share.
    vec3 kd_amb = vec3(1.0) - f_schlick_rough(NdotV, f0, roughness);
    color += kd_amb * indirect * diffuse_color * ao;

    if (FEAT_EMISSION) {
        color += texture(t_emission, v_uv).rgb * pc.emission.rgb;
    }

    float alpha = (ALPHA_MODE == 2u) ? albedo.a : 1.0;
    o_color = vec4(color, alpha);
}
