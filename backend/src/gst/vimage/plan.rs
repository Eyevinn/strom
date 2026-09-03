//! Deciding, once per caps negotiation, whether vImage can do a conversion —
//! and if so, exactly which call to make for each frame.
//!
//! [`Plan::build`] is the gate. It returns `None` for anything vImage has no
//! direct path for; the element then falls back to `GstVideoConverter`, the
//! same code `videoconvert` runs. Nothing here allocates or fails at streaming
//! time: every decision, including the generated colour matrices, is resolved
//! in `build` and reused for every frame.

use gstreamer_video::{VideoColorMatrix, VideoColorRange, VideoFormat, VideoInfo};

use super::accelerate as acc;

/// Which Y'CbCr layout a plan reads or writes, together with everything
/// needed to find its planes in a `VideoFrameRef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum YuvKind {
    /// Y plane + interleaved CbCr plane (`NV12`).
    Nv12,
    /// Y plane + separate Cb and Cr planes. `I420` and `YV12` differ only in
    /// which plane index holds which chroma component, so both land here.
    Planar420 { cb_plane: u32, cr_plane: u32 },
    /// `UYVY` — Cb Y Cr Y in memory (vImage's `422CbYpCrYp8`).
    Uyvy,
    /// `YUY2` — Y Cb Y Cr in memory (vImage's `422YpCbYpCr8`).
    Yuy2,
}

impl YuvKind {
    fn vimage_type(self) -> u32 {
        match self {
            YuvKind::Nv12 => acc::kvImage420Yp8_CbCr8,
            YuvKind::Planar420 { .. } => acc::kvImage420Yp8_Cb8_Cr8,
            YuvKind::Uyvy => acc::kvImage422CbYpCrYp8,
            YuvKind::Yuy2 => acc::kvImage422YpCbYpCr8,
        }
    }

    /// Chroma subsampling constraints vImage imposes on the frame size.
    fn requires_even(self) -> (bool, bool) {
        match self {
            YuvKind::Nv12 | YuvKind::Planar420 { .. } => (true, true),
            YuvKind::Uyvy | YuvKind::Yuy2 => (true, false),
        }
    }
}

/// The single vImage call a negotiated caps pair resolves to.
pub(super) enum Plan {
    RgbToYuv {
        info: acc::vImage_ARGBToYpCbCr,
        /// Gather map: `canonical_ARGB[i] = src_pixel[permute[i]]`.
        permute: [u8; 4],
        dest: YuvKind,
    },
    YuvToRgb {
        info: acc::vImage_YpCbCrToARGB,
        /// Scatter map: `dest_pixel[i] = canonical_ARGB[permute[i]]`.
        permute: [u8; 4],
        src: YuvKind,
    },
    /// Packed 8-bit four-channel to packed 8-bit four-channel.
    RgbPermute { permute: [u8; 4] },
    /// `I420`/`YV12` to `NV12`: copy Y, interleave the two chroma planes.
    ChromaInterleave { cb_plane: u32, cr_plane: u32 },
    /// `NV12` to `I420`/`YV12`: copy Y, split the chroma plane in two.
    ChromaDeinterleave { cb_plane: u32, cr_plane: u32 },
}

