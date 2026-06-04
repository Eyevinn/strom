// Master TV roll: vertical sync-loss roll through the cut, with a faint
// blanking bar at the wrap seam.
uniform float u_intensity;

void main() {
    float e = envelope() * u_intensity;
    float shift = e * 0.85;
    float y = fract(v_texcoord.y + shift);
    vec4 src = texture2D(tex, vec2(v_texcoord.x, y));
    // Blanking bar around the seam.
    float seam = min(y, 1.0 - y);
    float bar = 1.0 - 0.7 * e * (1.0 - smoothstep(0.0, 0.04, seam));
    gl_FragColor = vec4(src.rgb * bar, src.a);
}
