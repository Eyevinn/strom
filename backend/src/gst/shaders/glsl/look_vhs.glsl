// Animated VHS tape look: line jitter, occasional tear bands, chroma shift,
// scanlines and tape noise. Self-animating via the buffer-time `time` uniform.
uniform float u_intensity;

void main() {
    vec2 uv = v_texcoord;
    // Bound the timebase before it seeds a hash: absolute `time` grows
    // unbounded and after ~a day of uptime float32 ULP swamps the per-line
    // hash step, collapsing jitter/tear into a frozen pattern. A look has no
    // take-relative anchor, so wrap (~68 min period, imperceptible for tape
    // damage). The fract(time) noise below is already bounded. (See
    // master_glitch for the precision rationale.)
    float bt = mod(time, 4096.0);
    // Per-line horizontal jitter, re-rolled ~15x per second.
    float jitter = (hash12(vec2(floor(uv.y * height), floor(bt * 15.0))) - 0.5)
        * 0.003 * u_intensity;
    // Occasional wider tear band.
    float band = step(0.985, hash12(vec2(floor(bt * 7.0), floor(uv.y * 30.0))))
        * 0.02 * u_intensity;
    uv.x += jitter + band;
    // Chroma shift: sample R and B slightly apart.
    float ca = 0.0025 * u_intensity;
    vec4 center = texture2D(tex, uv);
    float r = texture2D(tex, uv + vec2(ca, 0.0)).r;
    float b = texture2D(tex, uv - vec2(ca, 0.0)).b;
    vec3 rgb = vec3(r, center.g, b);
    // Scanlines.
    rgb *= 1.0 - u_intensity * 0.15 * (0.5 + 0.5 * sin(uv.y * height * 3.14159265));
    // Tape noise.
    rgb += (hash12(uv * vec2(width, height) + vec2(fract(time) * 60.0)) - 0.5)
        * 0.08 * u_intensity;
    gl_FragColor = vec4(rgb, center.a);
}
