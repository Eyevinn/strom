//! Correctness and negotiation tests for `stromvimageconvert`.
//!
//! The load-bearing test is [`vimage_output_matches_videoconvert`]: it pushes
//! the same frame through this element and through stock `videoconvert` and
//! compares the two outputs pixel for pixel. Everything about the vImage path
//! — permute maps, plane dimensions, colour matrices, pixel ranges — is only
//! as good as that comparison, and nothing else in the tree would catch a
//! channel swap or a half-height chroma plane.
//!
//! These tests need no element beyond `gst-plugins-base`, so they run in CI on
//! macOS runners without extra packages. They are macOS-only because the module
//! they test is.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use gstreamer_video::{VideoFormat, VideoInfo};

use super::imp::{PATH_FALLBACK, PATH_VIMAGE};
use super::plan::Plan;

fn init() {
    gst::init().expect("gst init");
    assert!(super::register(), "the vImage plugin must register");
}

/// Chroma is averaged over a 2x2 block, so `videoconvert` and vImage can
/// legitimately disagree by a rounding step; anything larger is a real bug.
/// Luma is a per-pixel matrix multiply, so it agrees far more tightly.
const MAX_DELTA: i32 = 2;

/// A deterministic, chroma-heavy test frame. Flat colour would hide a
/// red/blue swap against a bad permute map, and a smooth ramp would hide a
/// chroma plane written at the wrong stride, so this does both: hard vertical
/// colour bars over a diagonal luminance gradient.
fn fill_test_pattern(data: &mut [u8], width: usize, height: usize, stride: usize) {
    const BARS: [[u8; 3]; 8] = [
        [255, 255, 255],
        [255, 255, 0],
        [0, 255, 255],
        [0, 255, 0],
        [255, 0, 255],
        [255, 0, 0],
        [0, 0, 255],
        [16, 16, 16],
    ];
    for y in 0..height {
        for x in 0..width {
            let bar = BARS[(x * BARS.len()) / width];
            let shade = ((x + y) % 64) as u16;
            let px = y * stride + x * 4;
            for c in 0..3 {
                data[px + c] = ((bar[c] as u16 * (192 + shade)) / 255).min(255) as u8;
            }
            // A varying alpha would be dropped by every YUV target anyway;
            // keeping it varied still exercises the RGB permute path.
            data[px + 3] = (x % 256) as u8;
        }
    }
}

/// What one run of [`convert_one`] produced.
struct Converted {
    buffer: gst::Buffer,
    info: VideoInfo,
    /// The converter's `conversion-path` property, read while the caps were
    /// still negotiated. `None` for elements that do not have the property.
    path: Option<String>,
}

/// Run one buffer of `in_caps` through `element` and return the output buffer
/// together with the negotiated output info.
fn convert_one(
    element_name: &str,
    in_caps: &gst::Caps,
    out_caps: &gst::Caps,
    input: &gst::Buffer,
) -> Converted {
    let pipeline = gst::Pipeline::new();
    let src = gst_app::AppSrc::builder()
        .caps(in_caps)
        .format(gst::Format::Time)
        .build();
    let convert = gst::ElementFactory::make(element_name)
        .build()
        .unwrap_or_else(|e| panic!("{element_name} must be present: {e}"));
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", out_caps)
        .build()
        .expect("capsfilter");
    let sink = gst_app::AppSink::builder().sync(false).build();

    pipeline
        .add_many([
            src.upcast_ref::<gst::Element>(),
            &convert,
            &capsfilter,
            sink.upcast_ref(),
        ])
        .expect("add elements");
    gst::Element::link_many([
        src.upcast_ref::<gst::Element>(),
        &convert,
        &capsfilter,
        sink.upcast_ref(),
    ])
    .expect("link elements");

    pipeline.set_state(gst::State::Playing).expect("play");
    src.push_buffer(input.clone()).expect("push");
    src.end_of_stream().expect("eos");

    let sample = sink
        .try_pull_sample(gst::ClockTime::from_seconds(10))
        .unwrap_or_else(|| panic!("{element_name} produced no output"));
    let buffer = sample.buffer().expect("sample buffer").copy();
    let info = VideoInfo::from_caps(sample.caps().expect("sample caps")).expect("output info");
    // Read before NULL: `stop()` drops the negotiated state.
    let path = convert
        .has_property("conversion-path")
        .then(|| convert.property::<String>("conversion-path"));

    pipeline.set_state(gst::State::Null).expect("null");
    Converted { buffer, info, path }
}

