// Thermal camera: false-color palette over luma
// (black -> blue -> magenta -> red -> orange -> yellow -> white).
uniform float u_intensity;

vec3 thermal_palette(float t) {
    vec3 c1 = vec3(0.0, 0.0, 0.0);
    vec3 c2 = vec3(0.1, 0.0, 0.6);
    vec3 c3 = vec3(0.7, 0.0, 0.7);
    vec3 c4 = vec3(1.0, 0.2, 0.0);
    vec3 c5 = vec3(1.0, 0.8, 0.0);
    vec3 c6 = vec3(1.0, 1.0, 1.0);
    vec3 col = mix(c1, c2, smoothstep(0.00, 0.25, t));
    col = mix(col, c3, smoothstep(0.25, 0.45, t));
    col = mix(col, c4, smoothstep(0.45, 0.65, t));
    col = mix(col, c5, smoothstep(0.65, 0.85, t));
    col = mix(col, c6, smoothstep(0.85, 1.00, t));
    return col;
}

void main() {
    vec4 src = texture2D(tex, v_texcoord);
    vec3 heat = thermal_palette(luma(src.rgb));
    gl_FragColor = vec4(mix(src.rgb, heat, u_intensity), src.a);
}
