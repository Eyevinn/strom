// Darkened corners. Radius is normalized so the corners sit at r = 1.
uniform float u_amount;
uniform float u_softness;

void main() {
    vec4 src = texture2D(tex, v_texcoord);
    vec2 d = v_texcoord - 0.5;
    float r = length(d) * 1.41421356;
    float v = 1.0 - u_amount * smoothstep(1.0 - u_softness, 1.0, r);
    gl_FragColor = vec4(src.rgb * v, src.a);
}
