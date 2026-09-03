//! Executing a [`Plan`] against one pair of mapped frames.
//!
//! This is the per-buffer hot path. It does no allocation, takes no locks and
//! makes exactly one or two vImage calls; every decision was made in
//! [`Plan::build`](super::plan::Plan::build) at negotiation time.

use gstreamer as gst;
use gstreamer_video::prelude::*;
use gstreamer_video::VideoFrameRef;

use super::accelerate as acc;
use super::plan::{plane_buffer, Plan, YuvKind};

type SrcFrame<'a> = VideoFrameRef<&'a gst::BufferRef>;
type DstFrame<'a> = VideoFrameRef<&'a mut gst::BufferRef>;

/// Alpha written into packed RGB destinations. `videoconvert` writes opaque
/// for a Y'CbCr source too, so this matches it.
const OPAQUE_ALPHA: u8 = 255;

/// Read-only plane pointer plus its stride.
fn src_plane(frame: &SrcFrame<'_>, plane: u32) -> Option<(*mut u8, i32)> {
    let stride = *frame.plane_stride().get(plane as usize)?;
    let data = frame.plane_data(plane).ok()?;
    // Cast away const: every vImage_Buffer field is `void *`, and the source
    // descriptors are only ever passed to parameters the headers declare
    // `const vImage_Buffer *`.
    Some((data.as_ptr() as *mut u8, stride))
}

/// Writable plane pointer plus its stride.
///
/// The stride is read before the mutable borrow so both can be returned; the
/// borrow itself ends here, and the pointer stays valid for as long as the
/// mapped frame does.
fn dst_plane(frame: &mut DstFrame<'_>, plane: u32) -> Option<(*mut u8, i32)> {
    let stride = *frame.plane_stride().get(plane as usize)?;
    let data = frame.plane_data_mut(plane).ok()?;
    Some((data.as_mut_ptr(), stride))
}

/// The destination plane descriptors for a Y'CbCr layout, in the order the
/// matching vImage entry point wants them.
enum YuvPlanes {
    /// Y plus interleaved CbCr.
    Biplanar {
        y: acc::vImage_Buffer,
        cbcr: acc::vImage_Buffer,
    },
    /// Y plus separate Cb and Cr.
    Triplanar {
        y: acc::vImage_Buffer,
        cb: acc::vImage_Buffer,
        cr: acc::vImage_Buffer,
    },
    /// One packed 4:2:2 plane.
    Packed(acc::vImage_Buffer),
}

/// Build the plane descriptors for a Y'CbCr frame, whichever side it is on.
///
/// `plane` yields `(pointer, stride)` for a plane index; the caller supplies
/// the read or write variant.
fn yuv_planes(
    kind: YuvKind,
    width: u32,
    height: u32,
    mut plane: impl FnMut(u32) -> Option<(*mut u8, i32)>,
) -> Option<YuvPlanes> {
    let (y_ptr, y_stride) = plane(0)?;
    let y = plane_buffer(y_ptr, width, height, y_stride);
    match kind {
        YuvKind::Nv12 => {
            let (uv_ptr, uv_stride) = plane(1)?;
            Some(YuvPlanes::Biplanar {
                y,
                // One sample of this plane is a Cb/Cr pair, so its width is
                // half the luma width even though it occupies as many bytes.
                cbcr: plane_buffer(uv_ptr, width / 2, height / 2, uv_stride),
            })
        }
        YuvKind::Planar420 { cb_plane, cr_plane } => {
            let (cb_ptr, cb_stride) = plane(cb_plane)?;
            let (cr_ptr, cr_stride) = plane(cr_plane)?;
            Some(YuvPlanes::Triplanar {
                y,
                cb: plane_buffer(cb_ptr, width / 2, height / 2, cb_stride),
                cr: plane_buffer(cr_ptr, width / 2, height / 2, cr_stride),
            })
        }
        // 4:2:2 packed formats are a single plane; the luma descriptor built
        // above already names it with the full pixel width.
        YuvKind::Uyvy | YuvKind::Yuy2 => Some(YuvPlanes::Packed(y)),
    }
}

