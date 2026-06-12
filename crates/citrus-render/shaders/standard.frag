#version 450
// citrus standard shader — fragment stage (phase 1: PBR/toon hybrid core).
// Feature toggles are specialization constants: each material's enabled
// feature set selects a pipeline variant; disabled features compile out.

layout(constant_id = 0) const bool FEAT_TOON = false;
layout(constant_id = 1) const bool FEAT_NORMAL_MAP = false;
layout(constant_id = 2) const bool FEAT_EMISSION = false;
layout(constant_id = 3) const uint ALPHA_MODE = 0u; // 0 opaque, 1 cutout, 2 blend

layout(set = 0, binding = 0) uniform FrameData {
    mat4 view;
    mat4 proj;
    mat4 view_proj;
    vec4 camera_pos;
    vec4 light_dir;
    vec4 light_color;
    vec4 ambient;
    vec4 misc; // x = time in seconds
} frame;

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
    vec3 L = normalize(-frame.light_dir.xyz);
    vec3 H = normalize(V + L);
    float NdotL = max(dot(N, L), 0.0);
    float NdotV = max(dot(N, V), 1e-4);
    float NdotH = max(dot(N, H), 0.0);
    float VdotH = max(dot(V, H), 0.0);

    vec3 f0 = mix(vec3(0.04), albedo.rgb, metallic);
    vec3 diffuse_color = albedo.rgb * (1.0 - metallic);

    vec3 spec = d_ggx(NdotH, roughness * roughness)
        * g_smith(NdotV, NdotL, roughness)
        * f_schlick(VdotH, f0)
        / max(4.0 * NdotV * NdotL, 1e-4);
    vec3 lit = (diffuse_color / PI + spec) * NdotL;

    if (FEAT_TOON) {
        float steps = max(pc.params0.z, 2.0);
        float banded = floor(clamp(NdotL, 0.0, 0.999) * steps) / (steps - 1.0);
        vec3 lit_toon = (diffuse_color / PI) * banded
            + spec * NdotL * step(0.001, banded);
        lit = mix(lit, lit_toon, clamp(pc.params0.w, 0.0, 1.0));
    }

    vec3 color = lit * frame.light_color.rgb
        + frame.ambient.rgb * diffuse_color * ao;

    if (FEAT_EMISSION) {
        color += texture(t_emission, v_uv).rgb * pc.emission.rgb;
    }

    float alpha = (ALPHA_MODE == 2u) ? albedo.a : 1.0;
    o_color = vec4(color, alpha);
}
