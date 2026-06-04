// Master glitch: RGB split, block displacement and line tearing, gated by the
// transition envelope (peaks at the cut point, silent before/after).
uniform float u_intensity;

void main() {
    float e = envelope() * u_intensity;
    vec2 uv = v_texcoord;
    float t = floor(time * 30.0);
    // Horizontal block displacement: random bands shift sideways.
    float band = floor(uv.y * 24.0);
    float shift = (hash12(vec2(band, t)) - 0.5) * 0.2 * e
        * step(0.6, hash12(vec2(band, t + 13.0)));
    uv.x = fract(uv.x + shift);
    // Line tear: thin rows get an extra kick.
    float row = floor(uv.y * height);
    uv.x = fract(uv.x + (hash12(vec2(row, t)) - 0.5) * 0.02 * e
        * step(0.9, hash12(vec2(row, t + 7.0))));
    // RGB split.
    float ca = 0.01 * e;
    float r = texture2D(tex, fract(uv + vec2(ca, 0.0))).r;
    vec4 c = texture2D(tex, uv);
    float b = texture2D(tex, fract(uv - vec2(ca, 0.0))).b;
    vec3 rgb = vec3(r, c.g, b);
    // Digital noise sparkle.
    rgb += (hash12(uv * vec2(width, height) + vec2(t)) - 0.5) * 0.25 * e;
    gl_FragColor = vec4(rgb, c.a);
}
