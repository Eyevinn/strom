// Master whip-pan: heavy directional motion blur, gated by the envelope.
// Composed with a push transition it reads as a fast camera whip.
uniform float u_dir_x;
uniform float u_dir_y;
uniform float u_intensity;

void main() {
    float e = envelope() * u_intensity;
    vec2 step_uv = vec2(u_dir_x, u_dir_y) * e * 0.05;
    vec4 sum = vec4(0.0);
    // 9 taps along the whip direction.
    for (int i = -4; i <= 4; i++) {
        sum += texture2D(tex, clamp(v_texcoord + step_uv * float(i), 0.0, 1.0));
    }
    gl_FragColor = sum / 9.0;
}
