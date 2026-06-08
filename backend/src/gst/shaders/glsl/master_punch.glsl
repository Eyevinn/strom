// Master punch-zoom: a hard zoom kick with camera shake — wide-topped
// envelope so the picture is fully punched in before the cut lands at the
// midpoint.
uniform float u_intensity;

void main() {
    float e = pow(envelope(), 0.5) * u_intensity;
    float zoom = 1.0 - 0.35 * e;
    // Take-relative frame counter: absolute `time` grows large enough over a
    // day of uptime that float32 ULP exceeds the per-frame hash step and the
    // shake freezes (see master_glitch for the full rationale).
    float t = floor((time - u_start) * 60.0);
    vec2 shake = (vec2(hash12(vec2(t, 1.0)), hash12(vec2(t, 2.0))) - 0.5) * 0.04 * e;
    vec2 uv = (v_texcoord - 0.5) * zoom + 0.5 + shake;
    gl_FragColor = texture2D(tex, clamp(uv, 0.0, 1.0));
}
