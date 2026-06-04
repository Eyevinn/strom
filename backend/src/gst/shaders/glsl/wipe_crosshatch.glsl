// Crosshatch dissolve: diagonal hatch strokes mixed with grain, like an
// ink sketch filling in.
float wipe_mask(vec2 uv, float p) {
    vec2 px = uv * vec2(width, height);
    float diag1 = 0.5 + 0.5 * sin((px.x + px.y) * 0.12);
    float diag2 = 0.5 + 0.5 * sin((px.x - px.y) * 0.12);
    float grain = hash12(floor(px / 3.0) + 0.5);
    float d = 0.4 * grain + 0.3 * diag1 + 0.3 * diag2;
    return reveal(d, p, u_softness * 2.0);
}
