#version 450
// Deferred screen-space reflections, resolve pass.
//
// Runs fullscreen AFTER the forward pass, so it marches against the CURRENT
// frame's lit colour (no 1-frame lag) using the depth prepass + the forward
// pass's reflectance/roughness G-buffer. The forward pass already added the
// environment-cube reflection (reflectance * env), correctly fogged; this pass
// re-derives that same env radiance and swaps in the screen-space hit:
//   out = scene + reflectance * conf * (ssr_radiance - env_radiance)
// so a confident SSR hit exactly replaces the env reflection and a miss leaves
// the forward result untouched. Normals are reconstructed from depth (geometric)
// which matches the shading normal on smooth reflectors (mirrors, metal).

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o_color;

layout(set = 0, binding = 0) uniform sampler2D u_color; // current lit HDR colour
layout(set = 0, binding = 1) uniform sampler2D u_gbuf;  // reflectance.rgb, roughness.a
layout(set = 0, binding = 2) uniform sampler2D u_depth; // scene depth prepass
layout(set = 0, binding = 3) uniform samplerCube u_env; // prefiltered environment

layout(set = 0, binding = 4) uniform SsrData {
    mat4 proj;
    mat4 inv_proj;
    mat4 view;
    mat4 inv_view;
    vec4 camera_pos;   // xyz world camera
    vec4 ssr;          // x enabled, y intensity, z max view dist, w roughness cutoff
    vec4 refl_center;  // xyz probe box centre, w probe intensity (0 = none)
    vec4 refl_extents; // xyz half-extents, w box-projection enabled
    vec4 fog_color;    // xyz colour, w density (0 = no fog)
    vec4 fog_params;   // x height falloff, y height ref, z start distance, w _
    vec4 fog_light;    // xyz sun colour*intensity, w anisotropy g
    vec4 fog_sun;      // xyz dir to sun, w time
    vec4 screen;       // x width, y height, z _, w _
} u;

const float ENV_MAX_LOD = 6.0; // matches cube_mip_count(64) - 1 / standard.frag

// The scene renders with a Y-flipped (negative-height) viewport, so a sampling
// UV maps to NDC with a flipped Y. This constant matches the forward pass.
const bool FLIP_Y = true;

// View-space Z of the scene surface at a sampling UV.
float scene_z(vec2 uv) {
    float d = texture(u_depth, uv).r;
    vec2 ndc = uv * 2.0 - 1.0;
    if (FLIP_Y) ndc.y = -ndc.y;
    vec4 vp = u.inv_proj * vec4(ndc, d, 1.0);
    return vp.z / vp.w;
}

// Full view-space position of the scene surface at a sampling UV + sampled depth.
vec3 view_pos(vec2 uv, float d) {
    vec2 ndc = uv * 2.0 - 1.0;
    if (FLIP_Y) ndc.y = -ndc.y;
    vec4 vp = u.inv_proj * vec4(ndc, d, 1.0);
    return vp.xyz / vp.w;
}

// Project a view-space point to the sampling UV (inverse of view_pos's mapping).
vec2 project(vec3 p) {
    vec4 c = u.proj * vec4(p, 1.0);
    vec2 uv = (c.xy / c.w) * 0.5 + 0.5;
    if (FLIP_Y) uv.y = 1.0 - uv.y;
    return uv;
}

// Octahedral decode of the view-space normal the forward pass stored (rg).
vec3 oct_decode(vec2 e) {
    e = e * 2.0 - 1.0;
    vec3 n = vec3(e.x, e.y, 1.0 - abs(e.x) - abs(e.y));
    float t = max(-n.z, 0.0);
    n.x += n.x >= 0.0 ? -t : t;
    n.y += n.y >= 0.0 ? -t : t;
    return normalize(n);
}

// --- Volumetric fog: a real raymarched participating medium (animated, patchy,
// with sun in-scattering), so fog hangs + drifts in the air instead of just
// tinting surface colour. ---
const float PI = 3.14159265359;

float hash13(vec3 p) {
    p = fract(p * 0.1031);
    p += dot(p, p.yzx + 33.33);
    return fract((p.x + p.y) * p.z);
}

float vnoise(vec3 p) {
    vec3 i = floor(p);
    vec3 f = fract(p);
    f = f * f * (3.0 - 2.0 * f);
    return mix(
        mix(mix(hash13(i + vec3(0, 0, 0)), hash13(i + vec3(1, 0, 0)), f.x),
            mix(hash13(i + vec3(0, 1, 0)), hash13(i + vec3(1, 1, 0)), f.x), f.y),
        mix(mix(hash13(i + vec3(0, 0, 1)), hash13(i + vec3(1, 0, 1)), f.x),
            mix(hash13(i + vec3(0, 1, 1)), hash13(i + vec3(1, 1, 1)), f.x), f.y),
        f.z);
}

