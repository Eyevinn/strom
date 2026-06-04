// Venetian blinds: horizontal slats, each revealing top-to-bottom.
uniform float u_count;

float wipe_mask(vec2 uv, float p) {
    float l = fract(uv.y * u_count);
    return reveal(l, p, u_softness);
}
