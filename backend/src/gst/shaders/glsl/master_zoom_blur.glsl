// Master zoom blur: radial streak blur toward the center, peaking at the
// cut point (CrossZoom-style).
uniform float u_intensity;

void main() {
    float e = envelope() * u_intensity;
    vec2 d = v_texcoord - 0.5;
    vec4 sum = vec4(0.0);
    for (int i = 0; i < 8; i++) {
        float t = float(i) / 7.0;
        sum += texture2D(tex, v_texcoord - d * t * 0.3 * e);
    }
    gl_FragColor = sum / 8.0;
}
