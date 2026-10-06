// Stinger matte: turns the clip's matte into alpha for the mixer to subtract
// from the destination alpha (see vision mixer stinger pads). Black keeps the
// outgoing source, white lets the incoming one through.
//
// u_layout: 0 = the whole frame is the matte, 1 = the right half (side by
// side), 2 = the bottom half (stacked).
uniform float u_layout;
uniform float u_invert;

void main() {
    vec2 uv = v_texcoord;
    if (u_layout > 1.5) {
        uv.y = 0.5 + uv.y * 0.5;
    } else if (u_layout > 0.5) {
        uv.x = 0.5 + uv.x * 0.5;
    }
    vec3 c = texture2D(tex, uv).rgb;
    // Rec. 709 luma, as OBS reads track mattes.
    float m = dot(c, vec3(0.2126, 0.7152, 0.0722));
    m = mix(m, 1.0 - m, u_invert);
    gl_FragColor = vec4(0.0, 0.0, 0.0, clamp(m, 0.0, 1.0));
}
