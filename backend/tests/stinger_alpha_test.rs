//! Alpha handling for stinger transitions.
//!
//! A stinger is a full-frame keyed clip composited over the program bus, so the
//! feature rests on two things being true: the mixer must preserve a frame's
//! per-pixel alpha, and a clip must still carry alpha after it is decoded.
//! These tests pin both down, plus the one alpha form that is *not* supported.
//!
//! The software compositor test is the guard: a GPU-only stinger is not
//! shippable, so `compositor` failing here fails the feature. The GL test is
//! supplementary and returns early when no GL context can be created, which is
//! the normal state of a headless runner.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::VideoFrameExt;

const W: u32 = 64;
const H: u32 = 64;
const FRAMES: usize = 5;
const FRAME_DUR_NS: u64 = 33_333_333;

/// Opaque blue background, as videotestsrc's big-endian ARGB `foreground-color`.
const BG_ARGB: &str = "0xff0000ff";

/// Build one overlay frame: opaque green on the left, fully transparent on the
/// right. Straight (non-premultiplied) alpha — that is what `RGBA` means in
/// GStreamer caps.
fn overlay_buffer(index: usize) -> gst::Buffer {
    let mut data = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            if x < W / 2 {
                data[i] = 0;
                data[i + 1] = 255;
                data[i + 2] = 0;
                data[i + 3] = 255;
            } else {
                data[i] = 0;
                data[i + 1] = 0;
                data[i + 2] = 0;
                data[i + 3] = 0;
            }
        }
    }
    let mut buf = gst::Buffer::from_mut_slice(data);
    {
        let b = buf.get_mut().expect("fresh buffer is writable");
        b.set_pts(gst::ClockTime::from_nseconds(index as u64 * FRAME_DUR_NS));
        b.set_duration(gst::ClockTime::from_nseconds(FRAME_DUR_NS));
    }
    buf
}

fn overlay_caps() -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .field("width", W as i32)
        .field("height", H as i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .build()
}

/// Run a composited pipeline to EOS and return the RGBA pixels of the last
/// output frame, as `(pixel_at, width, height)`.
fn composite(description: &str) -> Result<Vec<u8>, String> {
    composite_full(description, overlay_buffer, |_| {})
}

/// As [`composite`], but with a caller-supplied frame generator and a hook to
/// configure the compositor (e.g. a pad `operator`) after its pads exist.
fn composite_full<F, S>(description: &str, make: F, setup: S) -> Result<Vec<u8>, String>
where
    F: Fn(usize) -> gst::Buffer,
    S: Fn(&gst::Element),
{
    let pipeline = gst::parse::launch(description)
        .map_err(|e| format!("parse failed: {e}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;

    let appsrc = pipeline
        .by_name("fg")
        .ok_or("no appsrc named fg")?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| "fg is not an appsrc".to_string())?;
    appsrc.set_caps(Some(&overlay_caps()));
    appsrc.set_format(gst::Format::Time);
    appsrc.set_is_live(false);

    let appsink = pipeline
        .by_name("out")
        .ok_or("no appsink named out")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| "out is not an appsink".to_string())?;

    // Background pad under the overlay pad. Geometry is explicit on both so
    // neither depends on a sizing policy.
    let comp = pipeline.by_name("comp").ok_or("no compositor named comp")?;
    for (pad_name, zorder) in [("sink_0", 0u32), ("sink_1", 1u32)] {
        let pad = comp
            .static_pad(pad_name)
            .ok_or_else(|| format!("compositor has no {pad_name}"))?;
        pad.set_property("xpos", 0i32);
        pad.set_property("ypos", 0i32);
        pad.set_property("width", W as i32);
        pad.set_property("height", H as i32);
        pad.set_property("zorder", zorder);
        pad.set_property("alpha", 1.0f64);
    }

    setup(&comp);

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("failed to reach PLAYING: {e}"))?;

    // Push the whole overlay up front: FRAMES * W * H * 4 = 80 KB, well under
    // appsrc's 200 KB default max-bytes, so no push blocks and the pull below
    // cannot deadlock against a producer thread.
    for i in 0..FRAMES {
        appsrc
            .push_buffer(make(i))
            .map_err(|e| format!("push_buffer failed: {e:?}"))?;
    }
    let _ = appsrc.end_of_stream();

    // Take the last frame before EOS: the first composited frame can in
    // principle be aggregated before the overlay pad has data.
    let mut last: Option<Vec<u8>> = None;
    // Ends on Err: EOS or flushing.
    while let Ok(sample) = appsink.pull_sample() {
        let caps = sample.caps().ok_or("sample without caps")?;
        let info =
            gst_video::VideoInfo::from_caps(caps).map_err(|e| format!("bad video info: {e}"))?;
        let buf = sample.buffer().ok_or("sample without buffer")?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buf, &info)
            .map_err(|e| format!("frame map failed: {e}"))?;
        let stride = frame.plane_stride()[0] as usize;
        let src = frame
            .plane_data(0)
            .map_err(|e| format!("no plane 0: {e}"))?;
        // Copy out tightly packed so the caller need not know the stride.
        let mut packed = vec![0u8; (W * H * 4) as usize];
        for y in 0..H as usize {
            let s = y * stride;
            let d = y * (W as usize) * 4;
            packed[d..d + (W as usize) * 4].copy_from_slice(&src[s..s + (W as usize) * 4]);
        }
        last = Some(packed);
    }

    let _ = pipeline.set_state(gst::State::Null);
    last.ok_or_else(|| "pipeline produced no frames".to_string())
}

