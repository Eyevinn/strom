// Noise dissolve: organic blobby dissolve ordered by smooth two-octave
// value noise (soft edges instead of hard per-block grain).
uniform float u_cell;

float vnoise(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    f = f * f * (3.0 - 2.0 * f);
    float a = hash12(i + 0.5);
    float b = hash12(i + vec2(1.5, 0.5));
    float c = hash12(i + vec2(0.5, 1.5));
    float d = hash12(i + vec2(1.5, 1.5));
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

float wipe_mask(vec2 uv, float p) {
    vec2 q = uv * vec2(width, height) / max(u_cell, 8.0);
    float n = 0.65 * vnoise(q) + 0.35 * vnoise(q * 2.7 + 13.1);
    return reveal(n, p, u_softness * 4.0);
}
