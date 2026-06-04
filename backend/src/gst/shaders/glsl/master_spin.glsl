// Master spin: the frame twists around the center through the cut, with a
// slight zoom-in to keep corners covered.
uniform float u_intensity;

void main() {
    float e = envelope() * u_intensity;
    float ang = e * 0.45;
    float ca = cos(ang);
    float sa = sin(ang);
    vec2 asp = vec2(width / height, 1.0);
    vec2 q = (v_texcoord - 0.5) * asp;
    q = vec2(q.x * ca - q.y * sa, q.x * sa + q.y * ca);
    vec2 uv = q / asp * (1.0 - 0.18 * e) + 0.5;
    gl_FragColor = texture2D(tex, clamp(uv, 0.0, 1.0));
}
