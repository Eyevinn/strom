//! Embedded GLSL shader library for the vision mixer FX engine (GPU backend
//! only).
//!
//! # Animation model
//!
//! Shaders are self-animating: `glshader` sets a `time` uniform on every
//! frame from the **buffer PTS** (seconds), so all animation is evaluated at
//! composite time — the same principle as the border/geometry work. Rust sets
//! parameters once per event (`u_start`, `u_duration`, effect params) via the
//! `uniforms` property; the shader computes its own progress per frame:
//!
//! ```glsl
//! float p = clamp((time - u_start) / u_duration, 0.0, 1.0);
//! ```
//!
//! After a transition completes (`p == 1`) wipe masks evaluate to fully
//! opaque and master envelopes to zero — i.e. the shaders become identity
//! passes and can stay installed until the next take re-programs them.
//!
//! Precision note: `time` is a float32 of absolute PTS seconds. After hours
//! of runtime the subtraction `time - u_start` carries a few milliseconds of
//! quantization — invisible for transitions of 100 ms and up.
//!
//! # Compatibility
//!
//! All GLSL is written in GLES2 style (`varying`, `texture2D`,
//! `gl_FragColor`, no `#version`) — the same dialect `gleffects` ships, which
//! compiles on desktop GL compatibility contexts and GLES alike. A failed
//! shader compile posts a GST element error and kills the pipeline, so only
//! fragments from this module (CI-validated, see `shader_validation_test`)
//! may ever reach a `glshader` element.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_gl as gst_gl;
use strom_types::effects::{parse_hex_rgb, VideoEffect};
use tracing::{debug, error};

/// Common header: GStreamer's default GL filter vertex shader provides
/// `v_texcoord`; `glshader` itself sets `tex`, `time`, `width`, `height`.
const PRELUDE: &str = r#"
#ifdef GL_ES
precision highp float;
#endif
varying vec2 v_texcoord;
uniform sampler2D tex;
uniform float time;
uniform float width;
uniform float height;
"#;

/// Shared helpers available to every effect body.
const LIB: &str = r#"
float luma(vec3 c) {
    return dot(c, vec3(0.2989, 0.5866, 0.1145));
}
float hash12(vec2 p) {
    return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453);
}
// Soft-edged reveal: 1 where the ordering value d has been passed by the
// sweep at progress p, with a soft band of width s. At p=0 nothing is
// revealed, at p=1 everything is (for any d in 0..1).
float reveal(float d, float p, float s) {
    float edge = mix(-s, 1.0 + s, p);
    return smoothstep(0.0, max(s, 0.0001), edge - d + s);
}
"#;

/// Timing block for transitions: programmed once per take.
const TIMING: &str = r#"
uniform float u_start;
uniform float u_duration;
float fx_linear_p() {
    if (u_duration <= 0.0) return 1.0;
    return clamp((time - u_start) / u_duration, 0.0, 1.0);
}
// Eased progress for wipes (smoothstep), 0 -> 1.
float progress() {
    float p = fx_linear_p();
    return p * p * (3.0 - 2.0 * p);
}
// Master FX envelope: 0 -> 1 -> 0, peaking at the cut point.
float envelope() {
    if (u_duration <= 0.0) return 0.0;
    return sin(fx_linear_p() * 3.14159265);
}
"#;

/// Wipe shaders share a soft-edge width and (unless self-contained) a common
/// `main` that masks the incoming source's alpha. The mixer's normal
/// over-blend against the outgoing source does the rest.
const WIPE_DECLS: &str = r#"
uniform float u_softness;
uniform float u_invert;
"#;

const WIPE_MAIN: &str = r#"
void main() {
    vec4 src = texture2D(tex, v_texcoord);
    float m = clamp(wipe_mask(v_texcoord, progress()), 0.0, 1.0);
    m = mix(m, 1.0 - m, u_invert);
    gl_FragColor = vec4(src.rgb, src.a * m);
}
"#;

const IDENTITY_BODY: &str = r#"
void main() {
    gl_FragColor = texture2D(tex, v_texcoord);
}
"#;

const LOOK_CHROMA_KEY: &str = include_str!("glsl/look_chroma_key.glsl");
const LOOK_PIXELATE: &str = include_str!("glsl/look_pixelate.glsl");
const LOOK_BLUR: &str = include_str!("glsl/look_blur.glsl");
const LOOK_DUOTONE: &str = include_str!("glsl/look_duotone.glsl");
const LOOK_VIGNETTE: &str = include_str!("glsl/look_vignette.glsl");
const LOOK_VHS: &str = include_str!("glsl/look_vhs.glsl");
const LOOK_OLD_FILM: &str = include_str!("glsl/look_old_film.glsl");
const LOOK_EDGE_GLOW: &str = include_str!("glsl/look_edge_glow.glsl");

