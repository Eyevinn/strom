// Master glitch: RGB split, block displacement and line tearing, gated by a
// wide-topped envelope so the frame is fully shredded well before the cut
// at the midpoint — the switch itself must be unreadable.
uniform float u_intensity;

void main() {
    // Widened envelope: ramps up fast, holds near peak around the cut.
    float e = pow(envelope(), 0.4) * u_intensity;
    vec2 uv = v_texcoord;
    // Frame counter for the per-band hashes. MUST be take-relative, not
    // absolute `time`: with absolute PTS the argument to the sin()-based
    // hash grows to ~1e8 after a day of runtime, where float32 ULP (~32)
    // exceeds the per-band step (~13). Adjacent bands then collapse to the
    // same hash and all displacement freezes into a uniform (invisible)
    // shift — leaving only the hash-free RGB split. `time - u_start` stays
    // in 0..duration, so the hash is well-conditioned for any uptime.
    float t = floor((time - u_start) * 30.0);
    // Horizontal block displacement: random bands shift sideways hard.
    float band = floor(uv.y * 24.0);
    float shift = (hash12(vec2(band, t)) - 0.5) * 0.55 * e
        * step(0.35, hash12(vec2(band, t + 13.0)));
    uv.x = fract(uv.x + shift);
    // Vertical block jumps for full shredding near the peak.
    float vband = floor(uv.x * 16.0);
    uv.y = fract(uv.y + (hash12(vec2(vband, t + 29.0)) - 0.5) * 0.25 * e
        * step(0.55, hash12(vec2(vband, t + 31.0))));
    // Line tear: thin rows get an extra kick.
    float row = floor(uv.y * height);
    uv.x = fract(uv.x + (hash12(vec2(row, t)) - 0.5) * 0.05 * e
        * step(0.8, hash12(vec2(row, t + 7.0))));
    // RGB split.
    float ca = 0.03 * e;
    float r = texture2D(tex, fract(uv + vec2(ca, 0.0))).r;
    vec4 c = texture2D(tex, uv);
    float b = texture2D(tex, fract(uv - vec2(ca, 0.0))).b;
    vec3 rgb = vec3(r, c.g, b);
    // Digital noise sparkle.
    rgb += (hash12(uv * vec2(width, height) + vec2(t)) - 0.5) * 0.4 * e;
    gl_FragColor = vec4(rgb, c.a);
}
