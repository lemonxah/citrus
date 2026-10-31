//! Custom shader files: user-authored fragment shaders in GLSL (`.frag`) or
//! HLSL (`.hlsl`), compiled to SPIR-V at runtime via `glslc` and reflected into
//! inspector properties (see shader_props.rs for the declaration syntax and
//! docs/shaders.md for the authoring guide).
//!
//! A custom shader is a fragment-stage body. The engine prepends a preamble
//! declaring the frame UBO, lights/shadows/GI helpers, the fixed material
//! texture slots, vertex inputs and outputs, then generated declarations for
//! the shader's own properties: v1 push-constant defines, the v2 std140
//! material block (set 2, binding 0), texture samplers (set 2, bindings 1..)
//! and keyword specialization constants. Do NOT write a `#version` line.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

pub use crate::shader_props::*;

pub const SHADER_EXTENSION: &str = "frag";
pub const HLSL_SHADER_EXTENSION: &str = "hlsl";

/// True for files the custom-shader library loads (`.frag` / `.hlsl`).
pub fn is_shader_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(SHADER_EXTENSION) || e.eq_ignore_ascii_case(HLSL_SHADER_EXTENSION))
}

pub fn shader_lang_for(path: &Path) -> ShaderLang {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case(HLSL_SHADER_EXTENSION) => ShaderLang::Hlsl,
        _ => ShaderLang::Glsl,
    }
}

/// Fixed GLSL interface. Must match the standard pipeline layout (set 0 frame
/// resources, set 1 material textures, 128-byte push constants) and
/// standard.vert's outputs; FrameData mirrors `FrameUbo` in citrus-render.
const GLSL_PREAMBLE: &str = r#"#version 450
const int MAX_LIGHTS = 128;
const int MAX_SHADOW_VIEWS = 12;
const int MAX_PROBE_VOLUMES = 4;
const float CITRUS_PI = 3.14159265359;

struct Light { vec4 pos_kind; vec4 dir_range; vec4 color; vec4 spot; };
struct ProbeVolume { mat4 world_to_local; vec4 size_base; vec4 counts; };
struct Probe { vec4 c[6]; };

layout(set = 0, binding = 0) uniform FrameData {
    mat4 view;
    mat4 proj;
    mat4 view_proj;
    vec4 camera_pos;
    vec4 light_dir;
    vec4 light_color;
    vec4 ambient;
    vec4 misc;            // x time, y light count, z shadow spacing, w probe-volume count
    vec4 postfx0;
    vec4 postfx1;
    vec4 postfx2;
    vec4 postfx3;
    vec4 cascade_splits;
    Light lights[MAX_LIGHTS];
    mat4 shadow_vp[MAX_SHADOW_VIEWS];
    ProbeVolume probe_volumes[MAX_PROBE_VOLUMES];
    vec4 debug;
    mat4 inv_view;
    mat4 inv_proj;
    vec4 ssr;
    vec4 refl_center;
    vec4 refl_extents;
    vec4 fog_color;
    vec4 fog_params;
    vec4 refl_box;
    vec4 refl2_center;
    vec4 refl2_extents;
    vec4 refl2_box;
    vec4 refl_blend;
} frame;
layout(set = 0, binding = 1) uniform sampler2DArrayShadow u_shadow;
layout(set = 0, binding = 2) readonly buffer Probes { Probe probes[]; };
layout(set = 0, binding = 3) uniform sampler2DArray u_lightmap;
layout(set = 0, binding = 4) uniform sampler2D u_screen_gi;
layout(set = 0, binding = 7) uniform samplerCube u_env;
layout(set = 0, binding = 9) uniform samplerCube u_env_b;

layout(set = 1, binding = 0) uniform sampler2D t_albedo;
layout(set = 1, binding = 1) uniform sampler2D t_normal;
layout(set = 1, binding = 2) uniform sampler2D t_orm;
layout(set = 1, binding = 3) uniform sampler2D t_emission;
layout(push_constant) uniform Push {
    mat4 model;
    vec4 d0;
    vec4 d1;
    vec4 d2;
    vec4 d3;
} pc;
layout(location = 0) in vec3 v_world_pos;
layout(location = 1) in vec3 v_normal;
layout(location = 2) in vec2 v_uv;
layout(location = 3) in vec4 v_color;
layout(location = 4) in vec4 v_tangent;
layout(location = 5) in vec2 v_uv1;
layout(location = 0) out vec4 o_color;
// Deferred-SSR G-buffer (reflectance.rgb, roughness.a). Zeroed before the
// shader body runs (not a reflector); write it to opt into SSR.
layout(location = 1) out vec4 o_gbuf;
// Material alpha mode: 0 opaque, 1 cutout, 2 blend (pipeline state).
layout(constant_id = 63) const int citrus_alpha_mode = 0;
#define u_time (frame.misc.x)
#define u_light_count (int(frame.misc.y + 0.5))
#define u_camera_pos (frame.camera_pos.xyz)
#define u_light_dir (frame.light_dir.xyz)
#define u_light_color (frame.light_color.rgb)
#define u_ambient (frame.ambient.rgb)

// Probe SH-L1 radiance -> irradiance/PI in direction n.
vec3 citrus_sh(uint i, vec3 n) {
    return probes[i].c[0].rgb * 0.282095
         + 0.325735 * (probes[i].c[1].rgb * n.y
                     + probes[i].c[2].rgb * n.z
                     + probes[i].c[3].rgb * n.x);
}

// Smooth trilinear probe irradiance from the first volume containing the
// point; `coverage` fades to 0 at the outermost volume's edge.
vec3 citrus_probe_irradiance(vec3 world_pos, vec3 n, out float coverage) {
    int vc = int(frame.misc.w + 0.5);
    for (int vi = 0; vi < vc && vi < MAX_PROBE_VOLUMES; ++vi) {
        ProbeVolume v = frame.probe_volumes[vi];
        vec3 size = max(v.size_base.xyz, vec3(1e-4));
        vec3 local = (v.world_to_local * vec4(world_pos, 1.0)).xyz;
        vec3 t = (local + size * 0.5) / size;
        if (any(lessThan(t, vec3(0.0))) || any(greaterThan(t, vec3(1.0)))) continue;
        ivec3 cnt = ivec3(v.counts.xyz + 0.5);
        vec3 g = clamp(t, vec3(0.0), vec3(1.0)) * vec3(cnt - 1);
        ivec3 g0 = ivec3(floor(g));
        vec3 f = g - vec3(g0);
        f = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
        vec3 sum = vec3(0.0); float wsum = 0.0;
        for (int c = 0; c < 8; ++c) {
            ivec3 off = ivec3(c & 1, (c >> 1) & 1, (c >> 2) & 1);
            ivec3 gc = clamp(g0 + off, ivec3(0), cnt - 1);
            vec3 tw = mix(vec3(1.0) - f, f, vec3(off));
            float w = tw.x * tw.y * tw.z;
            uint idx = uint(v.size_base.w) + uint(gc.x + gc.y * cnt.x + gc.z * cnt.x * cnt.y);
            sum += w * citrus_sh(idx, n); wsum += w;
        }
        vec3 edge = min(t, vec3(1.0) - t);
        float e = min(edge.x, min(edge.y, edge.z));
        coverage = (vi + 1 < vc) ? 1.0 : smoothstep(0.0, 0.2, e);
        return wsum > 1e-5 ? max(sum / wsum, vec3(0.0)) : vec3(0.0);
    }
    coverage = 0.0;
    return vec3(0.0);
}

