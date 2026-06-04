// Master pixelate-through: the image dissolves into blocks across the cut
// and resolves back on the other side.
uniform float u_max_block;

void main() {
    float e = envelope();
    float block = max(1.0, e * u_max_block);
    vec2 res = vec2(width, height);
    vec2 b = vec2(block) / res;
    vec2 uv = (floor(v_texcoord / b) + 0.5) * b;
    gl_FragColor = texture2D(tex, uv);
}
