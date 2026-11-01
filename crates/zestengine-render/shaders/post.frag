#version 450
// Post-processing fullscreen pass: samples the linear-HDR scene-color target and
// applies the camera's blended Volume profile. Chromatic aberration and bloom
// (which need to read the rendered frame), then exposure, color grading,
// tonemap, and vignette. Writes display-space color to the swapchain.

layout(set = 0, binding = 0) uniform sampler2D u_hdr;
layout(set = 1, binding = 0) uniform sampler2D u_bloom;

layout(push_constant) uniform Post {
    vec4 p0; // x tonemap mode, y exposure EV, z grade exposure, w contrast
    vec4 p1; // x saturation, y temperature, z tint, w grading enabled
    vec4 p2; // x vignette enabled, y intensity, z smoothness, w (unused)
    vec4 p3; // xyz vignette color, w (unused)
    vec4 p4; // x bloom enabled, y threshold, z intensity, w radius
    vec4 p5; // x CA enabled, y CA intensity, z underwater wobble (0 = off), w time
    vec4 p6; // xyz waterline: near-plane height above the surface = x*u + y*v + z, w underwater blur (px at 1080p)
} pc;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o_color;

vec3 aces(vec3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), 0.0, 1.0);
}

void main() {
    vec2 uv = v_uv;

    // Under water: a slow wobble and a soft blur, only below the waterline.
    float under = 0.0;
    if (pc.p5.z > 0.0 || pc.p6.w > 0.0) {
        float above = pc.p6.x * uv.x + pc.p6.y * uv.y + pc.p6.z;
        under = 1.0 - smoothstep(-0.0015, 0.0015, above);
        float t = pc.p5.w;
        vec2 w = vec2(sin(uv.y * 21.0 + t * 1.7) + 0.5 * sin(uv.y * 47.0 - t * 2.3 + uv.x * 9.0),
                      cos(uv.x * 17.0 + t * 1.3) + 0.5 * cos(uv.x * 39.0 + t * 2.1 - uv.y * 7.0));
        uv = clamp(uv + w * (0.0016 * pc.p5.z * under), vec2(0.0), vec2(1.0));
    }

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
    if (under > 0.0 && pc.p6.w > 0.0) {
        // Golden-angle disk of taps, radius scaled to the target height.
        vec2 px = pc.p6.w * under / vec2(textureSize(u_hdr, 0).y) * vec2(float(textureSize(u_hdr, 0).y) / float(textureSize(u_hdr, 0).x), 1.0) * (float(textureSize(u_hdr, 0).y) / 1080.0);
        vec3 sum = color;
        for (int k = 1; k < 12; ++k) {
            float r = sqrt(float(k) / 11.0);
            float a = float(k) * 2.39996;
            sum += texture(u_hdr, clamp(uv + vec2(cos(a), sin(a)) * r * px, vec2(0.0), vec2(1.0))).rgb;
        }
        color = sum / 12.0;
    }

    // Exposure (EV), before bloom (UE order: exposure -> bloom -> tonemap); the
    // bloom prefilter applies the same exposure before its threshold.
    float ev = exp2(pc.p0.y);
    color *= ev;

    // Bloom: level 0 of the mip-chain bloom (bloom.comp), already exposed and
    // thresholded; the chain spreads it wide with no ring or tap artifacts.
    if (pc.p4.x > 0.5) {
        color += texture(u_bloom, uv).rgb * pc.p4.z;
    }

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