// Probe GI blended to flat ambient outside every volume.
// FluxVoxel moving emitters (light kind 3): the voxel grid's injection kernel,
// evaluated per pixel (as the standard shader does).
vec3 citrus_voxel_emitters(vec3 world_pos, vec3 n) {
    vec3 sum = vec3(0.0);
    int count = int(frame.misc.y + 0.5);
    for (int i = 0; i < count && i < MAX_LIGHTS; ++i) {
        Light l = frame.lights[i];
        if (int(l.pos_kind.w + 0.5) != 3) continue;
        vec3 d = l.pos_kind.xyz - world_pos;
        float dist = length(d);
        float f = clamp(1.0 - dist / max(l.dir_range.w, 1e-3), 0.0, 1.0);
        vec3 L = dist > 1e-4 ? d / dist : n;
        sum += l.color.rgb * (f * f * max(0.11141 + 0.25465 * dot(n, L), 0.0));
    }
    return sum;
}

vec3 citrus_gi(vec3 world_pos, vec3 n) {
    float cov;
    vec3 irr = citrus_probe_irradiance(world_pos, n, cov);
    return mix(u_ambient, irr, cov) + citrus_voxel_emitters(world_pos, n);
}

// Screen-space (Flux) GI gather at this pixel.
vec3 citrus_screen_gi() {
    vec2 suv = gl_FragCoord.xy / vec2(max(frame.postfx2.w, 1.0), max(frame.postfx3.w, 1.0));
    return texture(u_screen_gi, suv).rgb;
}

// Indirect diffuse (irradiance/PI) for THIS object, matching the standard
// shader's priority: lightmap (+ realtime probes / mixed Flux), screen GI,
// probe GI, ambient.
vec3 citrus_baked_gi(vec3 world_pos, vec3 n) {
    float mode = frame.debug.z;
    if (pc.d3.w >= 0.0) {
        int layer = int(pc.d3.w + 0.5);
        vec3 gi = texture(u_lightmap, vec3(v_uv1, float(layer))).rgb / CITRUS_PI;
        if (mode > -0.5) { float c; gi += citrus_probe_irradiance(world_pos, n, c); }
        if (mode > 1.5) gi += citrus_screen_gi();
        return gi;
    }
    if (mode > 0.5) {
        vec3 gi = citrus_screen_gi();
        if (mode > 1.5) gi += citrus_gi(world_pos, n);
        return gi;
    }
    if (mode < -0.5) return u_ambient;
    return citrus_gi(world_pos, n);
}

// Light i: direction to the light and its attenuated radiance (no N.L, no
// shadow). False when the light contributes nothing here (or is baked into
// this object's lightmap).
bool citrus_light(int i, vec3 world_pos, out vec3 L, out vec3 radiance) {
    Light l = frame.lights[i];
    L = vec3(0.0, 1.0, 0.0);
    radiance = vec3(0.0);
    if (l.spot.y < 0.0 && l.spot.w > 0.5 && pc.d3.w >= 0.0) return false;
    int kind = int(l.pos_kind.w + 0.5);
    // Kind 3 (FluxVoxel emitter) is indirect light, added by citrus_gi.
    if (kind == 3) return false;
    float atten = 1.0;
    if (kind == 0) {
        L = normalize(-l.dir_range.xyz);
    } else {
        vec3 d = l.pos_kind.xyz - world_pos;
        float dist = length(d);
        if (dist < 1e-4) return false;
        L = d / dist;
        float range = max(l.dir_range.w, 1e-3);
        float fall = clamp(1.0 - dist / range, 0.0, 1.0);
        atten = fall * fall / (1.0 + dist * dist);
        if (kind == 2) {
            float cd = dot(normalize(l.dir_range.xyz), -L);
            atten *= smoothstep(l.color.w, l.spot.x, cd);
        }
    }
    radiance = l.color.rgb * atten;
    return atten > 0.0;
}

// Shadow visibility of light i at a point (1 lit, 0 shadowed); the same
// cascades / cube faces / PCF as the standard shader.
float citrus_shadow(int i, vec3 world_pos, vec3 n, vec3 L) {
    Light light = frame.lights[i];
    int base = int(light.spot.y);
    if (base < 0) return 1.0;
    float bias = light.spot.z;
    bool soft = light.spot.w >= 0.0;
    int vcount = int(abs(light.spot.w) + 0.5);
    int kind = int(light.pos_kind.w + 0.5);
    int sub = 0;
    if (kind == 0 && vcount > 1) {
        float depth = -(frame.view * vec4(world_pos, 1.0)).z;
        int start = vcount - 1;
        for (int c = 0; c < vcount && c < 4; ++c) {
            if (depth < frame.cascade_splits[c]) { start = c; break; }
        }
        float margin = frame.misc.z * 3.0;
        sub = vcount - 1;
        for (int c = start; c < vcount && c < 4; ++c) {
            vec4 cc = frame.shadow_vp[base + c] * vec4(world_pos, 1.0);
            if (cc.w <= 0.0) { sub = c; continue; }
            vec2 cuv = (cc.xy / cc.w) * 0.5 + 0.5;
            sub = c;
            if (cuv.x >= margin && cuv.x <= 1.0 - margin && cuv.y >= margin && cuv.y <= 1.0 - margin) break;
        }
    } else if (vcount == 6) {
        vec3 fl = world_pos - light.pos_kind.xyz;
        vec3 a = abs(fl);
        if (a.x >= a.y && a.x >= a.z) sub = fl.x > 0.0 ? 0 : 1;
        else if (a.y >= a.z) sub = fl.y > 0.0 ? 2 : 3;
        else sub = fl.z > 0.0 ? 4 : 5;
    }
    int layer = base + sub;
    vec4 lc = frame.shadow_vp[layer] * vec4(world_pos, 1.0);
    if (lc.w <= 0.0) return 1.0;
    vec3 proj = lc.xyz / lc.w;
    vec2 uv = proj.xy * 0.5 + 0.5;
    float ndotl = clamp(dot(n, L), 0.0, 1.0);
    float slope = sqrt(max(1.0 - ndotl * ndotl, 0.0)) / max(ndotl, 0.2);
    float ref = proj.z - (bias + 0.0008) * (1.0 + slope);
    if (ref > 1.0) return 1.0;
    if (vcount != 6 && (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0)) return 1.0;
    if (!soft) return texture(u_shadow, vec4(clamp(uv, vec2(0.0), vec2(1.0)), float(layer), ref));
    float spacing = frame.misc.z;
    float sum = 0.0;
    for (int dx = -2; dx <= 2; ++dx) {
        for (int dy = -2; dy <= 2; ++dy) {
            vec2 suv = clamp(uv + vec2(float(dx), float(dy)) * spacing, vec2(0.0), vec2(1.0));
            sum += texture(u_shadow, vec4(suv, float(layer), ref));
        }
    }
    return sum / 25.0;
}

// Unshadowed Lambert diffuse from every scene light.
vec3 citrus_direct_diffuse(vec3 world_pos, vec3 n) {
    vec3 sum = vec3(0.0);
    for (int i = 0; i < u_light_count && i < MAX_LIGHTS; ++i) {
        vec3 L, rad;
        if (citrus_light(i, world_pos, L, rad)) sum += rad * max(dot(n, L), 0.0);
    }
    return sum;
}

// Shadowed Lambert diffuse from every scene light.
vec3 citrus_direct_lambert(vec3 world_pos, vec3 n) {
    vec3 sum = vec3(0.0);
    for (int i = 0; i < u_light_count && i < MAX_LIGHTS; ++i) {
        vec3 L, rad;
        if (!citrus_light(i, world_pos, L, rad)) continue;
        float ndl = max(dot(n, L), 0.0);
        if (ndl > 0.0) sum += rad * ndl * citrus_shadow(i, world_pos, n, L);
    }
    return sum;
}

