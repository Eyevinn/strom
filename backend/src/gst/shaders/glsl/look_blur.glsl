// Disc-bokeh defocus blur. A 24-tap Vogel (golden-angle) spiral samples the
// disc with even area coverage, so it stays gap-free as the radius grows —
// unlike a fixed-ring kernel, which leaves holes and visible rings past a few
// pixels. Sampling is in pixel space (aspect-corrected by the per-axis 1/res
// scale), so the bokeh stays circular on non-square frames. Suited to
// background defocus / privacy at radii up to ~40 px.
uniform float u_radius;

void main() {
    vec2 px = u_radius / vec2(width, height);
    const int TAPS = 24;
    const float GOLDEN = 2.39996323;  // golden angle (radians)
    // Center tap, then the spiral. Flat (uniform) weights give a true disc
    // bokeh rather than a Gaussian falloff.
    vec4 sum = texture2D(tex, v_texcoord);
    for (int i = 0; i < TAPS; i++) {
        float fi = float(i) + 0.5;
        float r = sqrt(fi / float(TAPS));   // even disc coverage
        float a = fi * GOLDEN;
        sum += texture2D(tex, v_texcoord + px * r * vec2(cos(a), sin(a)));
    }
    vec4 c = sum / float(TAPS + 1);
    // Dither: blurring exposes 8-bit banding in smooth out-of-focus areas.
    gl_FragColor = vec4(dither(c.rgb, v_texcoord * vec2(width, height)), c.a);
}
