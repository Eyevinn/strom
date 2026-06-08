// Primary color correction + white balance — the camera-matching tool.
// One pass, applied in a fixed pro order: white balance, brightness,
// contrast, gamma, saturation. All controls are neutral at their defaults
// (temperature/tint/brightness = 0, contrast/gamma/saturation = 1), so an
// untouched correction is an identity pass.
uniform float u_brightness;   // additive offset, neutral 0
uniform float u_contrast;     // multiplier around mid-gray, neutral 1
uniform float u_saturation;   // 0 = grayscale, neutral 1
uniform float u_gamma;        // midtone curve, neutral 1
uniform float u_temperature;  // warm (+) / cool (-), neutral 0
uniform float u_tint;         // magenta (+) / green (-), neutral 0

void main() {
    vec4 src = texture2D(tex, v_texcoord);
    vec3 c = src.rgb;

    // White balance as per-channel gain. Temperature trades red against
    // blue; tint trades green against the red/blue (magenta) axis.
    c.r *= 1.0 + 0.3 * u_temperature;
    c.b *= 1.0 - 0.3 * u_temperature;
    c.g *= 1.0 - 0.3 * u_tint;

    // Brightness (offset) then contrast (pivot around mid-gray).
    c += u_brightness;
    c = (c - 0.5) * u_contrast + 0.5;

    // Gamma on non-negative values (pow of a negative is undefined).
    c = pow(max(c, 0.0), vec3(1.0 / max(u_gamma, 0.001)));

    // Saturation: blend toward the pixel's luma.
    float l = luma(c);
    c = mix(vec3(l), c, u_saturation);

    gl_FragColor = vec4(clamp(c, 0.0, 1.0), src.a);
}