const WIPE_LINEAR: &str = include_str!("glsl/wipe_linear.glsl");
const WIPE_CLOCK: &str = include_str!("glsl/wipe_clock.glsl");
const WIPE_IRIS: &str = include_str!("glsl/wipe_iris.glsl");
const WIPE_BLINDS: &str = include_str!("glsl/wipe_blinds.glsl");
const WIPE_CHECKER: &str = include_str!("glsl/wipe_checker.glsl");
const WIPE_NOISE: &str = include_str!("glsl/wipe_noise.glsl");
const WIPE_LUMA: &str = include_str!("glsl/wipe_luma.glsl");
const WIPE_RIPPLE: &str = include_str!("glsl/wipe_ripple.glsl");

const MASTER_GLITCH: &str = include_str!("glsl/master_glitch.glsl");
const MASTER_FLASH: &str = include_str!("glsl/master_flash.glsl");
const MASTER_WHIP: &str = include_str!("glsl/master_whip.glsl");
const MASTER_PUNCH: &str = include_str!("glsl/master_punch.glsl");
const MASTER_PIXELATE: &str = include_str!("glsl/master_pixelate.glsl");
const MASTER_BURN: &str = include_str!("glsl/master_burn.glsl");

/// Default soft-edge width for wipes, in normalized ordering units.
const DEFAULT_WIPE_SOFTNESS: f32 = 0.05;

/// The identity fragment installed on every FX slot at build time.
pub fn identity_fragment() -> String {
    format!("{}{}", PRELUDE, IDENTITY_BODY)
}

const GL_FRAGMENT_SHADER: u32 = 0x8B30;

/// Attach the `create-shader` handler that makes runtime fragment swaps
/// work. `glshader` only compiles from its `fragment` property while it has
/// **no** shader (the first frame); once one exists, the property is inert
/// and `update-shader=true` merely emits `create-shader` — with no handler
/// the existing shader is kept (see `_maybe_recompile_shader` in
/// gstglfiltershader.c). This handler compiles the element's current
/// `fragment` string on the GL thread and returns the new shader.
///
/// On compile failure it logs and returns `None`, which keeps the previous
/// shader running — a runtime swap can never kill the pipeline (unlike the
/// initial property-path compile, which posts an element error).
///
/// The closure captures nothing — the element arrives as a signal argument
/// (no strong-ref cycles, per the project rule for GStreamer closures).
pub fn attach_create_shader_handler(elem: &gst::Element) {
    elem.connect("create-shader", false, |args| {
        let element = match args[0].get::<gst::Element>() {
            Ok(e) => e,
            Err(_) => return None,
        };
        let fragment = element.property::<Option<String>>("fragment")?;
        let Some(context) = element.property::<Option<gst_gl::GLContext>>("context") else {
            error!(
                "{}: create-shader fired without a GL context",
                element.name()
            );
            return None;
        };

        let shader = gst_gl::GLShader::new(&context);
        let vertex = gst_gl::GLSLStage::new_default_vertex(&context);
        if let Err(e) = shader.compile_attach_stage(&vertex) {
            error!("{}: vertex stage failed: {}", element.name(), e);
            return None;
        }
        let frag_stage = gst_gl::GLSLStage::with_string(
            &context,
            GL_FRAGMENT_SHADER,
            gst_gl::GLSLVersion::None,
            gst_gl::GLSLProfile::empty(),
            &fragment,
        );
        if let Err(e) = shader.compile_attach_stage(&frag_stage) {
            error!(
                "{}: fragment compile failed (keeping previous shader): {}",
                element.name(),
                e
            );
            return None;
        }
        if let Err(e) = shader.link() {
            error!("{}: shader link failed: {}", element.name(), e);
            return None;
        }
        debug!("{}: runtime shader swap compiled OK", element.name());
        Some(shader.to_value())
    });
}

fn compose_look(body: &str) -> String {
    format!("{}{}{}", PRELUDE, LIB, body)
}

fn compose_wipe(body: &str, self_contained: bool) -> String {
    if self_contained {
        format!("{}{}{}{}{}", PRELUDE, LIB, TIMING, WIPE_DECLS, body)
    } else {
        format!(
            "{}{}{}{}{}{}",
            PRELUDE, LIB, TIMING, WIPE_DECLS, body, WIPE_MAIN
        )
    }
}