// Environment / reflection-probe radiance along R (box-projected inside a
// probe zone), prefiltered by roughness.
vec3 citrus_probe_dir(vec3 world_pos, vec3 R, vec3 capture, vec3 center, vec3 extents, bool project) {
    if (!project) return R;
    vec3 bmin = center - extents;
    vec3 bmax = center + extents;
    vec3 eps = extents * 0.02 + 0.02;
    if (!(all(greaterThanEqual(world_pos, bmin - eps)) && all(lessThanEqual(world_pos, bmax + eps)))) return R;
    vec3 invR = 1.0 / R;
    vec3 tmax = max((bmax - world_pos) * invR, (bmin - world_pos) * invR);
    float dist = max(min(min(tmax.x, tmax.y), tmax.z), 0.0);
    return world_pos + R * dist - capture;
}
float citrus_probe_influence(vec3 p, vec3 center, vec3 extents, float blend) {
    vec3 d = extents - abs(p - center);
    float inner = min(d.x, min(d.y, d.z));
    if (blend <= 1e-4) return inner >= 0.0 ? 1.0 : 0.0;
    return clamp(inner / blend, 0.0, 1.0);
}
vec3 citrus_env(vec3 world_pos, vec3 R, float roughness) {
    float intensity = frame.refl_center.w;
    if (intensity < 0.0) return u_ambient;
    vec3 R0 = R;
    if (intensity > 0.0) {
        R0 = citrus_probe_dir(world_pos, R, frame.refl_center.xyz, frame.refl_box.xyz, frame.refl_extents.xyz, frame.refl_extents.w > 0.5);
    }
    float scale = intensity > 0.0 ? intensity : frame.ambient.w;
    vec3 env = textureLod(u_env, R0, roughness * float(textureQueryLevels(u_env) - 1)).rgb * scale;
    // Second reflection probe, blended like the standard shader.
    if (frame.refl2_center.w > 0.0) {
        vec3 R1 = citrus_probe_dir(world_pos, R, frame.refl2_center.xyz, frame.refl2_box.xyz, frame.refl2_extents.xyz, frame.refl2_extents.w > 0.5);
        vec3 env_b = textureLod(u_env_b, R1, roughness * float(textureQueryLevels(u_env_b) - 1)).rgb * frame.refl2_center.w;
        float f0 = citrus_probe_influence(world_pos, frame.refl_box.xyz, frame.refl_extents.xyz, frame.refl_blend.x);
        float f1 = citrus_probe_influence(world_pos, frame.refl2_box.xyz, frame.refl2_extents.xyz, frame.refl_blend.y);
        env = mix(env, env_b, frame.refl_blend.z > 0.5 ? f1 : f1 * (1.0 - f0));
    }
    return env;
}

// 4x4 ordered-dither threshold in (0, 1) for screen-door transparency.
float citrus_dither(vec2 fragcoord) {
    ivec2 p = ivec2(fragcoord) & 3;
    int i = p.x + p.y * 4;
    const float m[16] = float[16](0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0,
                                  3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);
    return (m[i] + 0.5) / 16.0;
}

vec3 citrus_aces(vec3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), 0.0, 1.0);
}

// The scene's Volume post chain (exposure, grade, tonemap, vignette), as the
// built-in shaders apply it. Call right before writing o_color.
vec3 citrus_postfx(vec3 color, vec2 fragcoord) {
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
    if (mode == 1) color = color / (color + vec3(1.0));
    else if (mode == 2) color = citrus_aces(color);
    if (frame.postfx2.x > 0.5) {
        vec2 uv = fragcoord / vec2(max(frame.postfx2.w, 1.0), max(frame.postfx3.w, 1.0));
        float dist = length(uv - 0.5) * 1.41421356;
        float sm = max(frame.postfx2.z, 1e-3);
        float mask = clamp((dist - (1.0 - sm)) / sm, 0.0, 1.0) * frame.postfx2.y;
        color = mix(color, frame.postfx3.xyz, mask);
    }
    return color;
}
"#;

/// Fixed HLSL interface: same bindings as the GLSL preamble. Textures pair a
/// `Texture2D name` with `SamplerState sampler_name` at the same binding;
/// glslc's `-fauto-combined-image-sampler` merges them into the combined
/// image samplers the pipeline layout uses.
const HLSL_PREAMBLE: &str = r#"
#define MAX_LIGHTS 128
#define MAX_SHADOW_VIEWS 12
#define MAX_PROBE_VOLUMES 4
static const float CITRUS_PI = 3.14159265359;

struct Light { float4 pos_kind; float4 dir_range; float4 color; float4 spot; };
struct ProbeVolume { float4x4 world_to_local; float4 size_base; float4 counts; };
struct Probe { float4 c[6]; };
struct FrameData {
    float4x4 view;
    float4x4 proj;
    float4x4 view_proj;
    float4 camera_pos;
    float4 light_dir;
    float4 light_color;
    float4 ambient;
    float4 misc;
    float4 postfx0;
    float4 postfx1;
    float4 postfx2;
    float4 postfx3;
    float4 cascade_splits;
    Light lights[MAX_LIGHTS];
    float4x4 shadow_vp[MAX_SHADOW_VIEWS];
    ProbeVolume probe_volumes[MAX_PROBE_VOLUMES];
    float4 debug;
    float4x4 inv_view;
    float4x4 inv_proj;
    float4 ssr;
    float4 refl_center;
    float4 refl_extents;
    float4 fog_color;
    float4 fog_params;
    float4 refl_box;
    float4 refl2_center;
    float4 refl2_extents;
    float4 refl2_box;
    float4 refl_blend;
};
[[vk::binding(0, 0)]] cbuffer CitrusFrame { FrameData frame; };
[[vk::binding(1, 0)]] Texture2DArray u_shadow;
[[vk::binding(1, 0)]] SamplerComparisonState sampler_u_shadow;
[[vk::binding(2, 0)]] StructuredBuffer<Probe> probes;
[[vk::binding(3, 0)]] Texture2DArray u_lightmap;
[[vk::binding(3, 0)]] SamplerState sampler_u_lightmap;
[[vk::binding(4, 0)]] Texture2D u_screen_gi;
[[vk::binding(4, 0)]] SamplerState sampler_u_screen_gi;
[[vk::binding(7, 0)]] TextureCube u_env;
[[vk::binding(7, 0)]] SamplerState sampler_u_env;
[[vk::binding(9, 0)]] TextureCube u_env_b;
[[vk::binding(9, 0)]] SamplerState sampler_u_env_b;

[[vk::binding(0, 1)]] Texture2D t_albedo;
[[vk::binding(0, 1)]] SamplerState sampler_t_albedo;
[[vk::binding(1, 1)]] Texture2D t_normal;
[[vk::binding(1, 1)]] SamplerState sampler_t_normal;
[[vk::binding(2, 1)]] Texture2D t_orm;
[[vk::binding(2, 1)]] SamplerState sampler_t_orm;
[[vk::binding(3, 1)]] Texture2D t_emission;
[[vk::binding(3, 1)]] SamplerState sampler_t_emission;

struct CitrusPush { float4x4 model; float4 d0; float4 d1; float4 d2; float4 d3; };
[[vk::push_constant]] CitrusPush pc;
[[vk::constant_id(63)]] const int citrus_alpha_mode = 0;

struct CitrusInput {
    [[vk::location(0)]] float3 world_pos : TEXCOORD0;
    [[vk::location(1)]] float3 normal : TEXCOORD1;
    [[vk::location(2)]] float2 uv : TEXCOORD2;
    [[vk::location(3)]] float4 color : TEXCOORD3;
    [[vk::location(4)]] float4 tangent : TEXCOORD4;
    [[vk::location(5)]] float2 uv1 : TEXCOORD5;
    float4 frag_coord : SV_Position;
    bool front_facing : SV_IsFrontFace;
};
// The current fragment's inputs, for the helpers below.
static CitrusInput citrus_in;

#define CITRUS_SAMPLE(tex, uv) tex.Sample(sampler_##tex, uv)
#define u_time (frame.misc.x)
#define u_light_count (int(frame.misc.y + 0.5))
#define u_camera_pos (frame.camera_pos.xyz)
#define u_light_dir (frame.light_dir.xyz)
#define u_light_color (frame.light_color.rgb)
#define u_ambient (frame.ambient.rgb)

