// Pinwheel: several radial blades sweep simultaneously around the center.
uniform float u_blades;

float wipe_mask(vec2 uv, float p) {
    vec2 q = (uv - 0.5) * vec2(width / height, 1.0);
    float a = atan(q.x, -q.y) / 6.2831853 + 0.5;
    float d = fract(a * u_blades);
    return reveal(d, p, u_softness * 2.0);
}
