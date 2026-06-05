// Master ripple: an expanding water ring distorts the WHOLE composite while
// the crossfade runs underneath — both pictures ripple through the change.
uniform float u_intensity;

void main() {
    float e = envelope() * u_intensity;
    vec2 d = v_texcoord - 0.5;
    float r = length(d);
    float wave = sin(r * 50.0 - fx_linear_p() * 32.0) * 0.045 * e;
    vec2 uv = v_texcoord + (d / max(r, 0.001)) * wave;
    gl_FragColor = texture2D(tex, clamp(uv, 0.0, 1.0));
}
