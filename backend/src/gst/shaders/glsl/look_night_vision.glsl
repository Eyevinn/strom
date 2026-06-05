// Night vision: green phosphor, lifted shadows, animated grain and a
// scope-style vignette.
uniform float u_intensity;

void main() {
    vec4 src = texture2D(tex, v_texcoord);
    float y = luma(src.rgb);
    // Lift shadows like an image intensifier.
    float gain = 1.0 - (1.0 - y) * (1.0 - y);
    vec3 nv = vec3(0.12, 1.0, 0.25) * gain;
    // Animated grain, stronger in dark areas.
    nv += (hash12(v_texcoord * vec2(width, height) + vec2(fract(time) * 90.0)) - 0.5)
        * 0.18 * (1.2 - y);
    // Scope vignette.
    vec2 d = (v_texcoord - 0.5) * vec2(width / height, 1.0);
    nv *= 1.0 - smoothstep(0.45, 0.75, length(d));
    gl_FragColor = vec4(mix(src.rgb, nv, u_intensity), src.a);
}
