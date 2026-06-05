// Iris wipe: a screen-round circle. u_iris_close=0 grows the circle from the
// center (iris open); u_iris_close=1 reveals outside-in (iris close).
uniform float u_iris_close;

float wipe_mask(vec2 uv, float p) {
    vec2 d = (uv - 0.5) * vec2(width / height, 1.0);
    float maxr = length(vec2(0.5 * width / height, 0.5));
    float r = length(d) / maxr;
    float dd = mix(r, 1.0 - r, u_iris_close);
    return reveal(dd, p, u_softness);
}