fn pixel(frame: &[u8], x: u32, y: u32) -> (u8, u8, u8, u8) {
    let i = ((y * W + x) * 4) as usize;
    (frame[i], frame[i + 1], frame[i + 2], frame[i + 3])
}

/// Assert the composited frame proves per-pixel alpha survived.
fn assert_alpha_survived(frame: &[u8], backend: &str) {
    let left = pixel(frame, W / 4, H / 2);
    let right = pixel(frame, 3 * W / 4, H / 2);

    // Left: the overlay is opaque there, so it must win over the background.
    assert!(
        left.1 > 200 && left.2 < 55,
        "{backend}: opaque overlay region should read green, got {left:?}"
    );

    // Right: the overlay is fully transparent there, so the blue background
    // must show through. Black here means the mixer painted the overlay's RGB
    // regardless of its alpha — the failure this spike exists to catch.
    assert!(
        right.2 > 200 && right.1 < 55,
        "{backend}: transparent overlay region should read the blue background, \
         got {right:?} (black here means per-pixel alpha was ignored)"
    );
}

/// Software path — `compositor`, from gst-plugins-bad, present in CI.
/// This is the one that gates the feature: a GPU-only stinger is not shippable.
#[test]
fn alpha_survives_software_compositor() {
    gst::init().expect("gstreamer init");

    if gst::ElementFactory::find("compositor").is_none() {
        panic!("compositor element missing — it ships in gst-plugins-bad, which CI installs");
    }

    let description = format!(
        "appsrc name=fg ! comp.sink_1 \
         videotestsrc num-buffers={FRAMES} pattern=solid-color foreground-color={BG_ARGB} \
           ! video/x-raw,format=RGBA,width={W},height={H},framerate=30/1 ! comp.sink_0 \
         compositor name=comp background=black \
           ! video/x-raw,format=RGBA ! appsink name=out sync=false"
    );

    let frame = composite(&description).expect("software composite");
    assert_alpha_survived(&frame, "compositor");
}

/// GPU path — `glvideomixerelement`. Skips when no GL context can be created:
/// on headless runners the elements exist but a GPU pipeline builds, starts,
/// and silently never produces a frame. Same probe as `vision_mixer_fx_test`.
#[test]
fn alpha_survives_gl_videomixer() {
    gst::init().expect("gstreamer init");

    if !gl_environment_available() {
        eprintln!("skipping: no usable GL environment");
        return;
    }

    let description = format!(
        "appsrc name=fg ! glupload ! glcolorconvert ! glmix.sink_1 \
         videotestsrc num-buffers={FRAMES} pattern=solid-color foreground-color={BG_ARGB} \
           ! video/x-raw,format=RGBA,width={W},height={H},framerate=30/1 \
           ! glupload ! glcolorconvert ! glmix.sink_0 \
         glvideomixerelement name=glmix background=black \
           ! gldownload ! video/x-raw,format=RGBA ! appsink name=out sync=false"
    );

    // `composite` looks up the mixer by the name "comp"; alias it here.
    let description = description.replace("glmix", "comp");

    let frame = composite(&description).expect("GL composite");
    assert_alpha_survived(&frame, "glvideomixerelement");
}

