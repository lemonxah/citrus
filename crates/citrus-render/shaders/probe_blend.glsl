// Reflection-probe helpers for the passes that composite over the forward
// pass's environment reflection (SSR resolve, RT reflections). They must match
// standard.frag's probe_dir / probe_influence, or those passes subtract a
// different environment than the forward pass added.

// Box-projected reflection direction for a probe; the raw direction for a
// surface outside its box (flush surfaces count as inside).
vec3 probe_box_dir(vec3 P, vec3 R, vec3 capture, vec3 center, vec3 extents, bool project) {
    if (!project) return R;
    vec3 bmin = center - extents;
    vec3 bmax = center + extents;
    vec3 eps = extents * 0.02 + 0.02;
    bool inside = all(greaterThanEqual(P, bmin - eps)) && all(lessThanEqual(P, bmax + eps));
    if (!inside) return R;
    vec3 invR = 1.0 / R;
    vec3 tmax = max((bmax - P) * invR, (bmin - P) * invR);
    float dist = max(min(min(tmax.x, tmax.y), tmax.z), 0.0);
    return P + R * dist - capture;
}

// 0 outside the box, ramping to 1 across the blend shell inside its faces.
float probe_influence(vec3 p, vec3 center, vec3 extents, float blend) {
    vec3 d = extents - abs(p - center);
    float inner = min(d.x, min(d.y, d.z));
    if (blend <= 1e-4) return inner >= 0.0 ? 1.0 : 0.0;
    return clamp(inner / blend, 0.0, 1.0);
}

// Weight of the second probe (slot 1) at p; `b_on_top` = slot 1 wins where
// both boxes overlap. Outside both, slot 0 is used.
float probe_b_weight(vec3 p, vec3 box0, vec3 ext0, float blend0, vec3 box1, vec3 ext1, float blend1, bool b_on_top) {
    float f0 = probe_influence(p, box0, ext0, blend0);
    float f1 = probe_influence(p, box1, ext1, blend1);
    return b_on_top ? f1 : f1 * (1.0 - f0);
}
