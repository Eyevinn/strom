// Master negative flash: the frame inverts through the crossfade peak.
uniform float u_intensity;

void main() {
    vec4 src = texture2D(tex, v_texcoord);
    float e = envelope() * u_intensity;
    gl_FragColor = vec4(mix(src.rgb, 1.0 - src.rgb, e), src.a);
}