/// A trivial shader-free GL run must reach EOS. Merely having the GL plugins
/// installed is not enough — see `vision_mixer_fx_test`.
fn gl_environment_available() -> bool {
    if gst::ElementFactory::find("glvideomixerelement").is_none()
        || gst::ElementFactory::find("gltestsrc").is_none()
    {
        return false;
    }
    let Ok(pipeline) = gst::parse::launch(
        "gltestsrc num-buffers=3 ! video/x-raw(memory:GLMemory),format=RGBA,width=64,height=64,framerate=30/1 ! fakesink sync=false",
    ) else {
        return false;
    };
    let Ok(pipeline) = pipeline.downcast::<gst::Pipeline>() else {
        return false;
    };
    if pipeline.set_state(gst::State::Playing).is_err() {
        return false;
    }
    let bus = pipeline.bus().expect("pipeline has a bus");
    // 20 s budget: software GL context creation can be slow on loaded CI.
    let ok = matches!(
        bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ),
        Some(msg) if matches!(msg.view(), gst::MessageView::Eos(_))
    );
    let _ = pipeline.set_state(gst::State::Null);
    ok
}

/// Encode a half-opaque / half-transparent BGRA clip to `path`, losslessly.
///
/// FFV1 in Matroska is the alpha-carrying combination available in CI (CI
/// installs `gstreamer1.0-libav` on Linux and `gst-libav` on macOS).
/// `avenc_ffv1` takes BGRA but not RGBA — the byte layouts of the two colours
/// used here happen to be identical, so `overlay_buffer` serves both.
fn write_alpha_clip(path: &std::path::Path) -> Result<(), String> {
    let description = format!(
        "appsrc name=fg ! avenc_ffv1 ! matroskamux ! filesink location={}",
        path.display()
    );
    let pipeline = gst::parse::launch(&description)
        .map_err(|e| format!("parse failed: {e}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;

    let appsrc = pipeline
        .by_name("fg")
        .ok_or("no appsrc named fg")?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| "fg is not an appsrc".to_string())?;
    appsrc.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", W as i32)
            .field("height", H as i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .build(),
    ));
    appsrc.set_format(gst::Format::Time);
    appsrc.set_is_live(false);

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("encode failed to reach PLAYING: {e}"))?;
    for i in 0..FRAMES {
        appsrc
            .push_buffer(overlay_buffer(i))
            .map_err(|e| format!("push_buffer failed: {e:?}"))?;
    }
    let _ = appsrc.end_of_stream();

    // The file is only complete once the muxer has written its headers, so wait
    // for EOS rather than tearing down as soon as the pushes return.
    let bus = pipeline.bus().expect("pipeline has a bus");
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(20),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    match msg {
        Some(m) if matches!(m.view(), gst::MessageView::Eos(_)) => Ok(()),
        Some(m) => Err(format!("encode failed: {:?}", m.view())),
        None => Err("encode timed out before EOS".to_string()),
    }
}

