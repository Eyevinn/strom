// Chroma key: pixels chromatically close to the key color become transparent.
// Works in CbCr space so brightness variations in the screen don't break the key.
uniform float u_key_r;
uniform float u_key_g;
uniform float u_key_b;
uniform float u_similarity;
uniform float u_smoothness;
uniform float u_spill;

vec2 chroma_cc(vec3 c) {
    float y = luma(c);
    return vec2(0.5643 * (c.b - y), 0.7132 * (c.r - y));
}

void main() {
    vec4 src = texture2D(tex, v_texcoord);
    vec2 key_cc = chroma_cc(vec3(u_key_r, u_key_g, u_key_b));
    float dist = distance(chroma_cc(src.rgb), key_cc);
    float soft = max(u_smoothness, 0.001);
    float alpha = smoothstep(u_similarity, u_similarity + soft, dist);
    // Spill suppression: desaturate pixels near the key color so keyed
    // edges don't glow green/blue against the new background.
    float spill_mask = 1.0 - smoothstep(u_similarity, u_similarity + soft + 0.1, dist);
    vec3 rgb = mix(src.rgb, vec3(luma(src.rgb)), spill_mask * u_spill);
    gl_FragColor = vec4(rgb, src.a * alpha);
}
