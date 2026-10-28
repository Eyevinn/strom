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

use super::imp::{PATH_FALLBACK, PATH_PASSTHROUGH, PATH_VIMAGE};
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
        // Through caps, as the element receives them: that fills in the
        // default chroma siting, which the builder leaves unknown.
        let in_info = VideoInfo::from_caps(&in_info.to_caps().expect("in caps")).expect("in info");
        let out_info =
            VideoInfo::from_caps(&out_info.to_caps().expect("out caps")).expect("out info");

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

/// The reverse direction, decoded Y'CbCr into RGB, checked against the colours
/// the frame was made from rather than against `videoconvert`: the two
/// upsample chroma differently (vImage repeats each sample, `videoconvert`
/// interpolates for some formats and not others), so they part by tens of
/// levels at every hard colour edge while both being right. Flat colours have
/// no edges, and a wrong permute map or chroma plane still misses by far more
/// than rounding.
#[test]
fn yuv_to_rgb_recovers_the_source_colours() {
    init();

    const COLOURS: [[u8; 3]; 5] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [200, 150, 40],
        [128, 128, 128],
    ];
    const MAX_ERROR: i32 = 3;
    let (width, height) = (128, 64);

    let pairs: &[(VideoFormat, VideoFormat)] = &[
        (VideoFormat::Nv12, VideoFormat::Rgba),
        (VideoFormat::Nv12, VideoFormat::Bgra),
        (VideoFormat::I420, VideoFormat::Argb),
        (VideoFormat::Yv12, VideoFormat::Abgr),
        (VideoFormat::Uyvy, VideoFormat::Bgrx),
        (VideoFormat::Yuy2, VideoFormat::Rgba),
    ];

    for &(src, dst) in pairs {
        let in_info = VideoInfo::builder(src, width, height)
            .fps(gst::Fraction::new(30, 1))
            .build()
            .expect("input info");
        let out_info = VideoInfo::builder(dst, width, height)
            .fps(gst::Fraction::new(30, 1))
            .build()
            .expect("output info");

        for colour in COLOURS {
            let ours = convert_one(
                super::ELEMENT_NAME,
                &in_info.to_caps().expect("input caps"),
                &out_info.to_caps().expect("output caps"),
                &flat_source(&in_info, colour),
            );
            assert_eq!(
                ours.path.as_deref(),
                Some(PATH_VIMAGE),
                "{src:?} -> {dst:?} did not take the vImage path"
            );

            let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(
                ours.buffer.as_ref(),
                &ours.info,
            )
            .expect("map output");
            let data = frame.plane_data(0).expect("packed plane");
            let stride = frame.plane_stride()[0] as usize;
            let format_info = ours.info.format_info();
            let offsets = format_info.poffset();
            for y in 0..height as usize {
                for x in 0..width as usize {
                    let px = y * stride + x * 4;
                    for (c, &want) in colour.iter().enumerate() {
                        let got = data[px + offsets[c] as usize];
                        assert!(
                            (got as i32 - want as i32).abs() <= MAX_ERROR,
                            "{src:?} -> {dst:?}: {colour:?} came back with component {c} \
                             = {got} at ({x}, {y})"
                        );
                    }
                }
            }
        }
    }
}

/// A flat frame of one RGB colour in `info`'s format, made by `videoconvert`.
fn flat_source(info: &VideoInfo, colour: [u8; 3]) -> gst::Buffer {
    let rgba = VideoInfo::builder(VideoFormat::Rgba, info.width(), info.height())
        .fps(gst::Fraction::new(30, 1))
        .build()
        .expect("rgba info");
    let mut buffer = gst::Buffer::with_size(rgba.size()).expect("allocate");
    {
        let mut map = buffer.get_mut().unwrap().map_writable().expect("map");
        for px in map.as_mut_slice().as_chunks_mut::<4>().0 {
            px[..3].copy_from_slice(&colour);
            px[3] = 255;
        }
    }
    convert_one(
        "videoconvert",
        &rgba.to_caps().expect("rgba caps"),
        &info.to_caps().expect("caps"),
        &buffer,
    )
    .buffer
}

