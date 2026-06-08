// Primary color correction + white balance — the camera-matching tool.
// One pass. Brightness, contrast and hue/saturation are computed in YCbCr
// (BT.601, consistent with luma()) so they behave like glcolorbalance:
// brightness/contrast move only the luma and never drift hue, while hue and
// saturation act only on the chroma. White balance is a per-channel RGB gain
// applied first; gamma is the final per-channel midtone curve. Every control
// is neutral at its default, so an untouched correction is an identity pass.
uniform float u_brightness;   // additive luma offset, neutral 0
uniform float u_contrast;     // luma multiplier around mid-gray, neutral 1
uniform float u_saturation;   // chroma scale, 0 = grayscale, neutral 1
uniform float u_hue;          // chroma rotation, -1..1 = -180..180 deg, neutral 0
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

    // Decompose into luma + chroma (BT.601).
    float y = luma(c);
    float cb = 0.5643 * (c.b - y);
    float cr = 0.7132 * (c.r - y);

    // Brightness + contrast on the luma only — preserves hue/saturation.
    y = (y - 0.5) * u_contrast + 0.5 + u_brightness;

    // Hue: rotate the (Cb, Cr) plane.
    float a = u_hue * 3.14159265;
    float cs = cos(a);
    float sn = sin(a);
    float cb2 = cb * cs - cr * sn;
    float cr2 = cb * sn + cr * cs;

    // Saturation: scale the (rotated) chroma.
    cb = cb2 * u_saturation;
    cr = cr2 * u_saturation;

    // Recompose to RGB.
    c = vec3(
        y + 1.402 * cr,
        y - 0.344136 * cb - 0.714136 * cr,
        y + 1.772 * cb
    );

    // Gamma on non-negative values (pow of a negative is undefined).
    c = pow(max(c, 0.0), vec3(1.0 / max(u_gamma, 0.001)));

    gl_FragColor = vec4(clamp(c, 0.0, 1.0), src.a);
}
