// Hexagon dissolve: the picture changes over in chunky hexagonal cells,
// each with a soft local reveal in pseudo-random order.
uniform float u_cell_px;

float wipe_mask(vec2 uv, float p) {
    vec2 px = uv * vec2(width, height) / max(u_cell_px, 4.0);
    vec2 r = vec2(1.0, 1.7320508);
    vec2 h = r * 0.5;
    vec2 a = mod(px, r) - h;
    vec2 b = mod(px + h, r) - h;
    vec2 g = dot(a, a) < dot(b, b) ? a : b;
    vec2 id = px - g;
    float offset = hash12(floor(id * 8.0) / 8.0 + 0.5) * 0.75;
    return clamp((p - offset) * 4.0, 0.0, 1.0);
}