impl Plan {
    /// A short name for the chosen path, for the one log line per negotiation.
    pub(super) fn describe(&self) -> &'static str {
        match self {
            Plan::RgbToYuv { .. } => "RGB to Y'CbCr",
            Plan::YuvToRgb { .. } => "Y'CbCr to RGB",
            Plan::RgbPermute { .. } => "RGB channel permute",
            Plan::ChromaInterleave { .. } => "chroma interleave",
            Plan::ChromaDeinterleave { .. } => "chroma deinterleave",
        }
    }

    /// Decide whether vImage can perform this conversion.
    ///
    /// Returns `None` — meaning "use the `GstVideoConverter` fallback" — for
    /// a resize, an odd frame size against a subsampled format, a colour
    /// matrix or range vImage's generated conversions do not cover, and every
    /// format pair not enumerated below.
    pub(super) fn build(in_info: &VideoInfo, out_info: &VideoInfo) -> Option<Self> {
        // vImage's converters do not resize. A scaling stage stays on
        // `GstVideoConverter`, which fuses convert and scale in one pass.
        if in_info.width() != out_info.width() || in_info.height() != out_info.height() {
            return None;
        }
        // Interlaced content needs field-aware chroma handling that these
        // frame-at-a-time calls do not provide.
        if in_info.is_interlaced() || out_info.is_interlaced() {
            return None;
        }

        let src = in_info.format();
        let dst = out_info.format();
        let size_ok = |kind: YuvKind| {
            let (even_w, even_h) = kind.requires_even();
            !(even_w && !in_info.width().is_multiple_of(2)
                || even_h && !in_info.height().is_multiple_of(2))
        };

        match (rgb32_layout(src), yuv_kind(dst)) {
            // Packed RGB -> Y'CbCr, the headline path.
            (Some(layout), Some(kind)) if size_ok(kind) => {
                let range = yuv_pixel_range(out_info)?;
                let matrix = rgb_to_yuv_matrix(out_info)?;
                require_full_range_rgb(in_info)?;
                let mut info = acc::vImage_ARGBToYpCbCr { opaque: [0; 128] };
                // SAFETY: `matrix` is one of Accelerate's own static matrices,
                // `range` is a fully initialised value type, and `info` is a
                // correctly sized and aligned output slot.
                let err = unsafe {
                    acc::vImageConvert_ARGBToYpCbCr_GenerateConversion(
                        matrix,
                        &range,
                        &mut info,
                        acc::kvImageARGB8888,
                        kind.vimage_type(),
                        acc::kvImageNoFlags,
                    )
                };
                if err != acc::kvImageNoError {
                    return None;
                }
                return Some(Plan::RgbToYuv {
                    info,
                    permute: layout,
                    dest: kind,
                });
            }
            _ => {}
        }

        match (yuv_kind(src), rgb32_layout(dst)) {
            (Some(kind), Some(layout)) if size_ok(kind) => {
                let range = yuv_pixel_range(in_info)?;
                let matrix = yuv_to_rgb_matrix(in_info)?;
                require_full_range_rgb(out_info)?;
                let mut info = acc::vImage_YpCbCrToARGB { opaque: [0; 128] };
                // SAFETY: as above — static matrix, initialised range, sized
                // and aligned output slot.
                let err = unsafe {
                    acc::vImageConvert_YpCbCrToARGB_GenerateConversion(
                        matrix,
                        &range,
                        &mut info,
                        kind.vimage_type(),
                        acc::kvImageARGB8888,
                        acc::kvImageNoFlags,
                    )
                };
                if err != acc::kvImageNoError {
                    return None;
                }
                return Some(Plan::YuvToRgb {
                    info,
                    permute: invert_permute(layout),
                    src: kind,
                });
            }
            _ => {}
        }

        // The remaining paths move bytes without touching colour, so they are
        // only correct when both sides agree on range and matrix.
        if in_info.colorimetry() != out_info.colorimetry() {
            return None;
        }

        if let (Some(src_layout), Some(dst_layout)) = (rgb32_layout(src), rgb32_layout(dst)) {
            // A source without alpha cannot supply one: `videoconvert` writes
            // opaque 0xff there, and a straight permute would copy padding.
            if out_info.format_info().has_alpha() && !in_info.format_info().has_alpha() {
                return None;
            }
            return Some(Plan::RgbPermute {
                permute: permute_between(src_layout, dst_layout),
            });
        }

        match (yuv_kind(src), yuv_kind(dst)) {
            (Some(YuvKind::Planar420 { cb_plane, cr_plane }), Some(YuvKind::Nv12))
                if size_ok(YuvKind::Nv12) =>
            {
                Some(Plan::ChromaInterleave { cb_plane, cr_plane })
            }
            (Some(YuvKind::Nv12), Some(YuvKind::Planar420 { cb_plane, cr_plane }))
                if size_ok(YuvKind::Nv12) =>
            {
                Some(Plan::ChromaDeinterleave { cb_plane, cr_plane })
            }
            _ => None,
        }
    }
}

