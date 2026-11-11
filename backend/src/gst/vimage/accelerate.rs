//! Raw FFI declarations for the vImage entry points this element uses.
//!
//! Only the handful of symbols needed by [`super::plan`] are declared. The
//! shapes are transcribed from the macOS SDK headers
//! (`Accelerate.framework/Frameworks/vImage.framework/Headers/{Conversion,vImage_Types}.h`);
//! the layout of every struct here is fixed public ABI.
//!
//! Everything in this module is `unsafe` to call. The safe wrappers live in
//! [`super::plan`], which is the only place allowed to build the buffer
//! descriptors.

#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case)]

use std::ffi::c_void;

/// `vImagePixelCount` — `unsigned long`, 64-bit on every Mac we build for.
pub type vImagePixelCount = usize;
/// `vImage_Error` — `ssize_t`. Negative values are errors, `0` is success.
pub type vImage_Error = isize;
/// `vImage_Flags` — `uint32_t`.
pub type vImage_Flags = u32;

/// Default behaviour, which includes vImage's own internal multithreading.
/// That is where the speed comes from: vImage tiles the image across the
/// cores itself, so the element does not manage a pool of its own.
pub const kvImageNoFlags: vImage_Flags = 0;
pub const kvImageNoError: vImage_Error = 0;

/// Not a vImage code: this element's own marker for "GStreamer would not hand
/// us a plane we expected", so the one error path can carry a single type.
/// Kept clear of vImage's own range, which runs from -21772 downwards.
pub const K_PLANE_UNAVAILABLE: vImage_Error = -1;

/// `vImageARGBType::kvImageARGB8888` — any 8-bit four-channel interleaved
/// buffer. Channel order is carried by the permute map, not by this value.
pub const kvImageARGB8888: u32 = 0;

/// `vImageYpCbCrType` discriminants for the four Y'CbCr layouts we map onto.
pub const kvImage422CbYpCrYp8: u32 = 0; // UYVY
pub const kvImage422YpCbYpCr8: u32 = 1; // YUY2
pub const kvImage420Yp8_Cb8_Cr8: u32 = 3; // I420 / YV12
pub const kvImage420Yp8_CbCr8: u32 = 4; // NV12

#[repr(C)]
#[derive(Clone, Copy)]
pub struct vImage_Buffer {
    pub data: *mut c_void,
    pub height: vImagePixelCount,
    pub width: vImagePixelCount,
    pub rowBytes: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct vImage_YpCbCrPixelRange {
    pub Yp_bias: i32,
    pub CbCr_bias: i32,
    pub YpRangeMax: i32,
    pub CbCrRangeMax: i32,
    pub YpMax: i32,
    pub YpMin: i32,
    pub CbCrMax: i32,
    pub CbCrMin: i32,
}

/// Opaque 128-byte, 16-byte-aligned conversion state. Apple documents these as
/// reusable across threads once generated, which is why the element builds one
/// per caps negotiation and then shares it for every frame.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct vImage_ARGBToYpCbCr {
    pub opaque: [u8; 128],
}

#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct vImage_YpCbCrToARGB {
    pub opaque: [u8; 128],
}

// The generated conversion state is plain bytes with no interior pointers into
// caller memory, and Apple documents it as safe to use concurrently from
// several threads. That is what lets `State` sit behind a shared reference
// while streaming threads read it.
unsafe impl Send for vImage_ARGBToYpCbCr {}
unsafe impl Sync for vImage_ARGBToYpCbCr {}
unsafe impl Send for vImage_YpCbCrToARGB {}
unsafe impl Sync for vImage_YpCbCrToARGB {}

#[repr(C)]
pub struct vImage_ARGBToYpCbCrMatrix {
    _private: [f32; 8],
}

#[repr(C)]
pub struct vImage_YpCbCrToARGBMatrix {
    _private: [f32; 5],
}

