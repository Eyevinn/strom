// Ripple reveal: the incoming image expands from the center inside a circular
// wave whose distortion decays as the transition completes.
// Self-contained main (the mask and a UV distortion are coupled).
void main() {
    float p = progress();
    vec2 d = v_texcoord - 0.5;
    float r = length(d);
    float amp = (1.0 - p) * 0.03;
    float wave = sin(r * 60.0 - p * 25.0) * amp;
    vec2 uv = v_texcoord + (d / max(r, 0.001)) * wave;
    vec4 src = texture2D(tex, uv);
    vec2 dn = (v_texcoord - 0.5) * vec2(width / height, 1.0);
    float maxr = length(vec2(0.5 * width / height, 0.5));
    float m = reveal(length(dn) / maxr, p, 0.1);
    m = mix(m, 1.0 - m, u_invert);
    gl_FragColor = vec4(src.rgb, src.a * m);
}
