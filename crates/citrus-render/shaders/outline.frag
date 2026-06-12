#version 450
// Selection outline: flat bright purple. Never touches the object's own
// surface, so the material being edited stays fully visible.

layout(location = 0) out vec4 o_color;

void main() {
    o_color = vec4(0.72, 0.25, 1.0, 1.0);
}