/// Compare every plane of two frames of the same format, reporting where and
/// by how much they first diverge.
fn assert_frames_match(
    label: &str,
    info: &VideoInfo,
    mine: &gst::Buffer,
    reference: &gst::Buffer,
    max_delta: i32,
) {
    let mine =
        gst_video::VideoFrameRef::from_buffer_ref_readable(mine.as_ref(), info).expect("map ours");
    let theirs = gst_video::VideoFrameRef::from_buffer_ref_readable(reference.as_ref(), info)
        .expect("map reference");

    let mut worst = 0i32;
    let mut worst_at = (0u32, 0usize, 0usize);
    for plane in 0..info.n_planes() {
        let a = mine.plane_data(plane).expect("our plane");
        let b = theirs.plane_data(plane).expect("reference plane");
        let stride = mine.plane_stride()[plane as usize] as usize;
        let rows = mine.plane_height(plane) as usize;
        // Compare only the active bytes of each row; padding is undefined.
        let row_bytes = (info.width() as usize
            * info.format_info().pixel_stride()[plane as usize] as usize)
            .min(stride);
        for row in 0..rows {
            for col in 0..row_bytes {
                let idx = row * stride + col;
                if idx >= a.len() || idx >= b.len() {
                    continue;
                }
                let delta = (a[idx] as i32 - b[idx] as i32).abs();
                if delta > worst {
                    worst = delta;
                    worst_at = (plane, row, col);
                }
            }
        }
    }

    assert!(
        worst <= max_delta,
        "{label}: vImage and videoconvert differ by {worst} \
         (plane {}, row {}, byte {}), which is more than the {max_delta} a \
         chroma rounding difference can explain",
        worst_at.0,
        worst_at.1,
        worst_at.2
    );
}

fn packed_rgb_buffer(info: &VideoInfo) -> gst::Buffer {
    let mut buffer = gst::Buffer::with_size(info.size()).expect("allocate");
    {
        let buffer = buffer.get_mut().unwrap();
        let mut map = buffer.map_writable().expect("map writable");
        let stride = info.stride()[0] as usize;
        fill_test_pattern(
            map.as_mut_slice(),
            info.width() as usize,
            info.height() as usize,
            stride,
        );
    }
    gst_video::VideoMeta::add(
        buffer.get_mut().unwrap(),
        gst_video::VideoFrameFlags::empty(),
        info.format(),
        info.width(),
        info.height(),
    )
    .ok();
    buffer
}

/// Every format pair the vImage path claims must produce what `videoconvert`
/// produces. A wrong permute map, a chroma plane described at the wrong size,
/// or the wrong colour matrix all show up here and nowhere else.
#[test]
fn vimage_output_matches_videoconvert() {
    init();

    // Second half of each pair is what a real flow asks for; the RGB source
    // formats are the ones Strom's HTML and compositor paths produce.
    let pairs: &[(VideoFormat, VideoFormat)] = &[
        (VideoFormat::Rgba, VideoFormat::Nv12),
        (VideoFormat::Rgba, VideoFormat::I420),
        (VideoFormat::Bgra, VideoFormat::Nv12),
        (VideoFormat::Bgra, VideoFormat::I420),
        (VideoFormat::Bgra, VideoFormat::Yv12),
        (VideoFormat::Rgba, VideoFormat::Uyvy),
        (VideoFormat::Rgba, VideoFormat::Yuy2),
        (VideoFormat::Rgba, VideoFormat::Bgra),
        (VideoFormat::Bgrx, VideoFormat::Rgbx),
        (VideoFormat::I420, VideoFormat::Nv12),
        (VideoFormat::Nv12, VideoFormat::I420),
    ];

    // 4:2:0 needs even dimensions, and a non-square frame catches width and
    // height being transposed in a plane descriptor.
    let (width, height) = (128, 64);

    for &(src, dst) in pairs {
        let in_info = VideoInfo::builder(src, width, height)
            .fps(gst::Fraction::new(30, 1))
            .build()
            .expect("input info");
        let out_info = VideoInfo::builder(dst, width, height)
            .fps(gst::Fraction::new(30, 1))
            .build()
            .expect("output info");

        assert!(
            Plan::build(&in_info, &out_info).is_some(),
            "{src:?} -> {dst:?} is listed here as a vImage pair but Plan::build declined it"
        );

        let in_caps = in_info.to_caps().expect("input caps");
        let out_caps = out_info.to_caps().expect("output caps");
        let input = source_buffer(&in_info);

        let ours = convert_one(super::ELEMENT_NAME, &in_caps, &out_caps, &input);
        let reference = convert_one("videoconvert", &in_caps, &out_caps, &input);

        // Without this the comparison is vacuous: if negotiation quietly chose
        // the fallback, both sides would be running GstVideoConverter and the
        // pixels would match no matter how wrong the vImage code was.
        assert_eq!(
            ours.path.as_deref(),
            Some(PATH_VIMAGE),
            "{src:?} -> {dst:?} did not take the vImage path"
        );

        assert_frames_match(
            &format!("{src:?} -> {dst:?}"),
            &ours.info,
            &ours.buffer,
            &reference.buffer,
            MAX_DELTA,
        );
    }
}

