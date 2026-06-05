// Checkerboard: cells flip in a pseudo-random order with a soft local reveal.
uniform float u_cols;
uniform float u_rows;

float wipe_mask(vec2 uv, float p) {
    vec2 cell = floor(uv * vec2(u_cols, u_rows));
    // Per-cell start offset in 0..0.75 so the full board completes at p=1.
    float offset = hash12(cell + 0.5) * 0.75;
    float local = clamp((p - offset) * 4.0, 0.0, 1.0);
    return local;
}