fn compose_master(body: &str) -> String {
    format!("{}{}{}{}", PRELUDE, LIB, TIMING, body)
}

/// Build the `uniforms` GstStructure from `(name, value)` pairs. All values
/// are `f32` — `glshader` maps `G_TYPE_FLOAT` to `glUniform1f`.
fn uniforms(pairs: &[(&str, f32)]) -> gst::Structure {
    let mut b = gst::Structure::builder("uniforms");
    for (name, value) in pairs {
        b = b.field(*name, *value);
    }
    b.build()
}

/// Fragment + uniforms for a persistent [`VideoEffect`] ("look").
/// The effect must already be sanitized — invalid colors fall back to white.
pub fn effect_shader(effect: &VideoEffect) -> (String, gst::Structure) {
    let rgb = |c: &str| parse_hex_rgb(c).unwrap_or((1.0, 1.0, 1.0));
    match effect {
        VideoEffect::None => (identity_fragment(), uniforms(&[])),
        VideoEffect::ChromaKey {
            key_color,
            similarity,
            smoothness,
            spill,
        } => {
            let (r, g, b) = rgb(key_color);
            (
                compose_look(LOOK_CHROMA_KEY),
                uniforms(&[
                    ("u_key_r", r),
                    ("u_key_g", g),
                    ("u_key_b", b),
                    ("u_similarity", *similarity),
                    ("u_smoothness", *smoothness),
                    ("u_spill", *spill),
                ]),
            )
        }
        VideoEffect::Pixelate { block_size } => (
            compose_look(LOOK_PIXELATE),
            uniforms(&[("u_block", *block_size)]),
        ),
        VideoEffect::Blur { radius } => {
            (compose_look(LOOK_BLUR), uniforms(&[("u_radius", *radius)]))
        }
        VideoEffect::Duotone { low, high, mix } => {
            let (lr, lg, lb) = rgb(low);
            let (hr, hg, hb) = rgb(high);
            (
                compose_look(LOOK_DUOTONE),
                uniforms(&[
                    ("u_low_r", lr),
                    ("u_low_g", lg),
                    ("u_low_b", lb),
                    ("u_high_r", hr),
                    ("u_high_g", hg),
                    ("u_high_b", hb),
                    ("u_mix", *mix),
                ]),
            )
        }
        VideoEffect::Vignette { amount, softness } => (
            compose_look(LOOK_VIGNETTE),
            uniforms(&[("u_amount", *amount), ("u_softness", *softness)]),
        ),
        VideoEffect::Vhs { intensity } => (
            compose_look(LOOK_VHS),
            uniforms(&[("u_intensity", *intensity)]),
        ),
        VideoEffect::OldFilm { intensity } => (
            compose_look(LOOK_OLD_FILM),
            uniforms(&[("u_intensity", *intensity)]),
        ),
        VideoEffect::EdgeGlow { color, intensity } => {
            let (r, g, b) = rgb(color);
            (
                compose_look(LOOK_EDGE_GLOW),
                uniforms(&[
                    ("u_glow_r", r),
                    ("u_glow_g", g),
                    ("u_glow_b", b),
                    ("u_intensity", *intensity),
                ]),
            )
        }
    }
}

/// Shader-mask wipe transitions, applied to the incoming source's TAKE slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WipeKind {
    Left,
    Right,
    Up,
    Down,
    Clock,
    IrisOpen,
    IrisClose,
    Blinds,
    Checker,
    Noise,
    Luma,
    Ripple,
}

