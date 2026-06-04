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
    // Smear strength peaks where the pixel is about to change over.
    float smear = (1.0 - m) * mix(1.0, -1.0, u_invert);
    vec2 uv = v_texcoord + vec2(u_dx, u_dy) * smear * 0.12;
    vec4 src = texture2D(tex, clamp(uv, 0.0, 1.0));
    gl_FragColor = vec4(src.rgb, src.a * m);
}
