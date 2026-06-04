// Master pixelate-through: the image dissolves into very coarse blocks
// across the cut and resolves back. Wide-topped envelope so the switch at
// the midpoint happens behind maximum pixelation.
uniform float u_max_block;

void main() {
    float e = pow(envelope(), 0.5);
    float block = max(1.0, e * u_max_block);
    vec2 res = vec2(width, height);
    vec2 b = vec2(block) / res;
    vec2 uv = (floor(v_texcoord / b) + 0.5) * b;
    gl_FragColor = texture2D(tex, uv);
}
