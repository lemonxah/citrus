#version 450
// citrus standard shader — vertex stage (phase 1)

layout(location = 0) in vec3 a_position;
layout(location = 1) in vec3 a_normal;
layout(location = 2) in vec2 a_uv;
layout(location = 3) in vec4 a_color;
layout(location = 4) in vec4 a_tangent; // xyz tangent, w handedness
layout(location = 5) in vec2 a_uv1;     // lightmap UVs

layout(set = 0, binding = 0) uniform FrameData {
    mat4 view;
    mat4 proj;
    mat4 view_proj;
    vec4 camera_pos;   // xyz
    vec4 light_dir;    // xyz, normalized, pointing FROM the light
    vec4 light_color;  // rgb * intensity
    vec4 ambient;      // rgb
    vec4 misc;         // x = time in seconds
} frame;

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

void main() {
    vec4 world = pc.model * vec4(a_position, 1.0);
    v_world_pos = world.xyz;
    // TODO: inverse-transpose for non-uniform scale
    mat3 normal_mat = mat3(pc.model);
    v_normal = normalize(normal_mat * a_normal);
    v_tangent = vec4(normalize(normal_mat * a_tangent.xyz), a_tangent.w);
    v_uv = a_uv;
    v_uv1 = a_uv1;
    v_color = a_color;
    gl_Position = frame.view_proj * world;
}
