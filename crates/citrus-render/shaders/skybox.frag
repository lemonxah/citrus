#version 450
// Skybox fragment stage. params0.x selects the source:
//   > 0.5  -> sample the equirectangular texture in slot 0
//   else   -> procedural horizon/zenith gradient

layout(set = 0, binding = 0) uniform FrameData {
    mat4 view;
    mat4 proj;
    mat4 view_proj;
    vec4 camera_pos;
    vec4 light_dir;
    vec4 light_color;
    vec4 ambient;
    vec4 misc;
} frame;

layout(set = 1, binding = 0) uniform sampler2D t_sky;

layout(push_constant) uniform Push {
    mat4 model;
    vec4 base_color;
    vec4 emission;
    vec4 params0; // x = has texture
    vec4 params1;
} pc;

layout(location = 0) in vec3 v_dir;
layout(location = 0) out vec4 o_color;

const float PI = 3.14159265359;

void main() {
    vec3 dir = normalize(v_dir);
    vec3 color;
    if (pc.params0.x > 0.5) {
        float u = atan(dir.z, dir.x) / (2.0 * PI) + 0.5;
        float v = acos(clamp(dir.y, -1.0, 1.0)) / PI;
        color = texture(t_sky, vec2(u, v)).rgb;
    } else {
        // Simple sky: brighter horizon, deep-blue zenith, dark ground.
        vec3 horizon = vec3(0.52, 0.60, 0.72);
        vec3 zenith = vec3(0.10, 0.16, 0.34);
        vec3 ground = vec3(0.06, 0.06, 0.08);
        if (dir.y >= 0.0) {
            color = mix(horizon, zenith, clamp(dir.y, 0.0, 1.0));
        } else {
            color = mix(horizon, ground, clamp(-dir.y * 2.0, 0.0, 1.0));
        }
    }
    o_color = vec4(color, 1.0);
}
