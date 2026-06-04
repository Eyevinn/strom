// Barn doors: opens from a center vertical seam outward.
float wipe_mask(vec2 uv, float p) {
    float d = abs(uv.x - 0.5) * 2.0;
    return reveal(d, p, u_softness);
}
