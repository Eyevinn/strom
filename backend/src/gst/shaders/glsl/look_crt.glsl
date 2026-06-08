// CRT monitor look: barrel distortion, scanlines, an RGB aperture grille and
// corner darkening. The scanline and grille frequencies are tied to a FIXED
// virtual raster, not the output resolution — so the look is identical at any
// render size and the pattern stays well below Nyquist, instead of collapsing
// into a shimmering 1-pixel moiré the way a per-output-pixel grille does.
uniform float u_intensity;

void main() {
    vec2 c = v_texcoord - 0.5;
    float r2 = dot(c, c);
    vec2 uv = 0.5 + c * (1.0 + 0.14 * u_intensity * r2);
    // Outside the curved tube: black.
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
        gl_FragColor = vec4(0.0, 0.0, 0.0, 1.0);
        return;
    }
    vec4 src = texture2D(tex, uv);
    vec3 rgb = src.rgb;

    // Scanlines: a fixed number of raster lines with a smooth raised-cosine
    // gap profile (no hard step to alias).
    float scan = 0.5 + 0.5 * cos(uv.y * 240.0 * 6.28318531);
    rgb *= 1.0 - u_intensity * 0.35 * scan;

    // Aperture grille: three phosphor stripes 120 deg apart at a fixed pitch.
    // cos() keeps the stripes band-limited; the 120 deg spread keeps the
    // summed brightness roughly flat so white stays neutral.
    vec3 phase = vec3(uv.x * 240.0 * 6.28318531) + vec3(0.0, 2.0943951, 4.1887902);
    vec3 grille = 0.6 + 0.4 * cos(phase);
    rgb *= mix(vec3(1.0), grille, u_intensity * 0.7);

    // Corner falloff, then compensate the overall dip from scanlines + grille.
    rgb *= 1.0 - u_intensity * r2;
    rgb *= 1.0 + 0.35 * u_intensity;

    gl_FragColor = vec4(clamp(rgb, 0.0, 1.0), src.a);
}
