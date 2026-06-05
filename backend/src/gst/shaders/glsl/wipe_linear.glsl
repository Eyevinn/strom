// Directional wipe. u_dx/u_dy pick the sweep axis: (1,0) reveals left-first
// moving right (matches slide_right), (-1,0) reveals right-first, etc.
uniform float u_dx;
uniform float u_dy;

float wipe_mask(vec2 uv, float p) {
    float d = 0.5 + (uv.x - 0.5) * u_dx + (uv.y - 0.5) * u_dy;
    return reveal(d, p, u_softness);
}
