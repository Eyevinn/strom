//! Shader-based video effects for the vision mixer (GPU backend only).
//!
//! These map to custom GLSL fragment shaders embedded in the backend and
//! applied through `glshader` elements in the GPU pipeline. Effects are
//! runtime-only state (like DSK toggles): they reset when the flow restarts.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// A persistent video effect ("look") applied to a vision mixer input or to
/// the PGM master output.
///
/// JSON encoding is internally tagged on `type`:
/// `{"type": "pixelate", "block_size": 24.0}`.
///
/// All numeric parameters are clamped server-side (see [`VideoEffect::sanitized`]);
/// colors are `#RRGGBB` hex strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VideoEffect {
    /// No effect (identity passthrough).
    #[default]
    None,
    /// Chroma key: pixels close to `key_color` become transparent.
    ChromaKey {
        /// Key color as `#RRGGBB` (typically green `#00B140` or blue).
        #[serde(default = "default_key_color")]
        key_color: String,
        /// Chroma distance below which pixels are fully keyed (0..1).
        #[serde(default = "default_similarity")]
        similarity: f32,
        /// Soft edge width added above `similarity` (0..1).
        #[serde(default = "default_smoothness")]
        smoothness: f32,
        /// Spill suppression amount — desaturates key-colored fringes (0..1).
        #[serde(default = "default_spill")]
        spill: f32,
    },
    /// Mosaic pixelation (privacy or stylization).
    Pixelate {
        /// Block size in source pixels (2..200).
        #[serde(default = "default_block_size")]
        block_size: f32,
    },
    /// Disc-bokeh defocus blur (24-tap Vogel spiral, single pass).
    Blur {
        /// Blur radius in source pixels (0..40).
        #[serde(default = "default_blur_radius")]
        radius: f32,
    },
    /// Two-color luma mapping. `low`=black point color, `high`=white point
    /// color. Grayscale = `#000000`/`#FFFFFF`, sepia = `#2B1D0E`/`#FFF1DF`.
    Duotone {
        #[serde(default = "default_duotone_low")]
        low: String,
        #[serde(default = "default_duotone_high")]
        high: String,
        /// Blend between original (0) and duotone (1).
        #[serde(default = "default_one")]
        mix: f32,
    },
    /// Darkened corners.
    Vignette {
        /// Strength of the darkening (0..1).
        #[serde(default = "default_vignette_amount")]
        amount: f32,
        /// Falloff softness (0.01..1).
        #[serde(default = "default_vignette_softness")]
        softness: f32,
    },
    /// Animated VHS look: chroma shift, scanlines, tape noise.
    Vhs {
        /// Overall intensity (0..1).
        #[serde(default = "default_half")]
        intensity: f32,
    },
    /// Animated old-film look: grain, flicker, scratches, warm tone.
    OldFilm {
        /// Overall intensity (0..1).
        #[serde(default = "default_half")]
        intensity: f32,
    },
    /// Edge detection glow added on top of the image.
    EdgeGlow {
        /// Glow color as `#RRGGBB`.
        #[serde(default = "default_glow_color")]
        color: String,
        /// Glow strength (0..1).
        #[serde(default = "default_half")]
        intensity: f32,
    },
    /// CRT monitor: barrel distortion, scanlines, RGB grille.
    Crt {
        /// Overall intensity (0..1).
        #[serde(default = "default_half")]
        intensity: f32,
    },
    /// Newspaper-print halftone dots.
    Halftone {
        /// Dot grid size in pixels (3..40).
        #[serde(default = "default_dot_size")]
        dot_size: f32,
    },
    /// Thermal-camera false color over luma.
    Thermal {
        /// Blend between original (0) and thermal (1).
        #[serde(default = "default_one")]
        intensity: f32,
    },
    /// Night-vision scope: green phosphor, lifted shadows, grain, vignette.
    NightVision {
        /// Blend between original (0) and night vision (1).
        #[serde(default = "default_one")]
        intensity: f32,
    },
    /// Color quantization into bands (screen-print / toon).
    Posterize {
        /// Number of levels per channel (2..16).
        #[serde(default = "default_levels")]
        levels: f32,
    },
    /// Underwater: wavy refraction, blue-green grade, caustic shimmer.
    Underwater {
        /// Overall intensity (0..1).
        #[serde(default = "default_half")]
        intensity: f32,
    },
    /// Primary color correction + white balance — the camera-matching tool.
    /// Every control is neutral at its default, so an untouched correction is
    /// an identity pass. Applied in a fixed order: white balance, brightness,
    /// contrast, hue, saturation, gamma.
    ColorCorrect {
        /// Additive brightness offset (-1..1, neutral 0).
        #[serde(default = "default_zero")]
        brightness: f32,
        /// Contrast multiplier around mid-gray (0..2, neutral 1).
        #[serde(default = "default_one")]
        contrast: f32,
        /// Saturation: 0 = grayscale, 1 = unchanged, up to 2 (0..2, neutral 1).
        #[serde(default = "default_one")]
        saturation: f32,
        /// Hue rotation: -1..1 maps to -180..180 degrees (neutral 0).
        #[serde(default = "default_zero")]
        hue: f32,
        /// Midtone gamma curve (0.1..3, neutral 1).
        #[serde(default = "default_one")]
        gamma: f32,
        /// White balance temperature: warm (+) / cool (-) (-1..1, neutral 0).
        #[serde(default = "default_zero")]
        temperature: f32,
        /// White balance tint: magenta (+) / green (-) (-1..1, neutral 0).
        #[serde(default = "default_zero")]
        tint: f32,
    },
}