/// Run one frame through the planned vImage call.
///
/// Returns the vImage error code on failure so the caller can log it once and
/// push an error rather than silently emitting a garbage frame.
pub(super) fn run(
    plan: &Plan,
    inframe: &SrcFrame<'_>,
    outframe: &mut DstFrame<'_>,
) -> Result<(), acc::vImage_Error> {
    let width = inframe.width();
    let height = inframe.height();
    let missing = || -> acc::vImage_Error { acc::K_PLANE_UNAVAILABLE };

    match plan {
        Plan::RgbToYuv {
            info,
            permute,
            dest,
        } => {
            let (src_ptr, src_stride) = src_plane(inframe, 0).ok_or_else(missing)?;
            let src = plane_buffer(src_ptr, width, height, src_stride);
            let planes =
                yuv_planes(*dest, width, height, |p| dst_plane(outframe, p)).ok_or_else(missing)?;
            // SAFETY: every descriptor points into a frame this function holds
            // mapped, with the sample dimensions and stride GStreamer reports
            // for that plane. `info` was generated for exactly this Y'CbCr
            // layout in `Plan::build`, and `permute` is a permutation of 0..4.
            let err = unsafe {
                match planes {
                    YuvPlanes::Biplanar { y, cbcr } => acc::vImageConvert_ARGB8888To420Yp8_CbCr8(
                        &src,
                        &y,
                        &cbcr,
                        info,
                        permute.as_ptr(),
                        acc::kvImageNoFlags,
                    ),
                    YuvPlanes::Triplanar { y, cb, cr } => {
                        acc::vImageConvert_ARGB8888To420Yp8_Cb8_Cr8(
                            &src,
                            &y,
                            &cb,
                            &cr,
                            info,
                            permute.as_ptr(),
                            acc::kvImageNoFlags,
                        )
                    }
                    YuvPlanes::Packed(dst) if *dest == YuvKind::Uyvy => {
                        acc::vImageConvert_ARGB8888To422CbYpCrYp8(
                            &src,
                            &dst,
                            info,
                            permute.as_ptr(),
                            acc::kvImageNoFlags,
                        )
                    }
                    YuvPlanes::Packed(dst) => acc::vImageConvert_ARGB8888To422YpCbYpCr8(
                        &src,
                        &dst,
                        info,
                        permute.as_ptr(),
                        acc::kvImageNoFlags,
                    ),
                }
            };
            check(err)
        }

        Plan::YuvToRgb { info, permute, src } => {
            let (dst_ptr, dst_stride) = dst_plane(outframe, 0).ok_or_else(missing)?;
            let dst = plane_buffer(dst_ptr, width, height, dst_stride);
            let planes =
                yuv_planes(*src, width, height, |p| src_plane(inframe, p)).ok_or_else(missing)?;
            // SAFETY: as above, with the roles of the frames exchanged.
            let err = unsafe {
                match planes {
                    YuvPlanes::Biplanar { y, cbcr } => acc::vImageConvert_420Yp8_CbCr8ToARGB8888(
                        &y,
                        &cbcr,
                        &dst,
                        info,
                        permute.as_ptr(),
                        OPAQUE_ALPHA,
                        acc::kvImageNoFlags,
                    ),
                    YuvPlanes::Triplanar { y, cb, cr } => {
                        acc::vImageConvert_420Yp8_Cb8_Cr8ToARGB8888(
                            &y,
                            &cb,
                            &cr,
                            &dst,
                            info,
                            permute.as_ptr(),
                            OPAQUE_ALPHA,
                            acc::kvImageNoFlags,
                        )
                    }
                    YuvPlanes::Packed(packed) if *src == YuvKind::Uyvy => {
                        acc::vImageConvert_422CbYpCrYp8ToARGB8888(
                            &packed,
                            &dst,
                            info,
                            permute.as_ptr(),
                            OPAQUE_ALPHA,
                            acc::kvImageNoFlags,
                        )
                    }
                    YuvPlanes::Packed(packed) => acc::vImageConvert_422YpCbYpCr8ToARGB8888(
                        &packed,
                        &dst,
                        info,
                        permute.as_ptr(),
                        OPAQUE_ALPHA,
                        acc::kvImageNoFlags,
                    ),
                }
            };
            check(err)
        }

        Plan::RgbPermute { permute } => {
            let (src_ptr, src_stride) = src_plane(inframe, 0).ok_or_else(missing)?;
            let src = plane_buffer(src_ptr, width, height, src_stride);
            let (dst_ptr, dst_stride) = dst_plane(outframe, 0).ok_or_else(missing)?;
            let dst = plane_buffer(dst_ptr, width, height, dst_stride);
            // SAFETY: both descriptors name a mapped single-plane frame of the
            // same size, and `permute` is a permutation of 0..4.
            let err = unsafe {
                acc::vImagePermuteChannels_ARGB8888(
                    &src,
                    &dst,
                    permute.as_ptr(),
                    acc::kvImageNoFlags,
                )
            };
            check(err)
        }

        Plan::ChromaInterleave { cb_plane, cr_plane } => {
            copy_luma(inframe, outframe, width, height)?;
            let (cb_ptr, cb_stride) = src_plane(inframe, *cb_plane).ok_or_else(missing)?;
            let (cr_ptr, cr_stride) = src_plane(inframe, *cr_plane).ok_or_else(missing)?;
            let (uv_ptr, uv_stride) = dst_plane(outframe, 1).ok_or_else(missing)?;
            let cb = plane_buffer(cb_ptr, width / 2, height / 2, cb_stride);
            let cr = plane_buffer(cr_ptr, width / 2, height / 2, cr_stride);
            let sources: [*const acc::vImage_Buffer; 2] = [&cb, &cr];
            // SAFETY: `destChannels` names the first byte of each interleaved
            // component within the destination plane, which is why the second
            // entry is offset by one; `destStrideBytes` of 2 tells vImage how
            // far apart consecutive samples of one component are.
            let err = unsafe {
                let destinations: [*mut std::ffi::c_void; 2] = [
                    uv_ptr as *mut std::ffi::c_void,
                    uv_ptr.add(1) as *mut std::ffi::c_void,
                ];
                acc::vImageConvert_PlanarToChunky8(
                    sources.as_ptr(),
                    destinations.as_ptr(),
                    2,
                    2,
                    (width / 2) as acc::vImagePixelCount,
                    (height / 2) as acc::vImagePixelCount,
                    uv_stride as usize,
                    acc::kvImageNoFlags,
                )
            };
            check(err)
        }

        Plan::ChromaDeinterleave { cb_plane, cr_plane } => {
            copy_luma(inframe, outframe, width, height)?;
            let (uv_ptr, uv_stride) = src_plane(inframe, 1).ok_or_else(missing)?;
            let (cb_ptr, cb_stride) = dst_plane(outframe, *cb_plane).ok_or_else(missing)?;
            let (cr_ptr, cr_stride) = dst_plane(outframe, *cr_plane).ok_or_else(missing)?;
            let cb = plane_buffer(cb_ptr, width / 2, height / 2, cb_stride);
            let cr = plane_buffer(cr_ptr, width / 2, height / 2, cr_stride);
            let destinations: [*const acc::vImage_Buffer; 2] = [&cb, &cr];
            // SAFETY: mirror of the interleave case above.
            let err = unsafe {
                let sources: [*const std::ffi::c_void; 2] = [
                    uv_ptr as *const std::ffi::c_void,
                    uv_ptr.add(1) as *const std::ffi::c_void,
                ];
                acc::vImageConvert_ChunkyToPlanar8(
                    sources.as_ptr(),
                    destinations.as_ptr(),
                    2,
                    2,
                    (width / 2) as acc::vImagePixelCount,
                    (height / 2) as acc::vImagePixelCount,
                    uv_stride as usize,
                    acc::kvImageNoFlags,
                )
            };
            check(err)
        }
    }
}

/// Copy the luma plane unchanged, honouring a stride difference between the
/// two frames.
fn copy_luma(
    inframe: &SrcFrame<'_>,
    outframe: &mut DstFrame<'_>,
    width: u32,
    height: u32,
) -> Result<(), acc::vImage_Error> {
    let (src_ptr, src_stride) = src_plane(inframe, 0).ok_or(acc::K_PLANE_UNAVAILABLE)?;
    let (dst_ptr, dst_stride) = dst_plane(outframe, 0).ok_or(acc::K_PLANE_UNAVAILABLE)?;
    let src = plane_buffer(src_ptr, width, height, src_stride);
    let dst = plane_buffer(dst_ptr, width, height, dst_stride);
    // SAFETY: both descriptors name the mapped luma plane of a frame of this
    // size; a luma sample is one byte.
    check(unsafe { acc::vImageCopyBuffer(&src, &dst, 1, acc::kvImageNoFlags) })
}

fn check(err: acc::vImage_Error) -> Result<(), acc::vImage_Error> {
    if err == acc::kvImageNoError {
        Ok(())
    } else {
        Err(err)
    }
}
