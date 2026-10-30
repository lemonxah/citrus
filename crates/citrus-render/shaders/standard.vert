#version 450
// citrus standard shader, vertex stage (phase 1)

layout(location = 0) in vec3 a_position;
layout(location = 1) in vec3 a_normal;
layout(location = 2) in vec2 a_uv;
layout(location = 3) in vec4 a_color;
layout(location = 4) in vec4 a_tangent; // xyz tangent, w handedness
layout(location = 5) in vec2 a_uv1;     // lightmap UVs

// Water surfaces displace their grid with the Gerstner waves here.
layout(constant_id = 4) const bool FEAT_WATER = false;

#include "standard_common.glsl"

// Water: the interactive ripple field (r height, gb gradient), material slot
// 15, with mips for reading it band-limited.
layout(set = 1, binding = 16) uniform sampler2D t_water_ripple;

// Per-instance model matrix (instance-rate vertex stream), so draws of one
// mesh + material batch into a single instanced draw (pc.model is unused).
layout(location = 8) in vec4 a_model0;
layout(location = 9) in vec4 a_model1;
layout(location = 10) in vec4 a_model2;
layout(location = 11) in vec4 a_model3;

layout(push_constant) uniform Push {
    mat4 model;
    vec4 base_color;
    vec4 emission; // rgb * intensity
    vec4 params0;  // metallic, roughness, toon_steps, pbr_toon_blend
    vec4 params1;  // alpha_cutoff, normal_strength, occlusion_strength, _
} pc;

layout(location = 0) out vec3 v_world_pos;
layout(location = 1) out vec3 v_normal;
layout(location = 2) out vec2 v_uv;
layout(location = 3) out vec4 v_color;
layout(location = 4) out vec4 v_tangent;
layout(location = 5) out vec2 v_uv1;
// Bit-identical positions in the depth prepass and the main pass (the main
// pass depth-tests EQUAL against the prepass).
invariant gl_Position;

void main() {
    mat4 model = mat4(a_model0, a_model1, a_model2, a_model3);
    vec3 position = a_position;
    vec3 normal = a_normal;
    vec4 tangent = a_tangent;
    vec2 uv1 = a_uv1;
    vec4 color = a_color;
    vec2 water_rest = vec2(0.0);
    if (FEAT_WATER) {
        // Rest point in the water's local space (an ocean grid is drawn
        // shifted to follow the camera, so it comes through the model
        // matrix), displaced by the waves; they calm toward a river's banks
        // (a_color.b = edge). a_color.rg carries the local flow (m/s / 10 +
        // 0.5), a_uv1.x the grid's spacing here: the displacement holds no
        // detail finer than it (the pixel shader adds that to the normal).
        int slot = clamp(int(fx.water.x + 0.5), 0, MAX_WATER_BODIES - 1);
        water_rest = (frame.water_bodies[slot].to_local * (model * vec4(a_position, 1.0))).xz;
        float spacing = max(a_uv1.x, 1e-3);
        vec3 d;
        float crest;
        float lost;
        water_waves(slot, water_rest, frame.water.x, spacing, d, normal, crest, lost);
        vec2 ruv = water_ripple_uv(slot, water_rest);
        float rfade = water_ripple_fade(ruv);
        if (rfade > 0.0) {
            float lod = water_ripple_lod(spacing, frame.water_bodies[slot].ripple.z, float(textureSize(t_water_ripple, 0).x));
            d.y += textureLod(t_water_ripple, ruv, lod).r * rfade;
        }
        float calm = 1.0 - a_color.b * a_color.b;
        d.y *= calm;
        normal = normalize(mix(vec3(0.0, 1.0, 0.0), normal, calm));
        position = a_position + d;
        tangent = vec4(normalize(vec3(normal.y, -normal.x, 0.0)), 1.0);
        uv1 = vec2(d.y, spacing);
        vec3 flow = mat3(model) * vec3(a_color.r - 0.5, 0.0, a_color.g - 0.5) * 10.0;
        color = vec4(flow.xz, a_color.b, 1.0);
    }
    vec4 world = model * vec4(position, 1.0);
    v_world_pos = world.xyz;
    // Normals need the inverse-transpose of the model's 3x3 so they stay correct
    // under non-uniform scale (plain mat3(model) skews them); it equals
    // mat3(model) for uniform scale/rotation. Tangents transform like directions,
    // so they use the plain basis.
    mat3 model3 = mat3(model);
    mat3 normal_mat = transpose(inverse(model3));
    v_normal = normalize(normal_mat * normal);
    v_tangent = vec4(normalize(model3 * tangent.xyz), tangent.w);
    // Water: the rest point (water-local XZ) the pixel shader evaluates its
    // normal, ripples and foam at.
    v_uv = FEAT_WATER ? water_rest : a_uv;
    v_uv1 = uv1;
    v_color = color;
    gl_Position = frame.view_proj * world;
}