#[link(name = "Accelerate", kind = "framework")]
extern "C" {
    pub static kvImage_ARGBToYpCbCrMatrix_ITU_R_601_4: *const vImage_ARGBToYpCbCrMatrix;
    pub static kvImage_ARGBToYpCbCrMatrix_ITU_R_709_2: *const vImage_ARGBToYpCbCrMatrix;
    pub static kvImage_YpCbCrToARGBMatrix_ITU_R_601_4: *const vImage_YpCbCrToARGBMatrix;
    pub static kvImage_YpCbCrToARGBMatrix_ITU_R_709_2: *const vImage_YpCbCrToARGBMatrix;

    pub fn vImageConvert_ARGBToYpCbCr_GenerateConversion(
        matrix: *const vImage_ARGBToYpCbCrMatrix,
        pixelRange: *const vImage_YpCbCrPixelRange,
        outInfo: *mut vImage_ARGBToYpCbCr,
        inARGBType: u32,
        outYpCbCrType: u32,
        flags: vImage_Flags,
    ) -> vImage_Error;

    pub fn vImageConvert_YpCbCrToARGB_GenerateConversion(
        matrix: *const vImage_YpCbCrToARGBMatrix,
        pixelRange: *const vImage_YpCbCrPixelRange,
        outInfo: *mut vImage_YpCbCrToARGB,
        inYpCbCrType: u32,
        outARGBType: u32,
        flags: vImage_Flags,
    ) -> vImage_Error;

    // RGB -> Y'CbCr
    pub fn vImageConvert_ARGB8888To420Yp8_CbCr8(
        src: *const vImage_Buffer,
        destYp: *const vImage_Buffer,
        destCbCr: *const vImage_Buffer,
        info: *const vImage_ARGBToYpCbCr,
        permuteMap: *const u8,
        flags: vImage_Flags,
    ) -> vImage_Error;

    pub fn vImageConvert_ARGB8888To420Yp8_Cb8_Cr8(
        src: *const vImage_Buffer,
        destYp: *const vImage_Buffer,
        destCb: *const vImage_Buffer,
        destCr: *const vImage_Buffer,
        info: *const vImage_ARGBToYpCbCr,
        permuteMap: *const u8,
        flags: vImage_Flags,
    ) -> vImage_Error;

    pub fn vImageConvert_ARGB8888To422CbYpCrYp8(
        src: *const vImage_Buffer,
        dest: *const vImage_Buffer,
        info: *const vImage_ARGBToYpCbCr,
        permuteMap: *const u8,
        flags: vImage_Flags,
    ) -> vImage_Error;

    pub fn vImageConvert_ARGB8888To422YpCbYpCr8(
        src: *const vImage_Buffer,
        dest: *const vImage_Buffer,
        info: *const vImage_ARGBToYpCbCr,
        permuteMap: *const u8,
        flags: vImage_Flags,
    ) -> vImage_Error;

    // Y'CbCr -> RGB
    pub fn vImageConvert_420Yp8_CbCr8ToARGB8888(
        srcYp: *const vImage_Buffer,
        srcCbCr: *const vImage_Buffer,
        dest: *const vImage_Buffer,
        info: *const vImage_YpCbCrToARGB,
        permuteMap: *const u8,
        alpha: u8,
        flags: vImage_Flags,
    ) -> vImage_Error;

    pub fn vImageConvert_420Yp8_Cb8_Cr8ToARGB8888(
        srcYp: *const vImage_Buffer,
        srcCb: *const vImage_Buffer,
        srcCr: *const vImage_Buffer,
        dest: *const vImage_Buffer,
        info: *const vImage_YpCbCrToARGB,
        permuteMap: *const u8,
        alpha: u8,
        flags: vImage_Flags,
    ) -> vImage_Error;

    pub fn vImageConvert_422CbYpCrYp8ToARGB8888(
        src: *const vImage_Buffer,
        dest: *const vImage_Buffer,
        info: *const vImage_YpCbCrToARGB,
        permuteMap: *const u8,
        alpha: u8,
        flags: vImage_Flags,
    ) -> vImage_Error;

    pub fn vImageConvert_422YpCbYpCr8ToARGB8888(
        src: *const vImage_Buffer,
        dest: *const vImage_Buffer,
        info: *const vImage_YpCbCrToARGB,
        permuteMap: *const u8,
        alpha: u8,
        flags: vImage_Flags,
    ) -> vImage_Error;

    // Channel shuffling and plane interleaving
    pub fn vImagePermuteChannels_ARGB8888(
        src: *const vImage_Buffer,
        dest: *const vImage_Buffer,
        permuteMap: *const u8,
        flags: vImage_Flags,
    ) -> vImage_Error;

    pub fn vImageConvert_ChunkyToPlanar8(
        srcChannels: *const *const c_void,
        destPlanarBuffers: *const *const vImage_Buffer,
        channelCount: u32,
        srcStrideBytes: usize,
        srcWidth: vImagePixelCount,
        srcHeight: vImagePixelCount,
        srcRowBytes: usize,
        flags: vImage_Flags,
    ) -> vImage_Error;

    pub fn vImageConvert_PlanarToChunky8(
        srcPlanarBuffers: *const *const vImage_Buffer,
        destChannels: *const *mut c_void,
        channelCount: u32,
        destStrideBytes: usize,
        destWidth: vImagePixelCount,
        destHeight: vImagePixelCount,
        destRowBytes: usize,
        flags: vImage_Flags,
    ) -> vImage_Error;

    pub fn vImageCopyBuffer(
        src: *const vImage_Buffer,
        dest: *const vImage_Buffer,
        pixelSize: usize,
        flags: vImage_Flags,
    ) -> vImage_Error;
}