/// At HD sizes, caps without a `chroma-site` field mean left-cosited chroma,
/// which vImage does not write. The element must label its output with the
/// centred siting it produces, and then match `videoconvert` given that label.
/// RGB has no chroma, so a `chroma-site` on the input caps changes nothing.
#[test]
fn hd_output_is_labelled_with_the_siting_vimage_writes() {
    init();

    let in_info = VideoInfo::builder(VideoFormat::Rgba, 1920, 1080)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .expect("input info");
    let unlabelled = in_info.to_caps().expect("input caps");
    let mut labelled = unlabelled.clone();
    labelled.make_mut().set("chroma-site", "mpeg2");
    let input = packed_rgb_buffer(&in_info);

    for (in_caps, dst) in [&unlabelled, &labelled].into_iter().flat_map(|caps| {
        [VideoFormat::Nv12, VideoFormat::I420, VideoFormat::Uyvy].map(|dst| (caps, dst))
    }) {
        let ours = convert_one(
            super::ELEMENT_NAME,
            in_caps,
            &gst::Caps::builder("video/x-raw")
                .field("format", dst.to_str())
                .build(),
            &input,
        );
        assert_eq!(
            ours.info.chroma_site(),
            gst_video::VideoChromaSite::JPEG,
            "RGBA -> {dst:?} at 1080p from {in_caps} was not labelled with centred chroma"
        );
        assert_eq!(ours.path.as_deref(), Some(PATH_VIMAGE), "from {in_caps}");

        let out_caps = ours.info.to_caps().expect("output caps");
        let reference = convert_one("videoconvert", in_caps, &out_caps, &input);
        assert_frames_match(
            &format!("Rgba -> {dst:?} at 1080p"),
            &ours.info,
            &ours.buffer,
            &reference.buffer,
            MAX_DELTA,
        );
    }
}

/// A siting the peer insists on is not vImage's, so it has to go to the
/// fallback, which resamples to it.
#[test]
fn a_pinned_cosited_siting_takes_the_fallback() {
    init();

    let in_info = VideoInfo::builder(VideoFormat::Rgba, 1920, 1080)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .expect("input info");
    let ours = convert_one(
        super::ELEMENT_NAME,
        &in_info.to_caps().expect("input caps"),
        &gst::Caps::builder("video/x-raw")
            .field("format", "NV12")
            .field("chroma-site", "mpeg2")
            .build(),
        &packed_rgb_buffer(&in_info),
    );
    assert_eq!(ours.info.chroma_site(), gst_video::VideoChromaSite::MPEG2);
    assert_eq!(ours.path.as_deref(), Some(PATH_FALLBACK));
}

/// The 4:2:0 shuffles copy chroma bytes as they are, so the output must keep
/// the input's siting, and a peer that insists on another one has to get the
/// fallback. Centred input at 1080p, where the default is cosited, so keeping
/// the input's siting and taking the default cannot be confused.
#[test]
fn a_chroma_copy_keeps_the_input_siting() {
    init();

    let in_info = VideoInfo::builder(VideoFormat::I420, 1920, 1080)
        .fps(gst::Fraction::new(30, 1))
        .chroma_site(gst_video::VideoChromaSite::JPEG)
        .build()
        .expect("input info");
    let in_caps = in_info.to_caps().expect("input caps");
    let input = source_buffer(&in_info);

    let unconstrained = gst::Caps::builder("video/x-raw")
        .field("format", "NV12")
        .build();
    let ours = convert_one(super::ELEMENT_NAME, &in_caps, &unconstrained, &input);
    assert_eq!(ours.info.chroma_site(), gst_video::VideoChromaSite::JPEG);
    assert_eq!(ours.path.as_deref(), Some(PATH_VIMAGE));

    let cosited = gst::Caps::builder("video/x-raw")
        .field("format", "NV12")
        .field("chroma-site", "mpeg2")
        .build();
    let ours = convert_one(super::ELEMENT_NAME, &in_caps, &cosited, &input);
    assert_eq!(ours.info.chroma_site(), gst_video::VideoChromaSite::MPEG2);
    assert_eq!(ours.path.as_deref(), Some(PATH_FALLBACK));
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

/// Same caps on both sides must go straight through, whatever the input
/// carries. If this regresses, an element asked only to pass frames through
/// starts converting them, and `conversion-path` names an engine that is doing
/// nothing.
#[test]
fn identical_caps_pass_straight_through() {
    init();

    let progressive = |format, width, height| {
        VideoInfo::builder(format, width, height)
            .fps(gst::Fraction::new(30, 1))
            .build()
            .expect("info")
    };
    let cases = [
        (progressive(VideoFormat::Nv12, 128, 64), None),
        (progressive(VideoFormat::I420, 1920, 1080), None),
        // RGB caps naming a siting: the label must not force a conversion.
        (
            progressive(VideoFormat::Rgba, 1920, 1080),
            Some(("chroma-site", "mpeg2")),
        ),
        (
            progressive(VideoFormat::Uyvy, 1920, 1080),
            Some(("interlace-mode", "interleaved")),
        ),
    ];
    for (info, extra) in cases {
        let mut caps = info.to_caps().expect("caps");
        if let Some((field, value)) = extra {
            caps.make_mut()
                .structure_mut(0)
                .expect("structure")
                .set(field, value);
        }
        let ours = convert_one(
            super::ELEMENT_NAME,
            &caps,
            &gst::Caps::builder("video/x-raw").build(),
            &source_buffer(&info),
        );
        assert_eq!(ours.info.format(), info.format(), "{caps}");
        assert_eq!(
            ours.path.as_deref(),
            Some(PATH_PASSTHROUGH),
            "{caps} was converted instead of passed through"
        );
    }
}

/// Run a `gst::parse::launch` description with `CONVERT` standing for the
/// element under test, named `convert`, and an appsink named `sink`. Returns
/// the first sample, its caps, and the conversion path.
fn launch_one(label: &str, launch: &str) -> (Option<gst::Sample>, String) {
    let pipeline = gst::parse::launch(&launch.replace("CONVERT", super::ELEMENT_NAME))
        .unwrap_or_else(|e| panic!("{label}: {e}"))
        .downcast::<gst::Pipeline>()
        .expect("pipeline");
    let convert = pipeline.by_name("convert").expect("convert");
    let sink = pipeline
        .by_name("sink")
        .expect("sink")
        .downcast::<gst_app::AppSink>()
        .expect("appsink");

    pipeline.set_state(gst::State::Playing).expect("play");
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(10));
    // Read before NULL: `stop()` drops the negotiated state.
    let path = convert.property::<String>("conversion-path");
    pipeline.set_state(gst::State::Null).expect("null");
    (sample, path)
}

