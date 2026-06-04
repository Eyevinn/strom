// Noise dissolve: a film-style granular dissolve ordered by per-block noise.
uniform float u_cell;

float wipe_mask(vec2 uv, float p) {
    float n = hash12(floor(uv * vec2(width, height) / max(u_cell, 1.0)) + 0.5);
    return reveal(n, p, u_softness);
}
