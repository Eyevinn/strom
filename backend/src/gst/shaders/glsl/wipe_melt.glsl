// Doom-style melt: the picture drips away in narrow columns, each with a
// random head start. Self-contained main — inverted mode (the take engine's
// preferred orientation) translates the outgoing content downward per
// column; upright mode does a column-staggered top-down reveal instead.
// Linear progress: melt speed should be constant, not eased.
void main() {
    float colw = max(width / 48.0, 2.0);
    float off = hash12(vec2(floor(v_texcoord.x * width / colw), 7.0));
    float p = fx_linear_p();
    vec2 uv = v_texcoord;
    float m;
    if (u_invert > 0.5) {
        float fall = max(0.0, p * 1.35 - off * 0.35);
        uv.y -= fall;
        m = step(0.0, uv.y);
    } else {
        m = reveal(v_texcoord.y * 0.7 + off * 0.3, p, u_softness);
    }
    vec4 src = texture2D(tex, clamp(uv, 0.0, 1.0));
    gl_FragColor = vec4(src.rgb, src.a * m);
}