fn default_key_color() -> String {
    "#00B140".to_string()
}
fn default_similarity() -> f32 {
    0.35
}
fn default_smoothness() -> f32 {
    0.1
}
fn default_spill() -> f32 {
    0.5
}
fn default_block_size() -> f32 {
    24.0
}
fn default_blur_radius() -> f32 {
    6.0
}
fn default_duotone_low() -> String {
    "#000000".to_string()
}
fn default_duotone_high() -> String {
    "#FFFFFF".to_string()
}
fn default_one() -> f32 {
    1.0
}
fn default_zero() -> f32 {
    0.0
}
fn default_half() -> f32 {
    0.5
}
fn default_vignette_amount() -> f32 {
    0.5
}
fn default_vignette_softness() -> f32 {
    0.5
}
fn default_glow_color() -> String {
    "#00FFD0".to_string()
}
fn default_dot_size() -> f32 {
    8.0
}
fn default_levels() -> f32 {
    5.0
}

/// Parse `#RRGGBB` (case-insensitive) into RGB components in `0.0..=1.0`.
pub fn parse_hex_rgb(color: &str) -> Option<(f32, f32, f32)> {
    let s = color.trim().strip_prefix('#')?;
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some((
        p(0)? as f32 / 255.0,
        p(2)? as f32 / 255.0,
        p(4)? as f32 / 255.0,
    ))
}

fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        lo
    }
}

impl VideoEffect {
    /// Short stable identifier for the effect kind (used in logs and as the
    /// shader cache key).
    pub fn kind(&self) -> &'static str {
        match self {
            VideoEffect::None => "none",
            VideoEffect::ChromaKey { .. } => "chroma_key",
            VideoEffect::Pixelate { .. } => "pixelate",
            VideoEffect::Blur { .. } => "blur",
            VideoEffect::Duotone { .. } => "duotone",
            VideoEffect::Vignette { .. } => "vignette",
            VideoEffect::Vhs { .. } => "vhs",
            VideoEffect::OldFilm { .. } => "old_film",
            VideoEffect::EdgeGlow { .. } => "edge_glow",
            VideoEffect::Crt { .. } => "crt",
            VideoEffect::Halftone { .. } => "halftone",
            VideoEffect::Thermal { .. } => "thermal",
            VideoEffect::NightVision { .. } => "night_vision",
            VideoEffect::Posterize { .. } => "posterize",
            VideoEffect::Underwater { .. } => "underwater",
            VideoEffect::ColorCorrect { .. } => "color_correct",
        }
    }

    /// Validate colors and clamp all numeric parameters into their documented
    /// ranges. Returns an error naming the offending field for bad colors.
    pub fn sanitized(&self) -> Result<VideoEffect, String> {
        let check_color = |name: &str, c: &str| -> Result<String, String> {
            parse_hex_rgb(c)
                .map(|_| c.trim().to_string())
                .ok_or_else(|| format!("invalid {} '{}' — expected #RRGGBB", name, c))
        };
        Ok(match self {
            VideoEffect::None => VideoEffect::None,
            VideoEffect::ChromaKey {
                key_color,
                similarity,
                smoothness,
                spill,
            } => VideoEffect::ChromaKey {
                key_color: check_color("key_color", key_color)?,
                similarity: clamp(*similarity, 0.0, 1.0),
                smoothness: clamp(*smoothness, 0.0, 1.0),
                spill: clamp(*spill, 0.0, 1.0),
            },
            VideoEffect::Pixelate { block_size } => VideoEffect::Pixelate {
                block_size: clamp(*block_size, 2.0, 200.0),
            },
            VideoEffect::Blur { radius } => VideoEffect::Blur {
                radius: clamp(*radius, 0.0, 40.0),
            },
            VideoEffect::Duotone { low, high, mix } => VideoEffect::Duotone {
                low: check_color("low", low)?,
                high: check_color("high", high)?,
                mix: clamp(*mix, 0.0, 1.0),
            },
            VideoEffect::Vignette { amount, softness } => VideoEffect::Vignette {
                amount: clamp(*amount, 0.0, 1.0),
                softness: clamp(*softness, 0.01, 1.0),
            },
            VideoEffect::Vhs { intensity } => VideoEffect::Vhs {
                intensity: clamp(*intensity, 0.0, 1.0),
            },
            VideoEffect::OldFilm { intensity } => VideoEffect::OldFilm {
                intensity: clamp(*intensity, 0.0, 1.0),
            },
            VideoEffect::EdgeGlow { color, intensity } => VideoEffect::EdgeGlow {
                color: check_color("color", color)?,
                intensity: clamp(*intensity, 0.0, 1.0),
            },
            VideoEffect::Crt { intensity } => VideoEffect::Crt {
                intensity: clamp(*intensity, 0.0, 1.0),
            },
            VideoEffect::Halftone { dot_size } => VideoEffect::Halftone {
                dot_size: clamp(*dot_size, 3.0, 40.0),
            },
            VideoEffect::Thermal { intensity } => VideoEffect::Thermal {
                intensity: clamp(*intensity, 0.0, 1.0),
            },
            VideoEffect::NightVision { intensity } => VideoEffect::NightVision {
                intensity: clamp(*intensity, 0.0, 1.0),
            },
            VideoEffect::Posterize { levels } => VideoEffect::Posterize {
                levels: clamp(*levels, 2.0, 16.0),
            },
            VideoEffect::Underwater { intensity } => VideoEffect::Underwater {
                intensity: clamp(*intensity, 0.0, 1.0),
            },
            VideoEffect::ColorCorrect {
                brightness,
                contrast,
                saturation,
                hue,
                gamma,
                temperature,
                tint,
            } => VideoEffect::ColorCorrect {
                brightness: clamp(*brightness, -1.0, 1.0),
                contrast: clamp(*contrast, 0.0, 2.0),
                saturation: clamp(*saturation, 0.0, 2.0),
                hue: clamp(*hue, -1.0, 1.0),
                gamma: clamp(*gamma, 0.1, 3.0),
                temperature: clamp(*temperature, -1.0, 1.0),
                tint: clamp(*tint, -1.0, 1.0),
            },
        })
    }
}