float fbm(vec3 p) {
    float a = 0.5, s = 0.0;
    for (int i = 0; i < 3; ++i) {
        s += a * vnoise(p);
        p *= 2.02;
        a *= 0.5;
    }
    return s;
}

// Henyey-Greenstein phase (forward-scatter sun glow when looking toward the sun).
float hg_phase(float cosT, float g) {
    float g2 = g * g;
    return (1.0 - g2) / (4.0 * PI * pow(max(1.0 + g2 - 2.0 * g * cosT, 1e-4), 1.5));
}

// March from the camera to `endWorld`, accumulating animated height-fog density +
// sun in-scattering. Returns the fogged colour.
vec3 apply_fog(vec3 color, vec3 endWorld) {
    vec3 ro = u.camera_pos.xyz;
    vec3 seg = endWorld - ro;
    float dist = length(seg);
    if (dist < 1e-3) return color;
    vec3 rd = seg / dist;
    const int STEPS = 48;
    float stepLen = dist / float(STEPS);
    vec3 wind = vec3(0.06, 0.0, 0.04) * u.fog_sun.w; // drift over time
    float phase = hg_phase(dot(rd, normalize(u.fog_sun.xyz)), u.fog_light.w);
    float start = u.fog_params.z;
    float transmittance = 1.0;
    vec3 inscatter = vec3(0.0);
    for (int i = 0; i < STEPS; ++i) {
        float d = stepLen * (float(i) + 0.5);
        if (d < start) continue;
        vec3 p = ro + rd * d;
        float height = exp(-u.fog_params.x * max(0.0, p.y - u.fog_params.y));
        float n = fbm(p * 0.08 + wind); // patchy, drifting density
        float dens = u.fog_color.w * height * clamp(0.25 + 1.5 * n, 0.0, 2.0) * stepLen;
        if (dens <= 0.0) continue;
        vec3 lit = u.fog_color.rgb + u.fog_light.rgb * phase;
        float a = 1.0 - exp(-dens);
        inscatter += transmittance * a * lit;
        transmittance *= exp(-dens);
        if (transmittance < 0.01) break;
    }
    return color * transmittance + inscatter;
}

// March the reflected view ray against the depth prepass; sample the CURRENT lit
// colour at the hit. Returns rgb radiance + a = hit confidence (0 = miss).
vec4 trace(vec3 P, vec3 N) {
    vec2 screen = u.screen.xy;
    vec3 dir = reflect(normalize(P), N);
    float maxDist = max(u.ssr.z, 1.0);

    vec3 endP = P + dir * maxDist;
    if (endP.z > -0.05 && abs(dir.z) > 1e-4) {
        float tc = (-0.05 - P.z) / dir.z;
        endP = P + dir * clamp(tc, 0.0, maxDist);
    }

    vec2 uv0 = project(P);
    vec2 uv1 = project(endP);
    float invz0 = 1.0 / P.z;
    float invz1 = 1.0 / endP.z;
    float segPx = max(length((uv1 - uv0) * screen), 1.0);
    float strideFrac = 2.0 / segPx; // ~2 px per step, max-distance-independent
    const int MAX_STEPS = 256;

    // Per-pixel jitter on the march start so the discrete steps don't line up into
    // coherent vertical streaks (breaks the pattern into fine noise).
    float jitter = (fract(sin(dot(uv0 * screen, vec2(12.9898, 78.233))) * 43758.5453) - 0.5)
                 * strideFrac;

    float prevT = 0.0;
    // Start NOT-in-front: a hit is only accepted once the ray has been seen
    // clearly in front of the scene (dz <= 0) at least once. Without this the
    // first steps sit on the originating surface (dz ~ 0, immediately "behind")
    // and register a false hit at the base — the vertical column smeared from the
    // object down to its reflection.
    bool wasInFront = false;
    for (int i = 1; i <= MAX_STEPS; ++i) {
        float t = float(i) * strideFrac + jitter;
        if (t > 1.0) break;
        if (t <= 0.0) continue;
        vec2 uv = mix(uv0, uv1, t);
        if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) break;
        float rayZ = 1.0 / mix(invz0, invz1, t);
        float rawd = texture(u_depth, uv).r;
        if (rawd >= 0.9999) { prevT = t; wasInFront = true; continue; } // sky
        float sZ = scene_z(uv);
        float dz = sZ - rayZ;
        // THICKNESS: a real hit is the ray crossing JUST behind the surface. If the
        // ray is far behind it (dz >= thickness) it actually passed behind a closer
        // foreground object (e.g. the thin cylinder's silhouette) — that's an
        // occlusion, not a reflection, so DON'T register it (this is what removes
        // the vertical column smeared between an object and its reflection).
        float thickness = max(1.0, 0.15 * abs(rayZ));
        if (wasInFront && dz > 0.0 && dz < thickness) {
            float lo = prevT, hi = t;
            for (int k = 0; k < 8; ++k) {
                float mt = (lo + hi) * 0.5;
                vec2 muv = mix(uv0, uv1, mt);
                float mrz = 1.0 / mix(invz0, invz1, mt);
                if (scene_z(muv) - mrz > 0.0) hi = mt; else lo = mt;
            }
            vec2 hitUv = mix(uv0, uv1, hi);
            float hrz = 1.0 / mix(invz0, invz1, hi);
            float residual = abs(scene_z(hitUv) - hrz);
            float conf = 1.0 - smoothstep(max(0.04 * abs(hrz), 0.04),
                                          max(0.16 * abs(hrz), 0.16), residual);
            if (conf > 0.01) {
                vec2 e = smoothstep(vec2(0.0), vec2(0.1), hitUv)
                       * (1.0 - smoothstep(vec2(0.9), vec2(1.0), hitUv));
                return vec4(texture(u_color, hitUv).rgb, e.x * e.y * conf);
            }
        }
        wasInFront = dz <= 0.0;
        prevT = t;
    }
    return vec4(0.0);
}

