// Heart iris: a heart shape grows from the center. The classic implicit
// heart curve ((x^2+y^2-1)^3 - x^2*y^3 = 0) has no closed-form radius, so
// the ordering value is found by bisecting the scale at which the boundary
// passes through the pixel (monotone in scale; bisection is plenty).
float in_heart(vec2 q, float s) {
    q /= max(s, 0.001);
    float a = q.x * q.x + q.y * q.y - 1.0;
    return q.x * q.x * q.y * q.y * q.y - a * a * a;
}

// Pixel position in heart space. Texture y points down, so flip. The heart
// curve is taller above the origin (lobes reach ~1.2, tip -1.0), so shift the
// evaluation point up to drop the rendered shape onto the optical center.
vec2 heart_q(vec2 uv) {
    vec2 q = (uv - 0.5) * vec2(width / height, 1.0) * 1.5;
    q.y = -q.y + 0.1;
    return q;
}

// Smallest scale at which q falls inside the heart (i.e. the scale the growing
// heart reaches the pixel). Capped at SMAX, which is generous enough to cover
// any on-screen pixel for a normal aspect ratio.
float heart_scale(vec2 q) {
    float lo = 0.01;
    float hi = 3.0;
    if (in_heart(q, hi) < 0.0) {
        return hi;
    }
    for (int i = 0; i < 14; i++) {
        float mid = 0.5 * (lo + hi);
        if (in_heart(q, mid) > 0.0) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    return hi;
}

float wipe_mask(vec2 uv, float p) {
    float s = heart_scale(heart_q(uv));
    // Normalize by the hardest-to-cover frame point so the ordering value
    // reaches 1.0 exactly when the heart has filled the screen. Without this,
    // every pixel beyond the max-scale heart shares one ordering value and the
    // whole border pops in together at the end of the transition. The corners
    // dominate (the heart narrows toward the tip, so the bottom corners need
    // the most scale); sampling the four corners captures the maximum.
    float smax = heart_scale(heart_q(vec2(0.0, 0.0)));
    smax = max(smax, heart_scale(heart_q(vec2(1.0, 0.0))));
    smax = max(smax, heart_scale(heart_q(vec2(0.0, 1.0))));
    smax = max(smax, heart_scale(heart_q(vec2(1.0, 1.0))));
    float d = clamp(s / max(smax, 0.001), 0.0, 1.0);
    return reveal(d, p, u_softness);
}