/// Where a [`VideoEffect`] is applied on a vision mixer block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum EffectTarget {
    /// A video input by index — the effect follows the source everywhere
    /// (PGM, PVW, thumbnails, PiPs).
    Input(usize),
    /// The PGM master output — applied after composition.
    Master,
}

impl std::fmt::Display for EffectTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EffectTarget::Input(i) => write!(f, "input {}", i),
            EffectTarget::Master => write!(f, "master"),
        }
    }
}

/// Request to set a video effect on a vision mixer block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SetVideoEffectRequest {
    /// Where to apply the effect.
    pub target: EffectTarget,
    /// The effect to apply. `{"type": "none"}` clears.
    pub effect: VideoEffect,
}

/// Response after setting a video effect.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SetVideoEffectResponse {
    pub message: String,
    /// The effect as applied (after parameter clamping).
    pub effect: VideoEffect,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_rgb_works() {
        assert_eq!(parse_hex_rgb("#FF0000"), Some((1.0, 0.0, 0.0)));
        assert_eq!(parse_hex_rgb("#00ff00"), Some((0.0, 1.0, 0.0)));
        assert!(parse_hex_rgb("#GGGGGG").is_none());
        assert!(parse_hex_rgb("red").is_none());
        assert!(parse_hex_rgb("#FFF").is_none());
    }

    #[test]
    fn sanitize_clamps_params() {
        let e = VideoEffect::Pixelate { block_size: 9999.0 };
        assert_eq!(
            e.sanitized().unwrap(),
            VideoEffect::Pixelate { block_size: 200.0 }
        );
        let e = VideoEffect::Blur { radius: f32::NAN };
        assert_eq!(e.sanitized().unwrap(), VideoEffect::Blur { radius: 0.0 });
    }

    #[test]
    fn sanitize_rejects_bad_color() {
        let e = VideoEffect::ChromaKey {
            key_color: "green".to_string(),
            similarity: 0.3,
            smoothness: 0.1,
            spill: 0.5,
        };
        assert!(e.sanitized().is_err());
    }

    #[test]
    fn effect_json_roundtrip() {
        let e = VideoEffect::Duotone {
            low: "#000000".to_string(),
            high: "#FFF1DF".to_string(),
            mix: 0.8,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"duotone\""));
        let back: VideoEffect = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn effect_defaults_fill_in() {
        let e: VideoEffect = serde_json::from_str(r#"{"type":"chroma_key"}"#).unwrap();
        match e {
            VideoEffect::ChromaKey { key_color, .. } => assert_eq!(key_color, "#00B140"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn target_json_shape() {
        let t = EffectTarget::Input(2);
        assert_eq!(serde_json::to_string(&t).unwrap(), r#"{"input":2}"#);
        let t = EffectTarget::Master;
        assert_eq!(serde_json::to_string(&t).unwrap(), r#""master""#);
    }
}
