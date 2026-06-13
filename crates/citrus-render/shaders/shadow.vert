#version 450
// Shadow depth pass: render scene geometry from a light's point of view into
// a depth map. Vertex-only (no fragment stage); the light's view-projection
// and the model matrix arrive as push constants (same 128-byte block as the
// main pass, reinterpreted).

layout(push_constant) uniform Push {
    mat4 light_vp;
    mat4 model;
} pc;

layout(location = 0) in vec3 a_position;

void main() {
    gl_Position = pc.light_vp * pc.model * vec4(a_position, 1.0);
}
