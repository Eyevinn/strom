// Clock wipe: a radial sweep starting at 12 o'clock, going clockwise.
float wipe_mask(vec2 uv, float p) {
    vec2 d = uv - 0.5;
    // atan(x, -y): 0 at 12 o'clock, increasing clockwise (y points down in
    // texture space), normalized to 0..1.
    float a = atan(d.x, -d.y) / 6.28318531 + 0.5;
    return reveal(a, p, u_softness * 0.5);
}
