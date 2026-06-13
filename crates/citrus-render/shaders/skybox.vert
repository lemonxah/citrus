#version 450
// Fullscreen-triangle skybox: no vertex buffer. Reconstructs a world-space
// view ray per vertex from the inverse view-projection so the fragment stage
// can sample an equirectangular map (or a procedural gradient).

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

layout(location = 0) out vec3 v_dir;

void main() {
    // (0,0), (2,0), (0,2) -> covers the screen in NDC after *2-1.
    vec2 p = vec2(float((gl_VertexIndex << 1) & 2), float(gl_VertexIndex & 2));
    vec2 ndc = p * 2.0 - 1.0;

    mat4 inv = inverse(frame.view_proj);
    vec4 near = inv * vec4(ndc, 0.0, 1.0);
    vec4 far = inv * vec4(ndc, 1.0, 1.0);
    v_dir = far.xyz / far.w - near.xyz / near.w;

    // z = 1 puts the skybox on the far plane (LESS_OR_EQUAL lets it pass
    // against the cleared depth; geometry then draws in front).
    gl_Position = vec4(ndc, 1.0, 1.0);
}
