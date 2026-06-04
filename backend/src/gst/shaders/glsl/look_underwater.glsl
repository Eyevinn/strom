// Underwater: slow wavy refraction, blue-green grade and drifting caustic
// shimmer. Self-animating via the buffer-time `time` uniform.
uniform float u_intensity;

void main() {
    vec2 uv = v_texcoord;
    uv.x += sin(uv.y * 14.0 + time * 1.7) * 0.006 * u_intensity;
    uv.y += cos(uv.x * 11.0 + time * 1.3) * 0.005 * u_intensity;
    vec4 src = texture2D(tex, clamp(uv, 0.0, 1.0));
    vec3 deep = src.rgb * vec3(0.55, 0.9, 1.0) + vec3(0.0, 0.03, 0.07);
    // Drifting caustic shimmer.
    float caustic = 0.5 + 0.5 * sin(uv.x * 28.0 + time * 2.1)
        * sin(uv.y * 23.0 - time * 1.6);
    deep *= 1.0 + 0.12 * u_intensity * caustic;
    gl_FragColor = vec4(mix(src.rgb, deep, u_intensity), src.a);
}