impl WipeKind {
    pub fn name(&self) -> &'static str {
        match self {
            WipeKind::Left => "wipe_left",
            WipeKind::Right => "wipe_right",
            WipeKind::Up => "wipe_up",
            WipeKind::Down => "wipe_down",
            WipeKind::Clock => "clock_wipe",
            WipeKind::IrisOpen => "iris_open",
            WipeKind::IrisClose => "iris_close",
            WipeKind::Blinds => "blinds",
            WipeKind::Checker => "checker_wipe",
            WipeKind::Noise => "noise_dissolve",
            WipeKind::Luma => "luma_wipe",
            WipeKind::Ripple => "ripple",
        }
    }

    /// The composed fragment shader for this wipe.
    pub fn fragment(&self) -> String {
        match self {
            WipeKind::Left | WipeKind::Right | WipeKind::Up | WipeKind::Down => {
                compose_wipe(WIPE_LINEAR, false)
            }
            WipeKind::Clock => compose_wipe(WIPE_CLOCK, false),
            WipeKind::IrisOpen | WipeKind::IrisClose => compose_wipe(WIPE_IRIS, false),
            WipeKind::Blinds => compose_wipe(WIPE_BLINDS, false),
            WipeKind::Checker => compose_wipe(WIPE_CHECKER, false),
            WipeKind::Noise => compose_wipe(WIPE_NOISE, false),
            WipeKind::Luma => compose_wipe(WIPE_LUMA, false),
            WipeKind::Ripple => compose_wipe(WIPE_RIPPLE, true),
        }
    }

    /// Uniforms for one run of this wipe starting at `start_s` (buffer-PTS
    /// seconds) over `duration_s`.
    pub fn run_uniforms(&self, start_s: f64, duration_s: f64) -> gst::Structure {
        let mut pairs: Vec<(&str, f32)> = vec![
            ("u_start", start_s as f32),
            ("u_duration", duration_s as f32),
            ("u_softness", DEFAULT_WIPE_SOFTNESS),
            // The take engine always runs wipes inverted: the mask removes
            // the OUTGOING source, revealing the incoming one underneath.
            // In-flight buffers rendered before the shader swap then show as
            // "PGM unchanged" instead of flashing the destination (the
            // mask/alpha race is structurally impossible this way around).
            ("u_invert", 1.0),
        ];
        match self {
            // Direction matches the slide convention: wipe_left's edge enters
            // at the right and sweeps left, etc.
            WipeKind::Left => pairs.extend([("u_dx", -1.0f32), ("u_dy", 0.0f32)]),
            WipeKind::Right => pairs.extend([("u_dx", 1.0f32), ("u_dy", 0.0f32)]),
            WipeKind::Up => pairs.extend([("u_dx", 0.0f32), ("u_dy", -1.0f32)]),
            WipeKind::Down => pairs.extend([("u_dx", 0.0f32), ("u_dy", 1.0f32)]),
            WipeKind::Clock => {}
            WipeKind::IrisOpen => pairs.push(("u_iris_close", 0.0)),
            WipeKind::IrisClose => pairs.push(("u_iris_close", 1.0)),
            WipeKind::Blinds => pairs.push(("u_count", 12.0)),
            WipeKind::Checker => pairs.extend([("u_cols", 10.0f32), ("u_rows", 6.0f32)]),
            WipeKind::Noise => pairs.push(("u_cell", 3.0)),
            WipeKind::Luma => {}
            WipeKind::Ripple => {}
        }
        uniforms(&pairs)
    }
}

/// How a master-FX take drives the underlying pad transition while the
/// envelope shader runs on the PGM slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterBase {
    /// Hard cut at the envelope midpoint (peak hides the cut).
    DelayedCut,
    /// Crossfade over the full duration.
    Fade,
    /// Push in a direction (dx, dy) — composed with the whip blur.
    Push(i32, i32),
}

/// Full-frame master FX transitions, applied to the PGM MASTER slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterFxKind {
    Glitch,
    Flash,
    WhipLeft,
    WhipRight,
    Punch,
    Pixelate,
    Burn,
}