float3 citrus_sh(uint i, float3 n) {
    Probe p = probes[i];
    return p.c[0].rgb * 0.282095 + 0.325735 * (p.c[1].rgb * n.y + p.c[2].rgb * n.z + p.c[3].rgb * n.x);
}

float3 citrus_probe_irradiance(float3 world_pos, float3 n, out float coverage) {
    int vc = int(frame.misc.w + 0.5);
    for (int vi = 0; vi < vc && vi < MAX_PROBE_VOLUMES; ++vi) {
        ProbeVolume v = frame.probe_volumes[vi];
        float3 size = max(v.size_base.xyz, 1e-4);
        float3 local = mul(v.world_to_local, float4(world_pos, 1.0)).xyz;
        float3 t = (local + size * 0.5) / size;
        if (any(t < 0.0) || any(t > 1.0)) continue;
        int3 cnt = int3(v.counts.xyz + 0.5);
        float3 g = saturate(t) * float3(cnt - 1);
        int3 g0 = int3(floor(g));
        float3 f = g - float3(g0);
        f = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
        float3 sum = 0.0; float wsum = 0.0;
        for (int c = 0; c < 8; ++c) {
            int3 off = int3(c & 1, (c >> 1) & 1, (c >> 2) & 1);
            int3 gc = clamp(g0 + off, 0, cnt - 1);
            float3 tw = lerp(1.0 - f, f, float3(off));
            float w = tw.x * tw.y * tw.z;
            uint idx = uint(v.size_base.w) + uint(gc.x + gc.y * cnt.x + gc.z * cnt.x * cnt.y);
            sum += w * citrus_sh(idx, n); wsum += w;
        }
        float3 edge = min(t, 1.0 - t);
        float e = min(edge.x, min(edge.y, edge.z));
        coverage = (vi + 1 < vc) ? 1.0 : smoothstep(0.0, 0.2, e);
        return wsum > 1e-5 ? max(sum / wsum, 0.0) : 0.0;
    }
    coverage = 0.0;
    return 0.0;
}

float3 citrus_voxel_emitters(float3 world_pos, float3 n) {
    float3 sum = 0.0;
    int count = int(frame.misc.y + 0.5);
    for (int i = 0; i < count && i < MAX_LIGHTS; ++i) {
        Light l = frame.lights[i];
        if (int(l.pos_kind.w + 0.5) != 3) continue;
        float3 d = l.pos_kind.xyz - world_pos;
        float dist = length(d);
        float f = saturate(1.0 - dist / max(l.dir_range.w, 1e-3));
        float3 L = dist > 1e-4 ? d / dist : n;
        sum += l.color.rgb * (f * f * max(0.11141 + 0.25465 * dot(n, L), 0.0));
    }
    return sum;
}

float3 citrus_gi(float3 world_pos, float3 n) {
    float cov;
    float3 irr = citrus_probe_irradiance(world_pos, n, cov);
    return lerp(u_ambient, irr, cov) + citrus_voxel_emitters(world_pos, n);
}

float3 citrus_screen_gi() {
    float2 suv = citrus_in.frag_coord.xy / float2(max(frame.postfx2.w, 1.0), max(frame.postfx3.w, 1.0));
    return u_screen_gi.Sample(sampler_u_screen_gi, suv).rgb;
}

float3 citrus_baked_gi(float3 world_pos, float3 n) {
    float mode = frame.debug.z;
    if (pc.d3.w >= 0.0) {
        float layer = floor(pc.d3.w + 0.5);
        float3 gi = u_lightmap.Sample(sampler_u_lightmap, float3(citrus_in.uv1, layer)).rgb / CITRUS_PI;
        if (mode > -0.5) { float c; gi += citrus_probe_irradiance(world_pos, n, c); }
        if (mode > 1.5) gi += citrus_screen_gi();
        return gi;
    }
    if (mode > 0.5) {
        float3 gi = citrus_screen_gi();
        if (mode > 1.5) gi += citrus_gi(world_pos, n);
        return gi;
    }
    if (mode < -0.5) return u_ambient;
    return citrus_gi(world_pos, n);
}

bool citrus_light(int i, float3 world_pos, out float3 L, out float3 radiance) {
    Light l = frame.lights[i];
    L = float3(0.0, 1.0, 0.0);
    radiance = 0.0;
    if (l.spot.y < 0.0 && l.spot.w > 0.5 && pc.d3.w >= 0.0) return false;
    int kind = int(l.pos_kind.w + 0.5);
    // Kind 3 (FluxVoxel emitter) is indirect light, added by citrus_gi.
    if (kind == 3) return false;
    float atten = 1.0;
    if (kind == 0) {
        L = normalize(-l.dir_range.xyz);
    } else {
        float3 d = l.pos_kind.xyz - world_pos;
        float dist = length(d);
        if (dist < 1e-4) return false;
        L = d / dist;
        float range = max(l.dir_range.w, 1e-3);
        float fall = saturate(1.0 - dist / range);
        atten = fall * fall / (1.0 + dist * dist);
        if (kind == 2) {
            float cd = dot(normalize(l.dir_range.xyz), -L);
            atten *= smoothstep(l.color.w, l.spot.x, cd);
        }
    }
    radiance = l.color.rgb * atten;
    return atten > 0.0;
}

float citrus_shadow(int i, float3 world_pos, float3 n, float3 L) {
    Light light = frame.lights[i];
    int base = int(light.spot.y);
    if (base < 0) return 1.0;
    float bias = light.spot.z;
    bool soft = light.spot.w >= 0.0;
    int vcount = int(abs(light.spot.w) + 0.5);
    int kind = int(light.pos_kind.w + 0.5);
    int sub = 0;
    if (kind == 0 && vcount > 1) {
        float depth = -mul(frame.view, float4(world_pos, 1.0)).z;
        int start = vcount - 1;
        for (int c = 0; c < vcount && c < 4; ++c) {
            if (depth < frame.cascade_splits[c]) { start = c; break; }
        }
        float margin = frame.misc.z * 3.0;
        sub = vcount - 1;
        for (int c2 = start; c2 < vcount && c2 < 4; ++c2) {
            float4 cc = mul(frame.shadow_vp[base + c2], float4(world_pos, 1.0));
            if (cc.w <= 0.0) { sub = c2; continue; }
            float2 cuv = (cc.xy / cc.w) * 0.5 + 0.5;
            sub = c2;
            if (cuv.x >= margin && cuv.x <= 1.0 - margin && cuv.y >= margin && cuv.y <= 1.0 - margin) break;
        }
    } else if (vcount == 6) {
        float3 fl = world_pos - light.pos_kind.xyz;
        float3 a = abs(fl);
        if (a.x >= a.y && a.x >= a.z) sub = fl.x > 0.0 ? 0 : 1;
        else if (a.y >= a.z) sub = fl.y > 0.0 ? 2 : 3;
        else sub = fl.z > 0.0 ? 4 : 5;
    }
    int layer = base + sub;
    float4 lc = mul(frame.shadow_vp[layer], float4(world_pos, 1.0));
    if (lc.w <= 0.0) return 1.0;
    float3 proj = lc.xyz / lc.w;
    float2 uv = proj.xy * 0.5 + 0.5;
    float ndotl = saturate(dot(n, L));
    float slope = sqrt(max(1.0 - ndotl * ndotl, 0.0)) / max(ndotl, 0.2);
    float ref = proj.z - (bias + 0.0008) * (1.0 + slope);
    if (ref > 1.0) return 1.0;
    if (vcount != 6 && (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0)) return 1.0;
    if (!soft) return u_shadow.SampleCmpLevelZero(sampler_u_shadow, float3(saturate(uv), float(layer)), ref);
    float spacing = frame.misc.z;
    float sum = 0.0;
    for (int dx = -2; dx <= 2; ++dx) {
        for (int dy = -2; dy <= 2; ++dy) {
            float2 suv = saturate(uv + float2(float(dx), float(dy)) * spacing);
            sum += u_shadow.SampleCmpLevelZero(sampler_u_shadow, float3(suv, float(layer)), ref);
        }
    }
    return sum / 25.0;
}

