// Two-color luma mapping: shadows -> u_low, highlights -> u_high.
// Grayscale and sepia are special cases of this.
uniform float u_low_r;
uniform float u_low_g;
uniform float u_low_b;
uniform float u_high_r;
uniform float u_high_g;
uniform float u_high_b;
uniform float u_mix;

void main() {
    vec4 src = texture2D(tex, v_texcoord);
    float y = luma(src.rgb);
    vec3 duo = mix(
        vec3(u_low_r, u_low_g, u_low_b),
        vec3(u_high_r, u_high_g, u_high_b),
        y
    );
    gl_FragColor = vec4(mix(src.rgb, duo, u_mix), src.a);
}