impl MasterFxKind {
    pub fn name(&self) -> &'static str {
        match self {
            MasterFxKind::Glitch => "glitch_cut",
            MasterFxKind::Flash => "flash_dissolve",
            MasterFxKind::WhipLeft => "whip_pan_left",
            MasterFxKind::WhipRight => "whip_pan_right",
            MasterFxKind::Punch => "punch_zoom",
            MasterFxKind::Pixelate => "pixelate_take",
            MasterFxKind::Burn => "film_burn",
        }
    }

    /// The pad transition that runs underneath the envelope.
    pub fn base(&self) -> MasterBase {
        match self {
            MasterFxKind::Glitch
            | MasterFxKind::Punch
            | MasterFxKind::Pixelate
            | MasterFxKind::Burn => MasterBase::DelayedCut,
            MasterFxKind::Flash => MasterBase::Fade,
            MasterFxKind::WhipLeft => MasterBase::Push(-1, 0),
            MasterFxKind::WhipRight => MasterBase::Push(1, 0),
        }
    }

    /// The composed fragment shader for this master FX.
    pub fn fragment(&self) -> String {
        match self {
            MasterFxKind::Glitch => compose_master(MASTER_GLITCH),
            MasterFxKind::Flash => compose_master(MASTER_FLASH),
            MasterFxKind::WhipLeft | MasterFxKind::WhipRight => compose_master(MASTER_WHIP),
            MasterFxKind::Punch => compose_master(MASTER_PUNCH),
            MasterFxKind::Pixelate => compose_master(MASTER_PIXELATE),
            MasterFxKind::Burn => compose_master(MASTER_BURN),
        }
    }

    /// Uniforms for one run of this master FX.
    pub fn run_uniforms(&self, start_s: f64, duration_s: f64) -> gst::Structure {
        let mut pairs: Vec<(&str, f32)> = vec![
            ("u_start", start_s as f32),
            ("u_duration", duration_s as f32),
        ];
        match self {
            MasterFxKind::Glitch => pairs.push(("u_intensity", 1.0)),
            MasterFxKind::Flash => pairs.push(("u_intensity", 1.0)),
            MasterFxKind::WhipLeft => pairs.extend([
                ("u_dir_x", -1.0f32),
                ("u_dir_y", 0.0f32),
                ("u_intensity", 1.0f32),
            ]),
            MasterFxKind::WhipRight => pairs.extend([
                ("u_dir_x", 1.0f32),
                ("u_dir_y", 0.0f32),
                ("u_intensity", 1.0f32),
            ]),
            MasterFxKind::Punch => pairs.push(("u_intensity", 1.0)),
            MasterFxKind::Pixelate => pairs.push(("u_max_block", 64.0)),
            MasterFxKind::Burn => pairs.push(("u_intensity", 1.0)),
        }
        uniforms(&pairs)
    }
}

/// Every distinct fragment the library can produce, with a representative
/// effect/uniform set — the CI validation test compiles each of these in a
/// real GL pipeline. Keep exhaustive: a fragment missing here can reach
/// production uncompiled and kill the pipeline at runtime.
pub fn all_fragments() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = vec![("identity".to_string(), identity_fragment())];
    let looks = [
        VideoEffect::ChromaKey {
            key_color: "#00B140".into(),
            similarity: 0.35,
            smoothness: 0.1,
            spill: 0.5,
        },
        VideoEffect::Pixelate { block_size: 24.0 },
        VideoEffect::Blur { radius: 6.0 },
        VideoEffect::Duotone {
            low: "#000000".into(),
            high: "#FFFFFF".into(),
            mix: 1.0,
        },
        VideoEffect::Vignette {
            amount: 0.5,
            softness: 0.5,
        },
        VideoEffect::Vhs { intensity: 0.5 },
        VideoEffect::OldFilm { intensity: 0.5 },
        VideoEffect::EdgeGlow {
            color: "#00FFD0".into(),
            intensity: 0.5,
        },
    ];
    for e in &looks {
        v.push((format!("look_{}", e.kind()), effect_shader(e).0));
    }
    for k in [
        WipeKind::Left,
        WipeKind::Clock,
        WipeKind::IrisOpen,
        WipeKind::Blinds,
        WipeKind::Checker,
        WipeKind::Noise,
        WipeKind::Luma,
        WipeKind::Ripple,
    ] {
        v.push((k.name().to_string(), k.fragment()));
    }
    for k in [
        MasterFxKind::Glitch,
        MasterFxKind::Flash,
        MasterFxKind::WhipLeft,
        MasterFxKind::Punch,
        MasterFxKind::Pixelate,
        MasterFxKind::Burn,
    ] {
        v.push((k.name().to_string(), k.fragment()));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_are_distinct_and_nonempty() {
        let frags = all_fragments();
        assert!(frags.len() >= 15);
        for (name, src) in &frags {
            assert!(src.contains("void main()"), "{} lacks main()", name);
            assert!(src.contains("gl_FragColor"), "{} lacks output", name);
        }
    }

    #[test]
    fn wipe_uniforms_carry_timing() {
        let s = WipeKind::Left.run_uniforms(12.5, 0.8);
        assert_eq!(s.get::<f32>("u_start").unwrap(), 12.5);
        assert_eq!(s.get::<f32>("u_duration").unwrap(), 0.8);
        assert_eq!(s.get::<f32>("u_dx").unwrap(), -1.0);
    }

    #[test]
    fn master_bases() {
        assert_eq!(MasterFxKind::Glitch.base(), MasterBase::DelayedCut);
        assert_eq!(MasterFxKind::Flash.base(), MasterBase::Fade);
        assert_eq!(MasterFxKind::WhipLeft.base(), MasterBase::Push(-1, 0));
    }
}