/// Decode `uri` and return the last frame as tightly packed BGRA.
fn decode_last_bgra(uri: &str) -> Result<Vec<u8>, String> {
    let description = format!(
        "uridecodebin uri={uri} ! videoconvert ! video/x-raw,format=BGRA \
         ! appsink name=out sync=false"
    );
    let pipeline = gst::parse::launch(&description)
        .map_err(|e| format!("parse failed: {e}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
    let appsink = pipeline
        .by_name("out")
        .ok_or("no appsink named out")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| "out is not an appsink".to_string())?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("decode failed to reach PLAYING: {e}"))?;

    let mut last: Option<Vec<u8>> = None;
    while let Ok(sample) = appsink.pull_sample() {
        let caps = sample.caps().ok_or("sample without caps")?;
        let info =
            gst_video::VideoInfo::from_caps(caps).map_err(|e| format!("bad video info: {e}"))?;
        let buf = sample.buffer().ok_or("sample without buffer")?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buf, &info)
            .map_err(|e| format!("frame map failed: {e}"))?;
        let stride = frame.plane_stride()[0] as usize;
        let src = frame
            .plane_data(0)
            .map_err(|e| format!("no plane 0: {e}"))?;
        let mut packed = vec![0u8; (W * H * 4) as usize];
        for y in 0..H as usize {
            let s = y * stride;
            let d = y * (W as usize) * 4;
            packed[d..d + (W as usize) * 4].copy_from_slice(&src[s..s + (W as usize) * 4]);
        }
        last = Some(packed);
    }

    let _ = pipeline.set_state(gst::State::Null);
    last.ok_or_else(|| "decode produced no frames".to_string())
}

/// Phase 3: does a clip's alpha channel survive `uridecodebin`?
///
/// A stinger is a clip played off disk, so Phase 1 (the mixer keeps alpha) is
/// only half the path — the clip has to arrive with alpha still attached. The
/// media player block decodes with `uridecodebin` and pins no caps of its own,
/// neither on the internal appsink nor on the main-pipeline appsrc, so what
/// `uridecodebin` produces is what reaches the mixer.
///
/// Discriminating because the pipeline converts to BGRA before the sink: if
/// alpha were lost anywhere in decode, `videoconvert` would fill it back in as
/// opaque and the transparent half would read alpha 255 instead of 0.
#[test]
fn alpha_survives_clip_decode() {
    gst::init().expect("gstreamer init");
    for name in ["avenc_ffv1", "matroskamux"] {
        if gst::ElementFactory::find(name).is_none() {
            panic!("{name} missing — CI installs gst-libav and plugins-good");
        }
    }

    let path = std::env::temp_dir().join(format!("strom_stinger_alpha_{}.mkv", std::process::id()));
    write_alpha_clip(&path).expect("encode alpha clip");

    let frame = decode_last_bgra(&format!("file://{}", path.display())).expect("decode alpha clip");
    let _ = std::fs::remove_file(&path);

    let left = pixel(&frame, W / 4, H / 2);
    let right = pixel(&frame, 3 * W / 4, H / 2);

    assert_eq!(
        left.3, 255,
        "opaque half of the clip should decode opaque, got alpha {}",
        left.3
    );
    assert_eq!(
        right.3, 0,
        "transparent half of the clip should decode transparent, got alpha {} \
         (255 means the alpha channel was dropped somewhere in decode)",
        right.3
    );
}

// --- Phase 4: premultiplied alpha ---------------------------------------
//
// Straight and premultiplied alpha are identical at alpha 0 and alpha 255, so
// the Phase 1 frame (hard opaque/transparent split) cannot tell them apart.
// The difference only exists at partial alpha, which is exactly where a
// stinger's soft edges live. These constants make it a uniform 50 % frame.

/// Mid-grey background, as videotestsrc's big-endian ARGB `foreground-color`.
/// Deliberately mid-tone: against black or white, clamping hides the errors.
const GREY_ARGB: &str = "0xff404040";
const BG_LEVEL: f64 = 64.0;
const HALF_A: u8 = 128;

/// White at 50 % alpha, straight (non-premultiplied): colour is untouched.
fn straight_half(index: usize) -> gst::Buffer {
    half_frame(index, 255)
}

/// The same intent, premultiplied: colour is pre-scaled by the alpha.
fn premultiplied_half(index: usize) -> gst::Buffer {
    half_frame(index, HALF_A)
}

fn half_frame(index: usize, level: u8) -> gst::Buffer {
    let mut data = vec![0u8; (W * H * 4) as usize];
    for px in data.chunks_exact_mut(4) {
        px[0] = level;
        px[1] = level;
        px[2] = level;
        px[3] = HALF_A;
    }
    let mut buf = gst::Buffer::from_mut_slice(data);
    {
        let b = buf.get_mut().expect("fresh buffer is writable");
        b.set_pts(gst::ClockTime::from_nseconds(index as u64 * FRAME_DUR_NS));
        b.set_duration(gst::ClockTime::from_nseconds(FRAME_DUR_NS));
    }
    buf
}

