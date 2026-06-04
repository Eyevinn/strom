// Master film burn: the frame blows out through hot orange-white, peaking at
// the cut point, with a noisy burn edge.
uniform float u_intensity;

void main() {
    vec4 src = texture2D(tex, v_texcoord);
    float e = envelope() * u_intensity;
    float n = hash12(floor(v_texcoord * vec2(width, height) / 6.0) + 0.5);
    // Burn mask sweeps through the noise field as the envelope rises.
    float burn = smoothstep(1.0 - e * 1.2, 1.0 - e * 1.2 + 0.3, n + e * 0.5);
    vec3 hot = mix(vec3(1.0, 0.45, 0.1), vec3(1.0), clamp(e * 1.5, 0.0, 1.0));
    vec3 rgb = mix(src.rgb, hot, clamp(burn * e * 2.0, 0.0, 1.0));
    gl_FragColor = vec4(rgb, src.a);
}
