// Citrus "Flux" GI common helpers — our own versions of established real-time GI
// shader math, ported faithfully for consistent, physically-based results.
// Sources (UE 5.8): SHCommon.ush, OctahedralCommon.ush, MonteCarlo.ush,
// and the screen-probe filtering shaders. Constants are kept verbatim (load-bearing).
//
// Naming: snake_case mirrors of the UE functions, e.g. sh_basis3 ==
// SHBasisFunction3, eval_sh_irradiance == EvaluateSHIrradiance.
#ifndef FLUX_COMMON
#define FLUX_COMMON

const float FLUX_PI = 3.14159265359;

// ----------------------------------------------------------------- octahedral
// Standard octahedral encode/decode (UE OctahedralCommon.ush). Used for storing
// a unit vector (e.g. a probe normal) in 2 channels.
vec2 unit_vector_to_octahedron(vec3 n) {
    n.xy /= dot(vec3(1.0), abs(n));
    if (n.z <= 0.0) {
        n.xy = (1.0 - abs(n.yx)) * vec2(n.x >= 0.0 ? 1.0 : -1.0, n.y >= 0.0 ? 1.0 : -1.0);
    }
    return n.xy;
}
vec3 octahedron_to_unit_vector(vec2 oct) {
    vec3 n = vec3(oct, 1.0 - dot(vec2(1.0), abs(oct)));
    float t = max(-n.z, 0.0);
    n.x += n.x >= 0.0 ? -t : t;
    n.y += n.y >= 0.0 ? -t : t;
    return normalize(n);
}

// Clarberg equal-area sphere mapping (UE MonteCarlo.ush EquiAreaSphericalMapping).
// The probe rays are traced through this mapping (NOT the plain octahedron), so the
// per-probe octahedral radiance atlas direction must use this to match.
vec3 equiarea_to_unit_vector(vec2 uv) {
    uv = 2.0 * uv - 1.0;
    float d = 1.0 - (abs(uv.x) + abs(uv.y));
    float r = 1.0 - abs(d);
    float phi = (r == 0.0) ? 0.0 : (FLUX_PI / 4.0) * ((abs(uv.y) - abs(uv.x)) / r + 1.0);
    float f = r * sqrt(2.0 - r * r);
    return vec3(
        f * sign(uv.x) * abs(cos(phi)),
        f * sign(uv.y) * abs(sin(phi)),
        sign(d) * (1.0 - r * r));
}

// ------------------------------------------------------------------------- SH
// 3-band (L2, 9-coefficient) real spherical harmonics, matching UE SHCommon.ush.
// V0 = {l0, three l1}, V1 = {four l2}, V2 = last l2.
struct SH3 { vec4 v0; vec4 v1; float v2; };
struct SH3RGB { SH3 r; SH3 g; SH3 b; };

SH3 sh3_zero() { SH3 o; o.v0 = vec4(0.0); o.v1 = vec4(0.0); o.v2 = 0.0; return o; }
SH3RGB sh3rgb_zero() { SH3RGB o; o.r = sh3_zero(); o.g = sh3_zero(); o.b = sh3_zero(); return o; }

// SHBasisFunction3: direction -> 9 basis values. Verbatim constants.
SH3 sh_basis3(vec3 d) {
    SH3 o;
    o.v0 = vec4(0.282095, -0.488603 * d.y, 0.488603 * d.z, -0.488603 * d.x);
    vec3 d2 = d * d;
    o.v1 = vec4(1.092548 * d.x * d.y,
                -1.092548 * d.y * d.z,
                0.315392 * (3.0 * d2.z - 1.0),
                -1.092548 * d.x * d.z);
    o.v2 = 0.546274 * (d2.x - d2.y);
    return o;
}

// CalcDiffuseTransferSH3: cosine-lobe transfer (Exponent=1 = Lambertian).
SH3 calc_diffuse_transfer_sh3(vec3 n, float e) {
    SH3 r = sh_basis3(n);
    float l0 = 2.0 * FLUX_PI / (1.0 + e);
    float l1 = 2.0 * FLUX_PI / (2.0 + e);
    float l2 = e * 2.0 * FLUX_PI / (3.0 + 4.0 * e + e * e);
    r.v0.x *= l0;
    r.v0.yzw *= l1;
    r.v1 *= l2;
    r.v2 *= l2;
    return r;
}

