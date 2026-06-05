// Posterize: quantize colors into bands for a screen-print / toon look.
uniform float u_levels;

void main() {
    vec4 src = texture2D(tex, v_texcoord);
    float n = max(u_levels, 2.0);
    vec3 rgb = floor(src.rgb * n + 0.5) / n;
    gl_FragColor = vec4(rgb, src.a);
}