/// Build the input frame for a pair, generating the non-RGB sources by letting
/// `videoconvert` produce them from the RGBA pattern.
fn source_buffer(info: &VideoInfo) -> gst::Buffer {
    if info.format_info().is_rgb() {
        return packed_rgb_buffer(info);
    }

    let rgba = VideoInfo::builder(VideoFormat::Rgba, info.width(), info.height())
        .fps(gst::Fraction::new(30, 1))
        .build()
        .expect("rgba info");
    convert_one(
        "videoconvert",
        &rgba.to_caps().expect("rgba caps"),
        &info.to_caps().expect("caps"),
        &packed_rgb_buffer(&rgba),
    )
    .buffer
}

/// A pair with no vImage path must still convert, through the fallback. If
/// this ever fails the element has stopped being a drop-in replacement.
#[test]
fn unsupported_pair_falls_back_and_still_converts() {
    init();

    // 10-bit 4:2:2 has no vImage entry point in this module, and GRAY8 has no
    // colour at all — both must land on GstVideoConverter.
    for target in [VideoFormat::V210, VideoFormat::Gray8] {
        let in_info = VideoInfo::builder(VideoFormat::Rgba, 128, 64)
            .fps(gst::Fraction::new(30, 1))
            .build()
            .expect("input info");
        let out_info = VideoInfo::builder(target, 128, 64)
            .fps(gst::Fraction::new(30, 1))
            .build()
            .expect("output info");

        assert!(
            Plan::build(&in_info, &out_info).is_none(),
            "{target:?} is not a vImage path and must not claim to be one"
        );

        let ours = convert_one(
            super::ELEMENT_NAME,
            &in_info.to_caps().expect("in caps"),
            &out_info.to_caps().expect("out caps"),
            &packed_rgb_buffer(&in_info),
        );
        let reference = convert_one(
            "videoconvert",
            &in_info.to_caps().expect("in caps"),
            &out_info.to_caps().expect("out caps"),
            &packed_rgb_buffer(&in_info),
        );

        assert_eq!(ours.path.as_deref(), Some(PATH_FALLBACK));

        // The fallback *is* GstVideoConverter, so this should be exact.
        assert_frames_match(
            &format!("fallback RGBA -> {target:?}"),
            &ours.info,
            &ours.buffer,
            &reference.buffer,
            0,
        );
    }
}

/// A resize has no vImage path, but the element still has to do it — this is
/// the case `videoformat.rs` hits whenever a resolution is set.
#[test]
fn resize_goes_through_the_fallback_and_produces_the_requested_size() {
    init();

    let in_info = VideoInfo::builder(VideoFormat::Rgba, 128, 64)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .expect("input info");
    let out_info = VideoInfo::builder(VideoFormat::I420, 64, 32)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .expect("output info");

    assert!(
        Plan::build(&in_info, &out_info).is_none(),
        "a resize must not take the vImage path"
    );

    let ours = convert_one(
        super::ELEMENT_NAME,
        &in_info.to_caps().expect("in caps"),
        &out_info.to_caps().expect("out caps"),
        &packed_rgb_buffer(&in_info),
    );
    assert_eq!(ours.path.as_deref(), Some(PATH_FALLBACK));
    assert_eq!((ours.info.width(), ours.info.height()), (64, 32));
    assert_eq!(ours.info.format(), VideoFormat::I420);
}

/// `gpu::configure_video_convert` reaches the element only through
/// `has_property("n-threads")`. Losing that property would silently drop the
/// fallback back to single-threaded conversion.
#[test]
fn element_exposes_n_threads_for_configure_video_convert() {
    init();

    let element = gst::ElementFactory::make(super::ELEMENT_NAME)
        .build()
        .expect("element must be registered");
    assert!(element.has_property("n-threads"));
    assert_eq!(
        element.property::<u32>("n-threads"),
        1,
        "the default must match videoconvert's, or configure_video_convert \
         would be raising it from a different baseline"
    );

    crate::gpu::configure_video_convert(&element);
    assert_eq!(
        element.property::<u32>("n-threads"),
        crate::gpu::video_convert_threads()
    );
}

/// Negotiation must prefer leaving the format alone. If this regresses, an
/// element asked only to pass frames through starts converting them.
#[test]
fn identical_caps_negotiate_without_conversion() {
    init();

    let info = VideoInfo::builder(VideoFormat::Nv12, 128, 64)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .expect("info");
    let caps = info.to_caps().expect("caps");

    let ours = convert_one(
        super::ELEMENT_NAME,
        &caps,
        &gst::Caps::builder("video/x-raw").build(),
        &source_buffer(&info),
    );
    assert_eq!(ours.info.format(), VideoFormat::Nv12);
    assert_eq!((ours.info.width(), ours.info.height()), (128, 64));
}