float dot_sh3(SH3 a, SH3 b) { return dot(a.v0, b.v0) + dot(a.v1, b.v1) + a.v2 * b.v2; }
vec3 dot_sh3_rgb(SH3RGB a, SH3 b) {
    return vec3(dot_sh3(a.r, b), dot_sh3(a.g, b), dot_sh3(a.b, b));
}
SH3 mul_sh3(SH3 a, float s) { SH3 o; o.v0 = a.v0 * s; o.v1 = a.v1 * s; o.v2 = a.v2 * s; return o; }
SH3 add_sh3(SH3 a, SH3 b) { SH3 o; o.v0 = a.v0 + b.v0; o.v1 = a.v1 + b.v1; o.v2 = a.v2 + b.v2; return o; }
// basis * radiance, per channel — project one sample direction's radiance to SH.
SH3RGB mul_sh3_rgb(SH3 basis, vec3 c) {
    SH3RGB o; o.r = mul_sh3(basis, c.r); o.g = mul_sh3(basis, c.g); o.b = mul_sh3(basis, c.b); return o;
}
SH3RGB add_sh3_rgb(SH3RGB a, SH3RGB b) {
    SH3RGB o; o.r = add_sh3(a.r, b.r); o.g = add_sh3(a.g, b.g); o.b = add_sh3(a.b, b.b); return o;
}
SH3RGB scale_sh3_rgb(SH3RGB a, float s) {
    SH3RGB o; o.r = mul_sh3(a.r, s); o.g = mul_sh3(a.g, s); o.b = mul_sh3(a.b, s); return o;
}

// EvaluateSHIrradiance: directional-occlusion-aware SH -> diffuse irradiance in
// `dir` (AO=1 → fully open cone → plain cosine-lobe dot). Caller multiplies the
// result by 4*PI (the 4π is applied at integration time).
vec3 eval_sh_irradiance(vec3 dir, float ao, SH3RGB sh) {
    SH3 t = calc_diffuse_transfer_sh3(dir, 1.0);
    float cos_a = sqrt(clamp(1.0 - ao, 0.0, 1.0));
    float sin_a = sqrt(clamp(1.0 - cos_a * cos_a, 0.0, 1.0));
    float z0 = sin_a * sin_a;
    float z1 = 1.0 - cos_a * cos_a * cos_a;
    float z2 = sin_a * sin_a * (1.0 + 3.0 * cos_a * cos_a);
    t.v0.x *= z0;
    t.v0.yzw *= z1;
    t.v1 *= z2;
    t.v2 *= z2;
    return max(vec3(0.0), dot_sh3_rgb(sh, t));
}

// --------------------------------------------------- low-discrepancy sampling
// Van der Corput / Hammersley (UE uses Hammersley16 for probe jitter + sampling).
// Stratified samples have far lower variance than pure-random at low ray counts,
// so the gather is much less noisy for the same ray budget.
float radical_inverse_vdc(uint bits) {
    bits = (bits << 16u) | (bits >> 16u);
    bits = ((bits & 0x55555555u) << 1u) | ((bits & 0xAAAAAAAAu) >> 1u);
    bits = ((bits & 0x33333333u) << 2u) | ((bits & 0xCCCCCCCCu) >> 2u);
    bits = ((bits & 0x0F0F0F0Fu) << 4u) | ((bits & 0xF0F0F0F0u) >> 4u);
    bits = ((bits & 0x00FF00FFu) << 8u) | ((bits & 0xFF00FF00u) >> 8u);
    return float(bits) * 2.3283064365386963e-10; // / 2^32
}
vec2 hammersley(uint i, uint n) {
    return vec2(float(i) / float(n), radical_inverse_vdc(i));
}

// Cosine-weighted hemisphere direction around n from a 2D sample u in [0,1)².
// pdf = cosθ/π, so a Lambertian estimate is the plain mean of incoming radiance.
vec3 cosine_hemisphere_dir(vec3 n, vec2 u) {
    float r = sqrt(u.x);
    float phi = 2.0 * FLUX_PI * u.y;
    // Frisvad-style orthonormal basis around n.
    float s = n.z >= 0.0 ? 1.0 : -1.0;
    float a = -1.0 / (s + n.z);
    float b = n.x * n.y * a;
    vec3 t = vec3(1.0 + s * n.x * n.x * a, s * b, -s * n.x);
    vec3 bt = vec3(b, s + n.y * n.y * a, -n.y);
    vec3 d = t * (r * cos(phi)) + bt * (r * sin(phi)) + n * sqrt(max(1.0 - u.x, 0.0));
    return normalize(length(d) > 1e-5 ? d : n);
}

// --------------------------------------------------------------- denoise utils
// MaxRayIntensity-style firefly clamp: scale the whole RGB so its max channel ==
// max_intensity (hue-preserving), rather than a per-channel min.
vec3 firefly_clamp(vec3 c, float max_intensity) {
    float m = max(c.r, max(c.g, c.b));
    return m > max_intensity ? c * (max_intensity / m) : c;
}

// Plane-distance bilateral weight (UE PLANE_WEIGHTING upsample): how much a
// neighbour at `sample_pos` lies on the fragment's tangent plane, relative to
// depth. `neg_scale` is negative (e.g. -100); returns exp2(neg_scale * relDiff²).
float plane_depth_weight(vec3 frag_pos, vec3 frag_normal, vec3 sample_pos,
                         float frag_dist, float neg_scale) {
    float plane_dist = abs(dot(vec4(sample_pos, -1.0), vec4(frag_normal, dot(frag_pos, frag_normal))));
    float rel = plane_dist / max(frag_dist, 1e-4);
    return exp2(neg_scale * rel * rel);
}

#endif // FLUX_COMMON
