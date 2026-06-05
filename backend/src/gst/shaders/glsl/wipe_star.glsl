// Star wipe: a five-point star grows from the center. Radius is modulated
// by angle (spike at each segment center, valley between), normalized so
// the valleys reach the frame corners at p=1.
float wipe_mask(vec2 uv, float p) {
    vec2 q = (uv - 0.5) * vec2(width / height, 1.0);
    float seg = 6.2831853 / 5.0;
    float a = atan(q.x, -q.y) + seg * 0.5;
    float k = abs(mod(a, seg) - seg * 0.5) / (seg * 0.5);
    float rr = mix(1.0, 0.42, k);
    float corner = length(vec2(0.5 * width / height, 0.5));
    float d = (length(q) / rr) * 0.42 / corner;
    return reveal(d, p, u_softness);
}