/// Byte offsets of A, R, G and B within one pixel of a packed 8-bit
/// four-channel format, or `None` if the format is not one.
///
/// The `x` variants are listed with their padding byte in the alpha slot:
/// on the RGB-to-Y'CbCr path alpha is never read, and on the reverse path
/// vImage writes a caller-supplied constant there.
fn rgb32_layout(format: VideoFormat) -> Option<[u8; 4]> {
    match format {
        VideoFormat::Rgba | VideoFormat::Rgbx => Some([3, 0, 1, 2]),
        VideoFormat::Bgra | VideoFormat::Bgrx => Some([3, 2, 1, 0]),
        VideoFormat::Argb | VideoFormat::Xrgb => Some([0, 1, 2, 3]),
        VideoFormat::Abgr | VideoFormat::Xbgr => Some([0, 3, 2, 1]),
        _ => None,
    }
}

fn yuv_kind(format: VideoFormat) -> Option<YuvKind> {
    match format {
        VideoFormat::Nv12 => Some(YuvKind::Nv12),
        VideoFormat::I420 => Some(YuvKind::Planar420 {
            cb_plane: 1,
            cr_plane: 2,
        }),
        // YV12 is I420 with the chroma planes swapped, so the same vImage
        // call works once the plane indices are exchanged.
        VideoFormat::Yv12 => Some(YuvKind::Planar420 {
            cb_plane: 2,
            cr_plane: 1,
        }),
        VideoFormat::Uyvy => Some(YuvKind::Uyvy),
        VideoFormat::Yuy2 => Some(YuvKind::Yuy2),
        _ => None,
    }
}

/// Invert a gather map into a scatter map (they are inverse permutations).
fn invert_permute(map: [u8; 4]) -> [u8; 4] {
    let mut out = [0u8; 4];
    for (canonical, &offset) in map.iter().enumerate() {
        out[offset as usize] = canonical as u8;
    }
    out
}

/// Build the map that `vImagePermuteChannels_ARGB8888` wants for a
/// source-to-destination channel reorder: `dest[i] = src[map[i]]`.
fn permute_between(src: [u8; 4], dst: [u8; 4]) -> [u8; 4] {
    let mut out = [0u8; 4];
    for canonical in 0..4 {
        out[dst[canonical] as usize] = src[canonical];
    }
    out
}

/// vImage's `ARGB8888` is full-range by definition, so a Y'CbCr conversion is
/// only equivalent to `videoconvert`'s when the RGB side is full-range too.
fn require_full_range_rgb(info: &VideoInfo) -> Option<()> {
    match info.colorimetry().range() {
        VideoColorRange::Range0_255 | VideoColorRange::Unknown => Some(()),
        _ => None,
    }
}

/// Map the caps' colour range onto vImage's pixel-range description.
///
/// The clamps are set wide open (`0..=255`) deliberately: `videoconvert` does
/// not pre-clamp to legal range either, so clamping here would be a visible
/// difference rather than a fidelity improvement.
fn yuv_pixel_range(info: &VideoInfo) -> Option<acc::vImage_YpCbCrPixelRange> {
    match info.colorimetry().range() {
        VideoColorRange::Range16_235 => Some(acc::vImage_YpCbCrPixelRange {
            Yp_bias: 16,
            CbCr_bias: 128,
            YpRangeMax: 235,
            CbCrRangeMax: 240,
            YpMax: 255,
            YpMin: 0,
            CbCrMax: 255,
            CbCrMin: 0,
        }),
        VideoColorRange::Range0_255 => Some(acc::vImage_YpCbCrPixelRange {
            Yp_bias: 0,
            CbCr_bias: 128,
            YpRangeMax: 255,
            CbCrRangeMax: 255,
            YpMax: 255,
            YpMin: 0,
            CbCrMax: 255,
            CbCrMin: 0,
        }),
        _ => None,
    }
}

