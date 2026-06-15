#version 450
// Post-processing fullscreen pass: samples the linear-HDR scene-color target and
// applies the camera's blended Volume profile. Chromatic aberration and bloom
// (which need to read the rendered frame), then exposure, color grading,
// tonemap, and vignette. Writes display-space color to the swapchain.

layout(set = 0, binding = 0) uniform sampler2D u_hdr;

layout(push_constant) uniform Post {
    vec4 p0; // x tonemap mode, y exposure EV, z grade exposure, w contrast
    vec4 p1; // x saturation, y temperature, z tint, w grading enabled
    vec4 p2; // x vignette enabled, y intensity, z smoothness, w (unused)
    vec4 p3; // xyz vignette color, w (unused)
    vec4 p4; // x bloom enabled, y threshold, z intensity, w radius
    vec4 p5; // x CA enabled, y CA intensity
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o_color;

vec3 aces(vec3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), 0.0, 1.0);
}

void main() {
    vec2 uv = v_uv;

    // Chromatic aberration: split the channels along the screen radius.
    vec3 color;
    if (pc.p5.x > 0.5) {
        vec2 dir = (uv - 0.5) * (pc.p5.y * 0.02);
        color.r = texture(u_hdr, uv + dir).r;
        color.g = texture(u_hdr, uv).g;
        color.b = texture(u_hdr, uv - dir).b;
    } else {
        color = texture(u_hdr, uv).rgb;
    }

    // Cheap single-pass bloom: blur the over-threshold part in a ring around the
    // pixel and add it back. A mip-chain bloom is the higher-quality follow-up.
    if (pc.p4.x > 0.5) {
        float thr = pc.p4.y;
        float r = max(pc.p4.w, 0.0) * 0.04;
        vec3 b = max(texture(u_hdr, uv).rgb - vec3(thr), vec3(0.0));
        float wsum = 1.0;
        for (int i = 0; i < 16; ++i) {
            float a = float(i) / 16.0 * 6.2831853;
            float rr = (0.5 + 0.5 * float(i & 1)) * r; // two rings
            vec2 o = vec2(cos(a), sin(a)) * rr;
            b += max(texture(u_hdr, uv + o).rgb - vec3(thr), vec3(0.0));
            wsum += 1.0;
        }
        color += (b / wsum) * pc.p4.z;
    }

    // Exposure (EV).
    color *= exp2(pc.p0.y);

    // Color grading (linear).
    if (pc.p1.w > 0.5) {
        color *= exp2(pc.p0.z);                       // post exposure
        float temp = pc.p1.y, tint = pc.p1.z;         // white balance
        color.r *= 1.0 + temp * 0.2 + tint * 0.1;
        color.g *= 1.0 - tint * 0.2;
        color.b *= 1.0 - temp * 0.2 + tint * 0.1;
        color = (color - 0.18) * pc.p0.w + 0.18;      // contrast @ mid-grey
        float l = dot(color, vec3(0.2126, 0.7152, 0.0722));
        color = mix(vec3(l), color, pc.p1.x);         // saturation
        color = max(color, vec3(0.0));
    }

    // Tonemap.
    int mode = int(pc.p0.x + 0.5);
    if (mode == 1) {
        color = color / (color + vec3(1.0));   // Reinhard
    } else if (mode == 2) {
        color = aces(color);                   // ACES
    }                                          // 0 = none

    // Vignette (display space).
    if (pc.p2.x > 0.5) {
        float dist = length(uv - 0.5) * 1.41421356;
        float sm = max(pc.p2.z, 1e-3);
        float mask = clamp((dist - (1.0 - sm)) / sm, 0.0, 1.0) * pc.p2.y;
        color = mix(color, pc.p3.xyz, mask);
    }

    o_color = vec4(color, 1.0);
}