/// What an AVFoundation camera sends: format, size and rate, with no pixel
/// aspect ratio, interlace mode or colorimetry. `capssetter` strips the caps
/// to that.
const CAMERA: &str =
    "videotestsrc num-buffers=1 ! video/x-raw,format=NV12,width=1920,height=1080 ! \
     capssetter replace=true \
       caps=\"video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1\"";

/// Caps a source leaves incomplete must still pass straight through.
/// Filling in a field on the output side makes the caps differ, and a copy of
/// every frame, in system memory, or through the CPU for GL memory, would
/// follow unless the frames are recognised as the same.
#[test]
fn input_without_pixel_aspect_ratio_passes_through() {
    init();

    for (label, launch) in [
        (
            "system memory",
            format!("{CAMERA} ! CONVERT name=convert ! video/x-raw ! appsink name=sink sync=false"),
        ),
        (
            "GL memory",
            "videotestsrc num-buffers=1 ! video/x-raw,format=RGBA,width=64,height=64 ! glupload ! \
             capssetter replace=true caps=\"video/x-raw(memory:GLMemory),format=RGBA,\
               width=64,height=64,framerate=30/1,texture-target=2D\" ! \
             CONVERT name=convert ! \
             video/x-raw(memory:GLMemory),pixel-aspect-ratio=[1/2,2/1] ! \
             gldownload ! video/x-raw,format=RGBA ! appsink name=sink sync=false"
                .to_string(),
        ),
    ] {
        let (sample, path) = launch_one(label, &launch);
        assert!(sample.is_some(), "{label}: no output");
        assert_eq!(
            path, PATH_PASSTHROUGH,
            "{label} without a pixel aspect ratio was converted"
        );
    }
}

/// A consumer that requires a field the input left out must receive it,
/// written into the output caps, as `videoconvert` does. Where the field's
/// value is the input's default the frames still pass through; where it
/// differs they are converted.
#[test]
fn a_field_the_peer_requires_is_written_into_the_output() {
    init();

    for (field, value, expected_path) in [
        ("pixel-aspect-ratio", "1/1", PATH_PASSTHROUGH),
        ("interlace-mode", "progressive", PATH_PASSTHROUGH),
        // 1080p without colorimetry means bt709.
        ("colorimetry", "bt709", PATH_PASSTHROUGH),
        ("colorimetry", "bt601", PATH_FALLBACK),
    ] {
        let label = format!("{field}={value}");
        let (sample, path) = launch_one(
            &label,
            &format!(
                "{CAMERA} ! CONVERT name=convert ! \
                 appsink name=sink sync=false caps=\"video/x-raw,{field}={value}\""
            ),
        );
        let sample = sample.unwrap_or_else(|| panic!("{label}: no output"));
        let caps = sample.caps().expect("caps");
        let written = caps
            .structure(0)
            .and_then(|s| s.value(field).ok())
            .map(|v| v.serialize().expect("serialize").to_string());
        assert_eq!(written.as_deref(), Some(value), "{label}: output {caps}");
        assert_eq!(path, expected_path, "{label}");
    }
}

