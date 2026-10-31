#version 450
// Fullscreen-triangle skybox: no vertex buffer. Reconstructs a world-space
// view ray per vertex from the inverse view-projection so the fragment stage
// can sample an equirectangular map (or a procedural gradient).

#ifdef MULTIVIEW
// Multiview: the full frame block for the per-eye cameras at its end.
#extension GL_EXT_multiview : require
#include "standard_common.glsl"
#else
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
#define VIEW_PROJ frame.view_proj
#define CAMERA_POS frame.camera_pos
#endif

layout(location = 0) out vec3 v_dir;

void main() {
    // (0,0), (2,0), (0,2) -> covers the screen in NDC after *2-1.
    vec2 p = vec2(float((gl_VertexIndex << 1) & 2), float(gl_VertexIndex & 2));
    vec2 ndc = p * 2.0 - 1.0;

    // Rotation + projection only: moving the camera to the origin keeps the
    // translation (large in world space, and per eye in VR) out of the
    // inverse, whose float error otherwise made the sky jitter as the head
    // moved. A mid-depth point then gives the ray without the far plane's
    // near-zero w.
    vec3 cam = CAMERA_POS.xyz;
    mat4 to_camera = mat4(1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, cam.x, cam.y, cam.z, 1.0);
    vec4 ray = inverse(VIEW_PROJ * to_camera) * vec4(ndc, 0.5, 1.0);
    v_dir = ray.xyz / ray.w;

    // z = 1 puts the skybox on the far plane (LESS_OR_EQUAL lets it pass
    // against the cleared depth; geometry then draws in front).
    gl_Position = vec4(ndc, 1.0, 1.0);
}
