#version 450
// Shadow depth pass: render scene geometry from a light's point of view into
// a depth map. Vertex-only (no fragment stage); the light's view-projection
// arrives as push constants, the model matrix from the instance stream (so
// casters of one mesh draw as one instanced or GPU-culled indirect draw).

layout(push_constant) uniform Push {
    mat4 light_vp;
    mat4 model; // unused (instance stream)
} pc;

layout(location = 0) in vec3 a_position;

layout(location = 8) in vec4 a_model0;
layout(location = 9) in vec4 a_model1;
layout(location = 10) in vec4 a_model2;
layout(location = 11) in vec4 a_model3;

void main() {
    mat4 model = mat4(vec4(a_model0.xyz, 0.0), a_model1, a_model2, a_model3);
    gl_Position = pc.light_vp * model * vec4(a_position, 1.0);
}