float3 citrus_direct_diffuse(float3 world_pos, float3 n) {
    float3 sum = 0.0;
    for (int i = 0; i < u_light_count && i < MAX_LIGHTS; ++i) {
        float3 L, rad;
        if (citrus_light(i, world_pos, L, rad)) sum += rad * max(dot(n, L), 0.0);
    }
    return sum;
}

float3 citrus_direct_lambert(float3 world_pos, float3 n) {
    float3 sum = 0.0;
    for (int i = 0; i < u_light_count && i < MAX_LIGHTS; ++i) {
        float3 L, rad;
        if (!citrus_light(i, world_pos, L, rad)) continue;
        float ndl = max(dot(n, L), 0.0);
        if (ndl > 0.0) sum += rad * ndl * citrus_shadow(i, world_pos, n, L);
    }
    return sum;
}

float3 citrus_probe_dir(float3 world_pos, float3 R, float3 capture, float3 center, float3 extents, bool project) {
    if (!project) return R;
    float3 bmin = center - extents;
    float3 bmax = center + extents;
    float3 eps = extents * 0.02 + 0.02;
    if (!(all(world_pos >= bmin - eps) && all(world_pos <= bmax + eps))) return R;
    float3 invR = 1.0 / R;
    float3 tmax = max((bmax - world_pos) * invR, (bmin - world_pos) * invR);
    float dist = max(min(min(tmax.x, tmax.y), tmax.z), 0.0);
    return world_pos + R * dist - capture;
}
float citrus_probe_influence(float3 p, float3 center, float3 extents, float blend) {
    float3 d = extents - abs(p - center);
    float inner = min(d.x, min(d.y, d.z));
    if (blend <= 1e-4) return inner >= 0.0 ? 1.0 : 0.0;
    return saturate(inner / blend);
}
float3 citrus_env(float3 world_pos, float3 R, float roughness) {
    float intensity = frame.refl_center.w;
    if (intensity < 0.0) return u_ambient;
    float3 R0 = R;
    if (intensity > 0.0) {
        R0 = citrus_probe_dir(world_pos, R, frame.refl_center.xyz, frame.refl_box.xyz, frame.refl_extents.xyz, frame.refl_extents.w > 0.5);
    }
    float scale = intensity > 0.0 ? intensity : frame.ambient.w;
    uint w, h, levels;
    u_env.GetDimensions(0, w, h, levels);
    float3 env = u_env.SampleLevel(sampler_u_env, R0, roughness * float(levels - 1)).rgb * scale;
    // Second reflection probe, blended like the standard shader.
    if (frame.refl2_center.w > 0.0) {
        float3 R1 = citrus_probe_dir(world_pos, R, frame.refl2_center.xyz, frame.refl2_box.xyz, frame.refl2_extents.xyz, frame.refl2_extents.w > 0.5);
        uint w2, h2, levels2;
        u_env_b.GetDimensions(0, w2, h2, levels2);
        float3 env_b = u_env_b.SampleLevel(sampler_u_env_b, R1, roughness * float(levels2 - 1)).rgb * frame.refl2_center.w;
        float f0 = citrus_probe_influence(world_pos, frame.refl_box.xyz, frame.refl_extents.xyz, frame.refl_blend.x);
        float f1 = citrus_probe_influence(world_pos, frame.refl2_box.xyz, frame.refl2_extents.xyz, frame.refl_blend.y);
        env = lerp(env, env_b, frame.refl_blend.z > 0.5 ? f1 : f1 * (1.0 - f0));
    }
    return env;
}

float citrus_dither(float2 fragcoord) {
    int2 p = int2(fragcoord) & 3;
    static const float m[16] = { 0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0,
                                 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0 };
    return (m[p.x + p.y * 4] + 0.5) / 16.0;
}

float3 citrus_aces(float3 x) {
    return saturate((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14));
}

float3 citrus_postfx(float3 color, float2 fragcoord) {
    color *= exp2(frame.postfx0.y);
    if (frame.postfx1.w > 0.5) {
        color *= exp2(frame.postfx0.z);
        float temp = frame.postfx1.y, tint = frame.postfx1.z;
        color.r *= 1.0 + temp * 0.2 + tint * 0.1;
        color.g *= 1.0 - tint * 0.2;
        color.b *= 1.0 - temp * 0.2 + tint * 0.1;
        color = (color - 0.18) * frame.postfx0.w + 0.18;
        float l = dot(color, float3(0.2126, 0.7152, 0.0722));
        color = max(lerp(l, color, frame.postfx1.x), 0.0);
    }
    int mode = int(frame.postfx0.x + 0.5);
    if (mode == 1) color = color / (color + 1.0);
    else if (mode == 2) color = citrus_aces(color);
    if (frame.postfx2.x > 0.5) {
        float2 uv = fragcoord / float2(max(frame.postfx2.w, 1.0), max(frame.postfx3.w, 1.0));
        float dist = length(uv - 0.5) * 1.41421356;
        float sm = max(frame.postfx2.z, 1e-3);
        float mask = saturate((dist - (1.0 - sm)) / sm) * frame.postfx2.y;
        color = lerp(color, frame.postfx3.xyz, mask);
    }
    return color;
}
"#;

const HLSL_EPILOGUE: &str = r#"
struct CitrusOutput {
    [[vk::location(0)]] float4 color : SV_Target0;
    [[vk::location(1)]] float4 gbuf : SV_Target1;
};
CitrusOutput main(CitrusInput i) {
    citrus_in = i;
    CitrusOutput o;
    o.color = citrus_frag(i);
    o.gbuf = float4(0.0, 0.0, 0.0, 0.0);
    return o;
}
"#;

fn push_define(prop: &ShaderProp, offset: usize) -> String {
    let vec = offset / 4;
    let comp = offset % 4;
    let access = match prop.kind.size() {
        1 => format!("pc.d{vec}.{}", ["x", "y", "z", "w"][comp]),
        3 => format!("pc.d{vec}.{}", ["xyz", "yzw"][comp]),
        _ => format!("pc.d{vec}"),
    };
    format!("#define {} ({access})\n", prop.name)
}

/// HLSL `packoffset` for a byte offset inside the material cbuffer.
fn packoffset(offset: u32) -> String {
    let reg = offset / 16;
    match (offset % 16) / 4 {
        0 => format!("c{reg}"),
        c => format!("c{reg}.{}", ["x", "y", "z", "w"][c as usize]),
    }
}

