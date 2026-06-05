// Halftone: newspaper-print dots on a 45-degree grid — dot size follows
// darkness.
uniform float u_dot;

void main() {
    vec2 px = v_texcoord * vec2(width, height);
    // Rotate the grid 45 degrees.
    vec2 g = vec2(px.x + px.y, px.x - px.y) * 0.70710678 / max(u_dot, 2.0);
    vec2 cell = fract(g) - 0.5;
    vec2 center = (floor(g) + 0.5) * max(u_dot, 2.0);
    vec2 suv = vec2(center.x + center.y, center.x - center.y) * 0.70710678
        / vec2(width, height);
    float y = luma(texture2D(tex, clamp(suv, 0.0, 1.0)).rgb);
    float radius = 0.7 * sqrt(max(1.0 - y, 0.0));
    float ink = 1.0 - smoothstep(radius - 0.12, radius + 0.12, length(cell) * 2.0);
    vec3 rgb = mix(vec3(0.96), vec3(0.05), ink);
    gl_FragColor = vec4(rgb, texture2D(tex, v_texcoord).a);
}
