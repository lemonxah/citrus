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
    vec4 postfx0; // x tonemap mode, y exposure EV, z grade exposure, w contrast
    vec4 postfx1; // x saturation, y temperature, z tint, w grading enabled
    vec4 postfx2; // x vignette enabled, y intensity, z smoothness, w screen width
    vec4 postfx3; // xyz vignette color, w screen height
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

vec3 tonemap_aces(vec3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), 0.0, 1.0);
}
vec3 apply_postfx(vec3 color, vec2 fragcoord) {
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
    o_color = vec4(apply_postfx(color, gl_FragCoord.xy), 1.0);
}