void main() {
    vec3 base = texture(u_color, v_uv).rgb;
    float depth = texture(u_depth, v_uv).r;
    vec4 g = texture(u_gbuf, v_uv);
    float roughness = g.b;
    float reflectivity = g.a; // env-reflection weight (0 = not a reflector)
    bool is_sky = depth >= 0.9999;

    vec3 outc = base;
    vec3 worldPos;

    if (is_sky) {
        // Reconstruct the world-space view ray to a far point so fog still
        // accumulates through empty air over the sky.
        vec2 ndc = v_uv * 2.0 - 1.0;
        if (FLIP_Y) ndc.y = -ndc.y;
        vec4 vp = u.inv_proj * vec4(ndc, 1.0, 1.0);
        vec3 vdir = normalize(vp.xyz / vp.w);
        vec3 wdir = normalize(mat3(u.inv_view) * vdir);
        worldPos = u.camera_pos.xyz + wdir * 200.0;
    } else {
        vec3 P = view_pos(v_uv, depth);
        worldPos = (u.inv_view * vec4(P, 1.0)).xyz;
        // SSR for reflective, smooth-enough surfaces (gated hard; rough/matte
        // keeps the clean env-cube reflection only).
        if (u.ssr.x > 0.5 && roughness <= u.ssr.w && reflectivity >= 1e-4) {
            vec3 N = oct_decode(g.rg); // exact view-space shading normal
            vec3 Vv = normalize(-P);
            float NdotV = max(dot(N, Vv), 1e-3);
            vec3 Nw = normalize(mat3(u.inv_view) * N);
            vec3 Vw = normalize(u.camera_pos.xyz - worldPos);
            vec3 Rw = reflect(-Vw, Nw);
            float probe_intensity = u.refl_center.w;
            if (probe_intensity > 0.0 && u.refl_extents.w > 0.5) {
                vec3 bmin = u.refl_center.xyz - u.refl_extents.xyz;
                vec3 bmax = u.refl_center.xyz + u.refl_extents.xyz;
                vec3 invR = 1.0 / Rw;
                vec3 t1 = (bmax - worldPos) * invR;
                vec3 t2 = (bmin - worldPos) * invR;
                vec3 tmax = max(t1, t2);
                Rw = (worldPos + Rw * min(min(tmax.x, tmax.y), tmax.z)) - u.refl_center.xyz;
            }
            float env_scale = probe_intensity > 0.0 ? probe_intensity : 1.0;
            vec3 env = textureLod(u_env, Rw, roughness * ENV_MAX_LOD).rgb * env_scale;
            vec4 ssr = trace(P, N);
            if (ssr.a > 0.0) {
                float rough_fade = 1.0 - smoothstep(u.ssr.w * 0.5, u.ssr.w, roughness);
                float graze_fade = smoothstep(0.15, 0.45, NdotV);
                float w = clamp(ssr.a * rough_fade * graze_fade * u.ssr.y, 0.0, 1.0);
                outc = base + reflectivity * w * (ssr.rgb - env);
            }
        }
    }

    // Volumetric fog applied to everything (surfaces + sky) so it hangs + drifts.
    if (u.fog_color.w > 0.0) {
        outc = apply_fog(outc, worldPos);
    }
    o_color = vec4(outc, 1.0);
}
