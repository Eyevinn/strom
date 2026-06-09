// Master punch-zoom: a hard zoom kick with camera shake — wide-topped
// envelope so the picture is fully punched in before the cut lands at the
// midpoint.
uniform float u_intensity;

void main() {
    float e = pow(envelope(), 0.5) * u_intensity;
    float zoom = 1.0 - 0.35 * e;
    // Compute the shake only while the envelope is live. Two reasons: (1) the
    // hash is pointless when e = 0, and (2) it must NOT contribute at all once
    // the transition is over — `0.0 * x` is NaN under IEEE when x is NaN/Inf,
    // and a hash of the take-relative frame counter can degrade on a stricter
    // driver, which would poison uv and clamp to a single edge texel (the
    // whole frame goes flat white or black). Gating on e keeps the post-take
    // state an exact identity pass.
    vec2 shake = vec2(0.0);
    if (e > 0.0) {
        // Take-relative frame counter: absolute `time` grows large enough over
        // a day of uptime that float32 ULP exceeds the per-frame hash step and
        // the shake freezes (see master_glitch for the full rationale).
        float t = floor((time - u_start) * 60.0);
        shake = (vec2(hash12(vec2(t, 1.0)), hash12(vec2(t, 2.0))) - 0.5) * 0.04 * e;
    }
    vec2 uv = (v_texcoord - 0.5) * zoom + 0.5 + shake;
    gl_FragColor = texture2D(tex, clamp(uv, 0.0, 1.0));
}
