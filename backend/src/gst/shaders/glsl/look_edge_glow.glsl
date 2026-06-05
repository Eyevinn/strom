// Sobel edge detection added as a colored glow on top of the image.
uniform float u_glow_r;
uniform float u_glow_g;
uniform float u_glow_b;
uniform float u_intensity;

void main() {
    vec2 px = 1.0 / vec2(width, height);
    float tl = luma(texture2D(tex, v_texcoord + px * vec2(-1.0, -1.0)).rgb);
    float tc = luma(texture2D(tex, v_texcoord + px * vec2(0.0, -1.0)).rgb);
    float tr = luma(texture2D(tex, v_texcoord + px * vec2(1.0, -1.0)).rgb);
    float ml = luma(texture2D(tex, v_texcoord + px * vec2(-1.0, 0.0)).rgb);
    float mr = luma(texture2D(tex, v_texcoord + px * vec2(1.0, 0.0)).rgb);
    float bl = luma(texture2D(tex, v_texcoord + px * vec2(-1.0, 1.0)).rgb);
    float bc = luma(texture2D(tex, v_texcoord + px * vec2(0.0, 1.0)).rgb);
    float br = luma(texture2D(tex, v_texcoord + px * vec2(1.0, 1.0)).rgb);
    float gx = (tr + 2.0 * mr + br) - (tl + 2.0 * ml + bl);
    float gy = (bl + 2.0 * bc + br) - (tl + 2.0 * tc + tr);
    float edge = clamp(length(vec2(gx, gy)) * 1.5, 0.0, 1.0);
    vec4 src = texture2D(tex, v_texcoord);
    vec3 glow = vec3(u_glow_r, u_glow_g, u_glow_b) * edge * u_intensity;
    gl_FragColor = vec4(src.rgb + glow, src.a);
}