fn rgb_to_yuv_matrix(info: &VideoInfo) -> Option<*const acc::vImage_ARGBToYpCbCrMatrix> {
    // SAFETY: reading the address of an Accelerate global. The pointees are
    // immutable framework constants that outlive the process.
    match info.colorimetry().matrix() {
        VideoColorMatrix::Bt601 => Some(unsafe { acc::kvImage_ARGBToYpCbCrMatrix_ITU_R_601_4 }),
        VideoColorMatrix::Bt709 => Some(unsafe { acc::kvImage_ARGBToYpCbCrMatrix_ITU_R_709_2 }),
        _ => None,
    }
}

fn yuv_to_rgb_matrix(info: &VideoInfo) -> Option<*const acc::vImage_YpCbCrToARGBMatrix> {
    // SAFETY: as above.
    match info.colorimetry().matrix() {
        VideoColorMatrix::Bt601 => Some(unsafe { acc::kvImage_YpCbCrToARGBMatrix_ITU_R_601_4 }),
        VideoColorMatrix::Bt709 => Some(unsafe { acc::kvImage_YpCbCrToARGBMatrix_ITU_R_709_2 }),
        _ => None,
    }
}

/// A `vImage_Buffer` pointing at one plane of a mapped frame.
///
/// `width` is in samples of that plane, not bytes, which is what every vImage
/// entry point expects; for the interleaved `CbCr` plane of NV12 a sample is
/// the Cb/Cr pair.
pub(super) fn plane_buffer(
    data: *mut u8,
    width: u32,
    height: u32,
    stride: i32,
) -> acc::vImage_Buffer {
    acc::vImage_Buffer {
        data: data as *mut std::ffi::c_void,
        height: height as acc::vImagePixelCount,
        width: width as acc::vImagePixelCount,
        rowBytes: stride as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gather_and_scatter_maps_are_inverses() {
        for format in [
            VideoFormat::Rgba,
            VideoFormat::Bgra,
            VideoFormat::Argb,
            VideoFormat::Abgr,
        ] {
            let gather = rgb32_layout(format).expect("packed 8-bit four-channel format");
            let scatter = invert_permute(gather);
            // Gathering then scattering must reproduce the original offsets.
            for canonical in 0..4 {
                assert_eq!(scatter[gather[canonical] as usize], canonical as u8);
            }
        }
    }

    /// The permute map is what stops BGRA arriving as RGBA with the channels
    /// rotated, so pin the two orders the compositor path actually uses.
    #[test]
    fn rgba_to_bgra_swaps_red_and_blue_and_keeps_alpha() {
        let rgba = rgb32_layout(VideoFormat::Rgba).unwrap();
        let bgra = rgb32_layout(VideoFormat::Bgra).unwrap();
        // dest[i] = src[map[i]]: B at dest 0 comes from src offset 2, and so on.
        assert_eq!(permute_between(rgba, bgra), [2, 1, 0, 3]);
        assert_eq!(permute_between(bgra, rgba), [2, 1, 0, 3]);
        // Identity must stay identity.
        assert_eq!(permute_between(rgba, rgba), [0, 1, 2, 3]);
    }

    #[test]
    fn yv12_is_i420_with_the_chroma_planes_exchanged() {
        assert_eq!(
            yuv_kind(VideoFormat::I420),
            Some(YuvKind::Planar420 {
                cb_plane: 1,
                cr_plane: 2
            })
        );
        assert_eq!(
            yuv_kind(VideoFormat::Yv12),
            Some(YuvKind::Planar420 {
                cb_plane: 2,
                cr_plane: 1
            })
        );
    }
}