/// Interlaced frames need field-aware chroma handling that vImage's
/// frame-at-a-time calls lack, so every pair, including those vImage has a
/// path for when progressive, must run on the fallback and match
/// `videoconvert` exactly.
#[test]
fn interlaced_input_takes_the_fallback() {
    init();

    for (src, dst) in [
        (VideoFormat::I420, VideoFormat::Nv12),
        (VideoFormat::Nv12, VideoFormat::Rgba),
        (VideoFormat::Uyvy, VideoFormat::Bgra),
        (VideoFormat::Rgba, VideoFormat::I420),
    ] {
        let progressive = VideoInfo::builder(src, 1920, 1080)
            .fps(gst::Fraction::new(30, 1))
            .build()
            .expect("info");
        let mut in_caps = progressive.to_caps().expect("caps");
        in_caps
            .make_mut()
            .structure_mut(0)
            .expect("structure")
            .set("interlace-mode", "interleaved");
        let input = source_buffer(&progressive);
        let out_caps = gst::Caps::builder("video/x-raw")
            .field("format", dst.to_str())
            .build();

        let ours = convert_one(super::ELEMENT_NAME, &in_caps, &out_caps, &input);
        let label = format!("interlaced {src:?} -> {dst:?}");
        assert_eq!(ours.path.as_deref(), Some(PATH_FALLBACK), "{label}");
        assert!(
            ours.info.is_interlaced(),
            "{label}: output lost interlacing"
        );

        let exact_caps = ours.info.to_caps().expect("output caps");
        let theirs = convert_one("videoconvert", &in_caps, &exact_caps, &input);
        assert_frames_match(&label, &ours.info, &ours.buffer, &theirs.buffer, 0);
    }
}

/// Caps features must link wherever `videoconvert` links them. Field-per-buffer
/// interlacing (`format:Interlaced`) is system memory and converts; GL memory
/// cannot be mapped here and may only pass through.
#[test]
fn caps_features_link_like_videoconvert() {
    init();

    let with_features = |caps: gst::Caps, features: &[&str]| {
        let mut caps = caps;
        caps.make_mut()
            .set_features(0, Some(gst::CapsFeatures::new(features.iter().copied())));
        caps
    };

    let fields = VideoInfo::builder(VideoFormat::Uyvy, 1920, 1080)
        .fps(gst::Fraction::new(30, 1))
        .interlace_mode(gst_video::VideoInterlaceMode::Alternate)
        .build()
        .expect("alternate info");
    let in_caps = with_features(
        fields.to_caps().expect("caps"),
        &[gst_video::CAPS_FEATURE_FORMAT_INTERLACED],
    );
    let out_caps = with_features(
        gst::Caps::builder("video/x-raw")
            .field("format", "NV12")
            .build(),
        &[gst_video::CAPS_FEATURE_FORMAT_INTERLACED],
    );
    let input = gst::Buffer::with_size(fields.size()).expect("field buffer");
    let ours = convert_one(super::ELEMENT_NAME, &in_caps, &out_caps, &input);
    assert_eq!(ours.info.format(), VideoFormat::Nv12, "field-per-buffer");
    assert_eq!(
        ours.path.as_deref(),
        Some(PATH_FALLBACK),
        "field-per-buffer"
    );

    let gl = VideoInfo::builder(VideoFormat::Rgba, 1920, 1080)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .expect("gl info");
    let gl_caps = with_features(gl.to_caps().expect("caps"), &["memory:GLMemory"]);
    let input = gst::Buffer::with_size(gl.size()).expect("gl buffer");
    let ours = convert_one(super::ELEMENT_NAME, &gl_caps, &gl_caps, &input);
    assert_eq!(ours.path.as_deref(), Some(PATH_PASSTHROUGH), "GL memory");

    // What the element offers downstream for each input: any format other
    // than the input's means it is willing to convert.
    let offers_conversion = |caps: &gst::Caps| {
        let input_format = caps
            .structure(0)
            .and_then(|s| s.get::<&str>("format").ok())
            .expect("input format");
        let element = gst::ElementFactory::make(super::ELEMENT_NAME)
            .build()
            .expect("element");
        // A capsfilter answers a caps query with its caps even when stopped;
        // an appsrc does not.
        let upstream = gst::ElementFactory::make("capsfilter")
            .property("caps", caps)
            .build()
            .expect("capsfilter");
        let bin = gst::Pipeline::new();
        bin.add_many([&upstream, &element]).expect("add");
        upstream.link(&element).expect("link");
        element
            .static_pad("src")
            .expect("src pad")
            .query_caps(None)
            .iter()
            .any(|s| s.get::<&str>("format").ok() != Some(input_format))
    };
    assert!(offers_conversion(&in_caps), "field-per-buffer must convert");
    assert!(
        !offers_conversion(&gl_caps),
        "GL memory cannot be mapped here and must only pass through"
    );
}
