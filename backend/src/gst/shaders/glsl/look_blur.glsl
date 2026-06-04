// Single-pass 13-tap disc blur. Not a true Gaussian but smooth enough for
// background defocus / privacy at radii up to ~20 px.
uniform float u_radius;

void main() {
    vec2 px = u_radius / vec2(width, height);
    vec4 sum = texture2D(tex, v_texcoord) * 0.1964;
    // Inner ring (r * 0.5), 4 taps
    sum += texture2D(tex, v_texcoord + px * vec2(0.5, 0.0)) * 0.1118;
    sum += texture2D(tex, v_texcoord + px * vec2(-0.5, 0.0)) * 0.1118;
    sum += texture2D(tex, v_texcoord + px * vec2(0.0, 0.5)) * 0.1118;
    sum += texture2D(tex, v_texcoord + px * vec2(0.0, -0.5)) * 0.1118;
    // Outer ring (r), 8 taps
    sum += texture2D(tex, v_texcoord + px * vec2(1.0, 0.0)) * 0.0448;
    sum += texture2D(tex, v_texcoord + px * vec2(-1.0, 0.0)) * 0.0448;
    sum += texture2D(tex, v_texcoord + px * vec2(0.0, 1.0)) * 0.0448;
    sum += texture2D(tex, v_texcoord + px * vec2(0.0, -1.0)) * 0.0448;
    sum += texture2D(tex, v_texcoord + px * vec2(0.7071, 0.7071)) * 0.0448;
    sum += texture2D(tex, v_texcoord + px * vec2(-0.7071, 0.7071)) * 0.0448;
    sum += texture2D(tex, v_texcoord + px * vec2(0.7071, -0.7071)) * 0.0448;
    sum += texture2D(tex, v_texcoord + px * vec2(-0.7071, -0.7071)) * 0.0448;
    gl_FragColor = sum;
}
