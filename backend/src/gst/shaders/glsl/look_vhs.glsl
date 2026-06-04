// Animated VHS tape look: line jitter, occasional tear bands, chroma shift,
// scanlines and tape noise. Self-animating via the buffer-time `time` uniform.
uniform float u_intensity;

void main() {
    vec2 uv = v_texcoord;
    // Per-line horizontal jitter, re-rolled ~15x per second.
    float jitter = (hash12(vec2(floor(uv.y * height), floor(time * 15.0))) - 0.5)
        * 0.003 * u_intensity;
    // Occasional wider tear band.
    float band = step(0.985, hash12(vec2(floor(time * 7.0), floor(uv.y * 30.0))))
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
