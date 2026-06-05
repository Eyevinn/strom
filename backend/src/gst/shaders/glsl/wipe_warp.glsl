// Directional warp wipe: like a linear wipe, but the picture smears along
// the sweep direction near the moving edge. Self-contained main (mask and
// UV distortion are coupled).
uniform float u_dx;
uniform float u_dy;

void main() {
    float p = progress();
    float d = 0.5 + (v_texcoord.x - 0.5) * u_dx + (v_texcoord.y - 0.5) * u_dy;
    float m = reveal(d, p, u_softness * 3.0);
    m = mix(m, 1.0 - m, u_invert);
    // Smear strength peaks where the pixel is about to change over, but
    // eases out over a much wider band than the alpha edge so the drag is
    // visible deep into the picture, not just on the seam.
    float band = reveal(d, p, u_softness * 10.0);
    band = mix(band, 1.0 - band, u_invert);
    float smear = (1.0 - band) * mix(1.0, -1.0, u_invert);
    if (abs(smear) < 0.002) {
        // Parked / fully revealed: plain sample, skip the blur taps.
        vec4 src = texture2D(tex, v_texcoord);
        gl_FragColor = vec4(src.rgb, src.a * m);
        return;
    }
    // One-sided directional blur trailing the sweep: the picture reads as
    // being dragged along with the edge (same tap idiom as master_whip).
    vec2 step_uv = vec2(u_dx, u_dy) * smear * 0.045;
    vec4 sum = vec4(0.0);
    for (int i = 0; i <= 8; i++) {
        sum += texture2D(tex, clamp(v_texcoord + step_uv * float(i), 0.0, 1.0));
    }
    vec4 src = sum / 9.0;
    gl_FragColor = vec4(src.rgb, src.a * m);
}