/// Generated per-shader declarations (property defines, material block,
/// samplers, keyword constants). `frozen` bakes keywords as plain constants.
pub fn property_declarations(source: &ShaderSource, frozen: Option<&KeywordSet>) -> String {
    let hlsl = source.lang == ShaderLang::Hlsl;
    let mut out = String::new();
    for prop in &source.props {
        if let PropStorage::Push { offset } = prop.storage {
            out.push_str(&push_define(prop, offset));
        }
    }
    for prop in &source.props {
        if let ShaderPropKind::Enum(opts) = &prop.kind {
            for (i, o) in opts.iter().enumerate() {
                let _ = writeln!(out, "{}const int {}_{o} = {i};", if hlsl { "static " } else { "" }, prop.name);
            }
        }
    }
    for prop in &source.props {
        let PropStorage::Keyword { id } = prop.storage else {
            continue;
        };
        let (ty, value) = match prop.kind {
            ShaderPropKind::Toggle => {
                let on = frozen.map_or(prop.default[0] != 0.0, |k| k[id as usize] != 0);
                ("bool", if on { "true".to_owned() } else { "false".to_owned() })
            }
            _ => {
                let v = frozen.map_or(prop.default[0].round() as i32, |k| k[id as usize] as i32);
                ("int", v.to_string())
            }
        };
        let decl = match (hlsl, frozen.is_some()) {
            (false, false) => format!("layout(constant_id = {id}) const {ty} {} = {value};", prop.name),
            (false, true) => format!("const {ty} {} = {value};", prop.name),
            (true, false) => format!("[[vk::constant_id({id})]] const {ty} {} = {value};", prop.name),
            (true, true) => format!("static const {ty} {} = {value};", prop.name),
        };
        out.push_str(&decl);
        out.push('\n');
    }
    if source.ubo_size > 0 {
        if hlsl {
            out.push_str("[[vk::binding(0, 2)]] cbuffer CitrusMaterial {\n");
        } else {
            out.push_str("layout(set = 2, binding = 0, std140) uniform CitrusMaterial {\n");
        }
        for prop in &source.props {
            let PropStorage::Uniform { offset } = prop.storage else {
                continue;
            };
            let member = match prop.kind {
                ShaderPropKind::Toggle => format!("_cp_{}", prop.name),
                ShaderPropKind::Texture { .. } => format!("{}_st", prop.name),
                _ => prop.name.clone(),
            };
            let line = if hlsl {
                format!("    {} {member} : packoffset({});\n", prop.kind.hlsl_type(), packoffset(offset))
            } else {
                format!("    layout(offset = {offset}) {} {member};\n", prop.kind.glsl_type())
            };
            out.push_str(&line);
        }
        out.push_str("};\n");
        for prop in &source.props {
            if matches!(prop.storage, PropStorage::Uniform { .. }) && prop.kind == ShaderPropKind::Toggle {
                let _ = writeln!(out, "#define {0} (_cp_{0} > 0.5)", prop.name);
            }
        }
    }
    for prop in source.textures() {
        let ShaderPropKind::Texture { binding, .. } = prop.kind else {
            continue;
        };
        let n = &prop.name;
        if hlsl {
            let _ = writeln!(out, "[[vk::binding({binding}, 2)]] Texture2D {n};");
            let _ = writeln!(out, "[[vk::binding({binding}, 2)]] SamplerState sampler_{n};");
        } else {
            let _ = writeln!(out, "layout(set = 2, binding = {binding}) uniform sampler2D {n};");
        }
        let _ = writeln!(out, "#define {n}_uv(uv) ((uv) * {n}_st.xy + {n}_st.zw)");
    }
    out
}

/// Full translation unit fed to glslc.
pub fn assemble_shader(source: &ShaderSource, frozen: Option<&KeywordSet>) -> String {
    let decls = property_declarations(source, frozen);
    let mut text = String::with_capacity(GLSL_PREAMBLE.len() + source.body.len() + decls.len() + 256);
    match source.lang {
        ShaderLang::Glsl => {
            text.push_str(GLSL_PREAMBLE);
            text.push_str(&decls);
            text.push_str("#define main citrus_user_main\n#line 1\n");
            text.push_str(&source.body);
            text.push_str("\n#undef main\nvoid main() {\n    o_gbuf = vec4(0.0);\n    citrus_user_main();\n}\n");
        }
        ShaderLang::Hlsl => {
            text.push_str(HLSL_PREAMBLE);
            text.push_str(&decls);
            text.push_str("#line 1\n");
            text.push_str(&source.body);
            text.push_str(HLSL_EPILOGUE);
        }
    }
    text
}

/// Compile the specialization-constant module (every keyword combination is
/// a pipeline specialization of it). `label` names the shader in errors.
pub fn compile_shader(source: &ShaderSource, label: &str) -> Result<Vec<u8>> {
    compile_shader_variant(source, label, None)
}

/// Vulkan environment custom shaders compile for. The editor and desktop
/// builds use 1.3; the Android / Quest cook uses 1.1 (SPIR-V 1.3), since
/// standalone headsets may expose only Vulkan 1.1, which can't load newer
/// SPIR-V.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShaderTargetEnv {
    #[default]
    Vulkan1_3,
    Vulkan1_1,
}

impl ShaderTargetEnv {
    fn glslc_flag(self) -> &'static str {
        match self {
            Self::Vulkan1_3 => "--target-env=vulkan1.3",
            Self::Vulkan1_1 => "--target-env=vulkan1.1",
        }
    }
}

thread_local! {
    static SHADER_TARGET_ENV: std::cell::Cell<ShaderTargetEnv> = const { std::cell::Cell::new(ShaderTargetEnv::Vulkan1_3) };
}

/// The target environment shader compiles on this thread use.
pub fn shader_target_env() -> ShaderTargetEnv {
    SHADER_TARGET_ENV.with(|e| e.get())
}

/// Restores the previous [`ShaderTargetEnv`] when dropped.
#[must_use = "the target environment reverts when the guard drops"]
pub struct ShaderTargetEnvGuard {
    prev: ShaderTargetEnv,
}

impl Drop for ShaderTargetEnvGuard {
    fn drop(&mut self) {
        SHADER_TARGET_ENV.with(|e| e.set(self.prev));
    }
}

/// Compile shaders on the current thread for `env` until the guard drops
/// (a cook scope: other threads, e.g. the editor's, are unaffected).
pub fn scoped_shader_target_env(env: ShaderTargetEnv) -> ShaderTargetEnvGuard {
    ShaderTargetEnvGuard { prev: SHADER_TARGET_ENV.with(|e| e.replace(env)) }
}

/// Compile with keywords frozen to plain constants, so glslc's optimizer
/// strips every disabled feature from the SPIR-V itself (baked variants).
pub fn compile_shader_variant(source: &ShaderSource, label: &str, frozen: Option<&KeywordSet>) -> Result<Vec<u8>> {
    let text = assemble_shader(source, frozen);
    let mut cmd = Command::new("glslc");
    cmd.arg(shader_target_env().glslc_flag()).arg("-fshader-stage=fragment");
    if source.lang == ShaderLang::Hlsl {
        cmd.args(["-x", "hlsl", "-fauto-combined-image-sampler"]);
    }
    let mut child = cmd
        .args(["-O", "-", "-o", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running glslc (is shaderc installed?)")?;
    child
        .stdin
        .take()
        .context("glslc stdin")?
        .write_all(text.as_bytes())
        .context("feeding glslc")?;
    let output = child.wait_with_output().context("waiting for glslc")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).replace("<stdin>", label);
        bail!("{}", stderr.trim());
    }
    Ok(output.stdout)
}

/// Sidecar holding a shader's precompiled SPIR-V (`toon.frag` -> `toon.frag.spv`).
pub fn precompiled_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".spv");
    p.into()
}

/// Stable short hash of a keyword set (baked variant file names).
pub fn keyword_hash(keywords: &KeywordSet) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in keywords {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Baked variant sidecar: `toon.frag` -> `toon.frag.variants/<hash>.spv`.
pub fn variant_path(path: &Path, keywords: &KeywordSet) -> PathBuf {
    let mut dir = path.as_os_str().to_owned();
    dir.push(".variants");
    PathBuf::from(dir).join(format!("{:016x}.spv", keyword_hash(keywords)))
}

fn read_if_fresh(path: &Path, sidecar: &Path) -> Option<Vec<u8>> {
    let src_time = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let spv_time = std::fs::metadata(sidecar).and_then(|m| m.modified()).ok()?;
    if spv_time < src_time {
        return None;
    }
    std::fs::read(sidecar).ok().filter(|b| b.len() >= 4 && b.len() % 4 == 0)
}

fn read_source(path: &Path) -> Result<ShaderSource> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shader".into());
    parse_shader_source_lang(&text, &name, shader_lang_for(path))
        .with_context(|| format!("parsing {}", path.display()))
}

/// Compile a shader file and write its `.spv` sidecar next to it.
pub fn precompile_shader_file(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    let source = read_source(path)?;
    let spirv = compile_shader(&source, &path.display().to_string())?;
    let out = precompiled_path(path);
    std::fs::write(&out, spirv).with_context(|| format!("writing {}", out.display()))?;
    Ok(out)
}

