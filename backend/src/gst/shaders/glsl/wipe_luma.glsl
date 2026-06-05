// Luma wipe: the incoming image reveals darkest areas first, highlights last.
float wipe_mask(vec2 uv, float p) {
    float y = luma(texture2D(tex, uv).rgb);
    return reveal(y, p, u_softness);
}
