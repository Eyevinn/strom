// Master TV roll: a vertical sync-loss roll through the cut. The picture eases
// into a slow roll, accelerates into a fast, motion-blurred smear of wrapping
// frames that hides the delayed cut at the midpoint, then decelerates and
// locks back to sync. A dark blanking bar and a horizontal tear ride the wrap
// seam so the frame visibly "loses sync" rather than simply scrolling.
uniform float u_intensity;

void main() {
    float p = fx_linear_p();
    // Monotonic roll over N whole frame-heights. smootherstep eases the roll
    // velocity to zero at both ends (so it locks cleanly, and ends on an
    // integer number of wraps = back in sync) while peaking at the midpoint,
    // exactly where the delayed cut is hidden.
    float N = 10.0;
    float s = p * p * p * (p * (p * 6.0 - 15.0) + 10.0); // smootherstep
    float roll = N * s;
    // Roll speed = d/dp of N*smootherstep; drives motion blur and tearing.
    float vel = N * 30.0 * p * p * (1.0 - p) * (1.0 - p);
    float blur = clamp(vel * 0.015, 0.0, 0.45) * u_intensity;

    // Horizontal tear riding the wrap seam, gated by roll speed. The two sides
    // of the seam (frame top meeting frame bottom) shove opposite ways.
    float yc = fract(v_texcoord.y + roll);
    float seam = min(yc, 1.0 - yc);
    float tear = (1.0 - smoothstep(0.0, 0.12, seam))
        * clamp(vel * 0.03, 0.0, 1.0) * u_intensity;
    float xoff = tear * 0.06 * sign(0.5 - yc);

    // Vertical motion blur: average taps spread along the roll direction so the
    // fast portion smears into an unreadable blur.
    vec3 acc = vec3(0.0);
    for (int i = 0; i < 7; i++) {
        float o = (float(i) / 6.0 - 0.5) * blur;
        float yy = fract(v_texcoord.y + roll + o);
        acc += texture2D(tex, vec2(v_texcoord.x + xoff, yy)).rgb;
    }
    vec3 rgb = acc / 7.0;

    // Dark blanking/retrace bar at the seam, gated by roll speed (like the
    // tear above) so it vanishes the instant the frame locks. vel is exactly
    // 0 at p=0 and p=1, and the roll ends on an integer wrap there, which
    // pins the seam to the top/bottom edges — without this gate the bar would
    // leave a permanent dark band along both edges after the transition.
    float bar = (1.0 - smoothstep(0.0, 0.05, seam))
        * clamp(vel * 0.05, 0.0, 1.0) * u_intensity;
    rgb *= 1.0 - 0.85 * bar;

    gl_FragColor = vec4(rgb, texture2D(tex, vec2(v_texcoord.x, yc)).a);
}
