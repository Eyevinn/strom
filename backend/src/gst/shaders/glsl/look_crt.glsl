// CRT monitor look: barrel distortion, scanlines, RGB aperture grille and
// corner darkening.
uniform float u_intensity;

void main() {
    vec2 c = v_texcoord - 0.5;
    float r2 = dot(c, c);
    vec2 uv = 0.5 + c * (1.0 + 0.14 * u_intensity * r2);
    // Outside the curved tube: black.
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
        gl_FragColor = vec4(0.0, 0.0, 0.0, 1.0);
        return;
    }
    vec4 src = texture2D(tex, uv);
    vec3 rgb = src.rgb;
    // Scanlines.
    rgb *= 1.0 - u_intensity * 0.25 * (0.5 + 0.5 * sin(uv.y * height * 3.14159265));
    // Aperture grille: cycle R/G/B emphasis across pixel columns.
    float col = mod(floor(uv.x * width), 3.0);
    vec3 grille = vec3(1.0);
    if (col < 0.5) {
        grille = vec3(1.0, 0.8, 0.8);
    } else if (col < 1.5) {
        grille = vec3(0.8, 1.0, 0.8);
    } else {
        grille = vec3(0.8, 0.8, 1.0);
    }
    rgb *= mix(vec3(1.0), grille, u_intensity * 0.7);
    // Corner falloff + slight brightness compensation.
    rgb *= (1.0 - u_intensity * 0.5 * r2 * 2.0) * (1.0 + 0.15 * u_intensity);
    gl_FragColor = vec4(rgb, src.a);
}
