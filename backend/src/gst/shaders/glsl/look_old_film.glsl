// Animated old-film look: warm desaturation, frame flicker, grain, vertical
// scratches and a light corner falloff. Runs at a 24 fps "cadence" so the
// damage pattern holds for a film frame instead of strobing per video frame.
uniform float u_intensity;

void main() {
    vec2 uv = v_texcoord;
    float t = floor(time * 24.0) / 24.0;
    vec4 src = texture2D(tex, uv);
    float y = luma(src.rgb);
    vec3 warm = vec3(y * 1.05, y * 0.95, y * 0.78);
    vec3 rgb = mix(src.rgb, warm, 0.6 * u_intensity);
    // Frame flicker.
    rgb *= 1.0 + u_intensity * 0.12 * (hash12(vec2(t, 3.7)) - 0.5) * 2.0;
    // Grain.
    rgb += (hash12(uv * vec2(width, height) + vec2(fract(t) * 100.0)) - 0.5)
        * 0.12 * u_intensity;
    // Occasional vertical scratch at a random x.
    float sx = hash12(vec2(t, 17.0));
    float scratch = step(0.6, hash12(vec2(t, 5.0)))
        * (1.0 - smoothstep(0.0, 0.002, abs(uv.x - sx)));
    rgb += scratch * 0.25 * u_intensity;
    // Projector light falloff.
    vec2 d = uv - 0.5;
    rgb *= 1.0 - u_intensity * 0.7 * dot(d, d);
    gl_FragColor = vec4(rgb, src.a);
}
