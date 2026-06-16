#version 450
// Selection outline: flat bright purple. Never touches the object's own
// surface, so the material being edited stays fully visible.

layout(location = 0) out vec4 o_color;
// Deferred-SSR G-buffer (gbuf pipeline variants only): the outline is not a
// reflector. Zero reflectance keeps the resolve pass from reflecting it.
layout(location = 1) out vec4 o_gbuf;

void main() {
    o_color = vec4(0.72, 0.25, 1.0, 1.0);
    o_gbuf = vec4(0.0);
}