/// Compile one keyword variant with its keywords frozen and write it to the
/// variant sidecar directory (game builds bake exactly the variants their
/// materials use).
pub fn bake_shader_variant(path: impl AsRef<Path>, keywords: &KeywordSet) -> Result<PathBuf> {
    let path = path.as_ref();
    let source = read_source(path)?;
    let spirv = compile_shader_variant(&source, &path.display().to_string(), Some(keywords))?;
    let out = variant_path(path, keywords);
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&out, spirv).with_context(|| format!("writing {}", out.display()))?;
    Ok(out)
}

/// A baked variant's SPIR-V, if present and not older than the source.
pub fn load_baked_variant(path: impl AsRef<Path>, keywords: &KeywordSet) -> Option<Vec<u8>> {
    let path = path.as_ref();
    read_if_fresh(path, &variant_path(path, keywords))
}

/// Read, parse, and compile a shader file (the precompiled sidecar is used
/// when present and fresh, so shipped games don't need glslc).
pub fn load_shader_file(path: impl AsRef<Path>) -> Result<(ShaderSource, Vec<u8>)> {
    let path = path.as_ref();
    let source = read_source(path)?;
    let spirv = match read_if_fresh(path, &precompiled_path(path)) {
        Some(spirv) => spirv,
        None => compile_shader(&source, &path.display().to_string())?,
    };
    Ok((source, spirv))
}

/// Starter shader written by Files > Create > New Shader.
pub const SHADER_TEMPLATE: &str = r#"// @shader "My Shader"
// @property tint "Tint" Color = (1, 0.6, 0.2, 1)
// @property glow "Glow" Range(0, 4) = 1
// @property speed "Speed" Range(0, 10) = 2
// @feature PULSE "Pulse"
// @property pulse_mask "Pulse Mask" Texture2D = white @linear
// @end

// Custom citrus shader (fragment stage). See docs/shaders.md. The engine provides:
//   textures   t_albedo, t_normal, t_orm, t_emission (material slots)
//   varyings   v_world_pos, v_normal, v_uv, v_uv1, v_color, v_tangent
//   uniforms   u_time, u_camera_pos, u_ambient, u_light_count, the `frame` UBO
//   lighting   citrus_light(i, pos, L, radiance), citrus_shadow(i, pos, n, L),
//              citrus_direct_lambert(pos, n), citrus_baked_gi(pos, n),
//              citrus_env(pos, R, roughness), citrus_postfx(color, fragXY)
//   output     o_color
// Property values are plain identifiers; feature / keyword toggles are
// specialization constants, so disabled branches cost nothing at runtime.
// Do not add a #version line.

void main() {
    vec4 albedo = texture(t_albedo, v_uv) * v_color * tint;
    vec3 n = normalize(gl_FrontFacing ? v_normal : -v_normal);
    vec3 color = albedo.rgb * (citrus_direct_lambert(v_world_pos, n) + citrus_baked_gi(v_world_pos, n));
    if (PULSE) {
        float pulse = 0.5 + 0.5 * sin(u_time * speed);
        color += tint.rgb * glow * pulse * texture(pulse_mask, pulse_mask_uv(v_uv)).r;
    }
    o_color = vec4(citrus_postfx(color, gl_FragCoord.xy), albedo.a);
}
"#;

/// Starter HLSL shader (same conventions; entry point `citrus_frag`).
pub const HLSL_SHADER_TEMPLATE: &str = r#"// @shader "My HLSL Shader"
// @property tint "Tint" Color = (1, 1, 1, 1)
// @property rim_power "Rim Power" Range(0.5, 8) = 3
// @feature RIM "Rim Light"
// @property rim_color "Rim Color" Color3(HDR) = (1, 1, 1)
// @end

// HLSL custom shader: implement citrus_frag(CitrusInput i) and return the
// colour. Textures pair `Texture2D x` with `SamplerState sampler_x`; use
// CITRUS_SAMPLE(x, uv). See docs/shaders.md.

