// Master flash: white flash riding on top of a crossfade (concert take).
uniform float u_intensity;

void main() {
    vec4 src = texture2D(tex, v_texcoord);
    float e = envelope() * u_intensity;
    gl_FragColor = vec4(mix(src.rgb, vec3(1.0), e), src.a);
}
