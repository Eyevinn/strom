// Mosaic pixelation: snap sampling to a block grid.
uniform float u_block;

void main() {
    vec2 res = vec2(width, height);
    vec2 b = vec2(max(u_block, 1.0)) / res;
    vec2 uv = (floor(v_texcoord / b) + 0.5) * b;
    gl_FragColor = texture2D(tex, uv);
}