float4 citrus_frag(CitrusInput i) {
    float4 albedo = CITRUS_SAMPLE(t_albedo, i.uv) * i.color * tint;
    float3 n = normalize(i.front_facing ? i.normal : -i.normal);
    float3 color = albedo.rgb * (citrus_direct_lambert(i.world_pos, n) + citrus_baked_gi(i.world_pos, n));
    if (RIM) {
        float3 v = normalize(u_camera_pos - i.world_pos);
        color += rim_color * pow(1.0 - saturate(dot(n, v)), rim_power);
    }
    return float4(citrus_postfx(color, i.frag_coord.xy), albedo.a);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn is_spirv(b: &[u8]) -> bool {
        b.len() > 4 && b[0..4] == [0x03, 0x02, 0x23, 0x07]
    }

    #[test]
    fn precompiled_sidecar_is_preferred_and_ignored_when_stale() {
        let dir = std::env::temp_dir().join(format!("citrus-spv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let frag = dir.join("tpl.frag");
        std::fs::write(&frag, SHADER_TEMPLATE).unwrap();
        let spv = precompile_shader_file(&frag).unwrap();
        let (_, compiled) = load_shader_file(&frag).unwrap();
        assert_eq!(compiled, std::fs::read(&spv).unwrap());
        std::fs::write(&spv, [0x03u8, 0x02, 0x23, 0x07]).unwrap();
        assert_eq!(load_shader_file(&frag).unwrap().1, vec![0x03, 0x02, 0x23, 0x07]);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&frag, SHADER_TEMPLATE).unwrap();
        assert!(load_shader_file(&frag).unwrap().1.len() > 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cook_scope_targets_vulkan_1_1() {
        // SPIR-V header word 1 is the version: 0x00010600 for Vulkan 1.3
        // (SPIR-V 1.6), 0x00010300 for Vulkan 1.1 (SPIR-V 1.3).
        let version = |b: &[u8]| u32::from_le_bytes(b[4..8].try_into().unwrap());
        let source = parse_shader_source(SHADER_TEMPLATE, "template").unwrap();
        assert_eq!(shader_target_env(), ShaderTargetEnv::Vulkan1_3);
        assert_eq!(version(&compile_shader(&source, "template").unwrap()), 0x0001_0600);
        {
            let _cook = scoped_shader_target_env(ShaderTargetEnv::Vulkan1_1);
            let spirv = compile_shader(&source, "template").unwrap();
            assert!(is_spirv(&spirv));
            assert_eq!(version(&spirv), 0x0001_0300);
            // Other threads keep the editor's target.
            std::thread::spawn(|| assert_eq!(shader_target_env(), ShaderTargetEnv::Vulkan1_3)).join().unwrap();
            let hlsl = parse_shader_source_lang(HLSL_SHADER_TEMPLATE, "hlsl", ShaderLang::Hlsl).unwrap();
            assert_eq!(version(&compile_shader(&hlsl, "hlsl").unwrap()), 0x0001_0300);
        }
        assert_eq!(shader_target_env(), ShaderTargetEnv::Vulkan1_3, "guard restores");
    }

    #[test]
    fn v1_toon_example_still_compiles() {
        let body = include_str!("../../../shaders/toon.frag");
        let source = parse_shader_source(body, "toon").unwrap();
        assert_eq!(source.display_name, "Toon (Poiyomi-lite)");
        assert!(source.props.iter().all(|p| matches!(p.storage, PropStorage::Push { .. })));
        assert!(is_spirv(&compile_shader(&source, "toon").unwrap()));
    }

    #[test]
    fn templates_parse_and_compile() {
        let source = parse_shader_source(SHADER_TEMPLATE, "template").unwrap();
        assert_eq!(source.display_name, "My Shader");
        assert_eq!(source.keyword_count(), 1);
        assert!(is_spirv(&compile_shader(&source, "template").unwrap()));

        let source = parse_shader_source_lang(HLSL_SHADER_TEMPLATE, "h", ShaderLang::Hlsl).unwrap();
        let spirv = compile_shader(&source, "hlsl template").unwrap();
        assert!(is_spirv(&spirv));
        let frozen = compile_shader_variant(&source, "hlsl template", Some(&[1; MAX_SHADER_KEYWORDS])).unwrap();
        assert!(is_spirv(&frozen));
    }

    pub(crate) fn spirv_words(b: &[u8]) -> Vec<u32> {
        b.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
    }

    /// (opcode, operands) for every instruction after the header.
    pub(crate) fn spirv_instructions(b: &[u8]) -> Vec<(u16, Vec<u32>)> {
        let w = spirv_words(b);
        let mut out = Vec::new();
        let mut i = 5;
        while i < w.len() {
            let count = (w[i] >> 16) as usize;
            out.push(((w[i] & 0xffff) as u16, w[i + 1..i + count.max(1)].to_vec()));
            i += count.max(1);
        }
        out
    }

    /// Member offsets of the block bound at (set, binding).
    fn block_offsets(spirv: &[u8], set: u32, binding: u32) -> Vec<u32> {
        let ins = spirv_instructions(spirv);
        let deco = |id: u32, kind: u32, v: u32| ins.iter().any(|(op, a)| *op == 71 && a[0] == id && a[1] == kind && a[2] == v);
        let var = ins
            .iter()
            .find(|(op, a)| *op == 59 && deco(a[1], 34, set) && deco(a[1], 33, binding))
            .expect("block variable");
        let ptr = var.1[0];
        let ty = ins.iter().find(|(op, a)| *op == 32 && a[0] == ptr).expect("pointer").1[2];
        let mut offs: Vec<(u32, u32)> = ins
            .iter()
            .filter(|(op, a)| *op == 72 && a[0] == ty && a[2] == 35)
            .map(|(_, a)| (a[1], a[3]))
            .collect();
        offs.sort();
        offs.into_iter().map(|(_, o)| o).collect()
    }

    #[test]
    fn material_block_offsets_match_std140_in_glsl_and_hlsl() {
        let decls = "// @property a Float = 1\n// @property b Color3 = (1, 1, 1)\n// @property c Float = 2\n\
                     // @property d Vector2 = (1, 2)\n// @property e Toggle = 1\n// @property f Enum(X, Y) = Y\n\
                     // @property g Color = (1, 1, 1, 1)\n// @property h Texture2D = white\n";
        let glsl = format!(
            "{decls}void main() {{ o_color = vec4(a + c + float(f) + (e ? 1.0 : 0.0), d, 1.0) * g + vec4(b, 0.0) + texture(h, h_uv(v_uv)); }}"
        );
        let hlsl = format!(
            "{decls}float4 citrus_frag(CitrusInput i) {{ return float4(a + c + float(f) + (e ? 1.0 : 0.0), d, 1.0) * g + float4(b, 0.0) + CITRUS_SAMPLE(h, h_uv(i.uv)); }}"
        );
        let expected: Vec<u32> = {
            let s = parse_shader_source(&glsl, "x").unwrap();
            s.props
                .iter()
                .filter_map(|p| match p.storage {
                    PropStorage::Uniform { offset } => Some(offset),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(expected, vec![0, 16, 28, 32, 40, 44, 48, 64]);
        for (text, lang) in [(glsl, ShaderLang::Glsl), (hlsl, ShaderLang::Hlsl)] {
            let s = parse_shader_source_lang(&text, "x", lang).unwrap();
            let spirv = compile_shader(&s, "x").unwrap();
            assert_eq!(block_offsets(&spirv, 2, 0), expected, "{lang:?}");
        }
    }

    /// Citrus Toon's presets select keyword variants: the frozen Simple variant
    /// must not contain the heavy features' code (texture samples, samplers),
    /// while the specialization module keeps every feature behind SpecIds.
    #[test]
    fn citrus_toon_variants_strip_disabled_features() {
        let body = include_str!("../../../shaders/citrus_toon.frag");
        let s = parse_shader_source(body, "citrus_toon").unwrap();
        assert_eq!(s.display_name, "Citrus Toon");
        let preset = |name: &str| {
            let i = s.presets.iter().position(|p| p.name == name).unwrap();
            let mut v = s.defaults();
            s.apply_preset(i, &mut v);
            s.keywords(&v)
        };
        let (simple, medium, heavy) = (preset("Simple"), preset("Medium"), preset("Heavy"));
        assert_eq!(s.keywords(&s.defaults()), simple, "defaults are the Simple preset");
        assert!(simple != medium && medium != heavy, "presets select distinct pipeline keys");

        let samples = |spirv: &[u8]| {
            spirv_instructions(spirv).iter().filter(|(op, _)| (87..=94).contains(op)).count()
        };
        let set2_bindings = |spirv: &[u8]| {
            let ins = spirv_instructions(spirv);
            let in_set2: Vec<u32> =
                ins.iter().filter(|(op, a)| *op == 71 && a[1] == 34 && a[2] == 2).map(|(_, a)| a[0]).collect();
            let mut b: Vec<u32> = ins
                .iter()
                .filter(|(op, a)| *op == 71 && a[1] == 33 && in_set2.contains(&a[0]))
                .map(|(_, a)| a[2])
                .collect();
            b.sort();
            b
        };
        let binding = |name: &str| match s.prop(name).unwrap().kind {
            ShaderPropKind::Texture { binding, .. } => binding,
            _ => unreachable!(),
        };
        let compile = |kw: &KeywordSet| compile_shader_variant(&s, "citrus_toon", Some(kw)).unwrap();
        let (vs, vm, vh) = (compile(&simple), compile(&medium), compile(&heavy));
        let (ss, sm, sh) = (samples(&vs), samples(&vm), samples(&vh));
        assert!(ss < sm && sm < sh, "texture samples: simple {ss}, medium {sm}, heavy {sh}");
        eprintln!(
            "citrus_toon variants: simple {} B / {ss} samples, medium {} B / {sm}, heavy {} B / {sh}",
            vs.len(),
            vm.len(),
            vh.len()
        );
        assert!(vs.len() * 3 < vh.len() * 2, "simple {} bytes vs heavy {} bytes", vs.len(), vh.len());
        let simple_b = set2_bindings(&vs);
        let heavy_b = set2_bindings(&vh);
        for name in ["dissolve_noise", "glitter_mask", "decal_tex", "detail_albedo", "height_tex", "matcap2_tex", "aniso_shift"] {
            assert!(!simple_b.contains(&binding(name)), "simple variant still binds {name}");
            assert!(heavy_b.contains(&binding(name)), "heavy variant lacks {name}");
        }

        let module = compile_shader(&s, "citrus_toon").unwrap();
        let spec_ids = spirv_instructions(&module).iter().filter(|(op, a)| *op == 71 && a[1] == 1).count();
        // One SpecId per keyword used in code plus the alpha mode; OUTLINE is
        // drawn by the engine and never read by the shader, so it folds away.
        assert_eq!(spec_ids, s.keyword_count(), "SpecIds in the specialization module");
        assert!(s.ubo_size as usize <= SHADER_UBO_MAX_BYTES);
    }

    #[test]
    fn baked_variant_round_trip() {
        let dir = std::env::temp_dir().join(format!("citrus-variant-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let frag = dir.join("tpl.frag");
        std::fs::write(&frag, SHADER_TEMPLATE).unwrap();
        let mut kw = [0u8; MAX_SHADER_KEYWORDS];
        assert!(load_baked_variant(&frag, &kw).is_none());
        bake_shader_variant(&frag, &kw).unwrap();
        assert!(load_baked_variant(&frag, &kw).is_some_and(|b| is_spirv(&b)));
        kw[0] = 1;
        assert!(load_baked_variant(&frag, &kw).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