fn grey_pipeline() -> String {
    format!(
        "appsrc name=fg ! comp.sink_1 \
         videotestsrc num-buffers={FRAMES} pattern=solid-color foreground-color={GREY_ARGB} \
           ! video/x-raw,format=RGBA,width={W},height={H},framerate=30/1 ! comp.sink_0 \
         compositor name=comp background=black \
           ! video/x-raw,format=RGBA ! appsink name=out sync=false"
    )
}

/// Phase 4: does the compositor handle a premultiplied source, and can any pad
/// `operator` rescue one?
///
/// ATEM exposes a "Pre Multiplied Key" toggle because stinger clips are
/// commonly authored premultiplied. GStreamer's `compositor` has no such
/// switch — its pad `operator` offers only source/over/add — and nothing in the
/// tree advertises premultiplied handling. This pins down what that means
/// numerically, so the spec can say whether premultiplied clips are supported,
/// rejected, or need a conversion step of our own.
#[test]
fn premultiplied_alpha_needs_explicit_handling() {
    gst::init().expect("gstreamer init");

    let a = HALF_A as f64 / 255.0;
    let centre = |frame: &Vec<u8>| pixel(frame, W / 2, H / 2).0 as f64;

    // Reference: straight alpha through the default `over`. This is correct
    // compositing and the value every other case is judged against.
    let straight =
        centre(&composite_full(&grey_pipeline(), straight_half, |_| {}).expect("straight over"));
    let expected_straight = 255.0 * a + BG_LEVEL * (1.0 - a);
    assert!(
        (straight - expected_straight).abs() < 6.0,
        "straight alpha over mid-grey should be ~{expected_straight:.0}, got {straight:.0}"
    );

    // The artifact: the same intent, premultiplied, blended as if straight.
    // The colour gets multiplied by alpha a second time, so it comes out dark.
    let premul_over =
        centre(&composite_full(&grey_pipeline(), premultiplied_half, |_| {}).expect("premul over"));
    let expected_dark = HALF_A as f64 * a + BG_LEVEL * (1.0 - a);
    assert!(
        (premul_over - expected_dark).abs() < 6.0,
        "premultiplied blended as straight should darken to ~{expected_dark:.0}, got {premul_over:.0}"
    );
    assert!(
        premul_over < straight - 40.0,
        "expected a large visible darkening; straight={straight:.0} premul={premul_over:.0}"
    );

    // Is `add` a rescue? Correct premultiplied `over` is src + dst*(1-a).
    // Measured: `add` returns exactly what `over` returns here, so it is not a
    // different blend for this case at all, let alone the right one. The
    // read-back below guards the obvious way to be fooled — a silently ignored
    // property would produce the same reading.
    let premul_add = centre(
        &composite_full(&grey_pipeline(), premultiplied_half, |comp| {
            let pad = comp.static_pad("sink_1").expect("sink_1");
            pad.set_property_from_str("operator", "add");
            // Read back: a silently-ignored property would make the comparison
            // below prove nothing.
            let applied = pad.property_value("operator");
            eprintln!(
                "sink_1 operator after set: {:?}",
                applied.serialize().map(|s| s.to_string())
            );
        })
        .expect("premul add"),
    );
    let correct_premul = HALF_A as f64 + BG_LEVEL * (1.0 - a);
    eprintln!(
        "straight/over={straight:.0}  premul/over={premul_over:.0}  premul/add={premul_add:.0}  \
correct premultiplied over would be {correct_premul:.0}"
    );
    assert!(
        (premul_add - correct_premul).abs() > 6.0,
        "no compositor operator produced a correct premultiplied `over` \
         (add gave {premul_add:.0}, correct is {correct_premul:.0}) — if this \
         now matches, GStreamer changed and premultiplied support got cheaper"
    );
}
