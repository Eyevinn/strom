// Heart iris: a heart shape grows from the center. The classic implicit
// heart curve ((x^2+y^2-1)^3 - x^2*y^3 = 0) has no closed-form radius, so
// the ordering value is found by bisecting the scale at which the boundary
// passes through the pixel (monotone in scale; 10 steps is plenty).
float in_heart(vec2 q, float s) {
    q /= max(s, 0.001);
    float a = q.x * q.x + q.y * q.y - 1.0;
    return q.x * q.x * q.y * q.y * q.y - a * a * a;
}

float wipe_mask(vec2 uv, float p) {
    vec2 q = (uv - 0.5) * vec2(width / height, 1.0) * 1.5;
    q.y = -q.y - 0.1; // texture y points down; nudge toward optical center
    float lo = 0.01;
    float hi = 1.0;
    if (in_heart(q, hi) < 0.0) {
        return reveal(1.0, p, u_softness);
    }
    for (int i = 0; i < 10; i++) {
        float mid = 0.5 * (lo + hi);
        if (in_heart(q, mid) > 0.0) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    return reveal(hi, p, u_softness);
}
