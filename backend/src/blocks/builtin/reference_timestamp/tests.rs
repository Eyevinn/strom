use super::burn_in::{decode_cells, Layout, TimestampBurnIn, CODE_CELLS};
use super::stamper::{ReferenceTimestamper, Timescale};
use super::ReferenceTimestampBuilder;
use crate::blocks::{BlockBuildContext, BlockBuilder};
use crate::events::EventBroadcaster;
use crate::tams::tai_mapping::{reference_tai_ns, HostTai};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom_types::PropertyValue;

const W: u32 = 320;
const H: u32 = 180;

/// A live test source on a Realtime clock through the stamper (and burn-in) into
/// an appsink. Returns the samples and the pipeline's base time.
fn run(
    timescale: Timescale,
    burn_in: bool,
    upstream_meta: Option<(&str, u64)>,
) -> (Vec<gst::Sample>, u64) {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let clock: gst::Clock = gst::glib::Object::builder::<gst::SystemClock>()
        .property("clock-type", gst::ClockType::Realtime)
        .build()
        .upcast();
    pipeline.use_clock(Some(&clock));
    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .property("num-buffers", 5i32)
        .property_from_str("pattern", "solid-color")
        .property("foreground-color", 0xff80_8080u32)
        .build()
        .unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::I420)
                .width(W as i32)
                .height(H as i32)
                .framerate(gst::Fraction::new(50, 1))
                .build(),
        )
        .build()
        .unwrap();
    let stamp = ReferenceTimestamper::new("stamp", timescale);
    let sink = gst_app::AppSink::builder().sync(false).build();
    pipeline
        .add_many([&src, &caps, stamp.upcast_ref(), sink.upcast_ref()])
        .unwrap();
    gst::Element::link_many([&src, &caps, stamp.upcast_ref()]).unwrap();
    if burn_in {
        let burn = TimestampBurnIn::new("burn");
        pipeline.add(&burn).unwrap();
        gst::Element::link_many([
            stamp.upcast_ref::<gst::Element>(),
            burn.upcast_ref(),
            sink.upcast_ref(),
        ])
        .unwrap();
    } else {
        stamp.link(&sink).unwrap();
    }
    if let Some((name, ts)) = upstream_meta {
        let name = name.to_string();
        caps.static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buf) = info.buffer_mut() {
                    gst::ReferenceTimestampMeta::add(
                        buf.make_mut(),
                        &gst::Caps::new_empty_simple(&name),
                        gst::ClockTime::from_nseconds(ts),
                        gst::ClockTime::NONE,
                    );
                }
                gst::PadProbeReturn::Ok
            });
    }

    pipeline.set_state(gst::State::Playing).unwrap();
    let mut samples = Vec::new();
    while let Ok(s) = sink.pull_sample() {
        samples.push(s);
    }
    let base = pipeline.base_time().unwrap().nseconds();
    pipeline.set_state(gst::State::Null).unwrap();
    (samples, base)
}

fn running_time(sample: &gst::Sample) -> u64 {
    let seg = sample
        .segment()
        .unwrap()
        .downcast_ref::<gst::format::Time>()
        .unwrap()
        .clone();
    seg.to_running_time(sample.buffer().unwrap().pts().unwrap())
        .unwrap()
        .nseconds()
}

fn metas(buf: &gst::BufferRef) -> Vec<(String, u64)> {
    buf.iter_meta::<gst::ReferenceTimestampMeta>()
        .map(|m| {
            (
                m.reference().structure(0).unwrap().name().to_string(),
                m.timestamp().nseconds(),
            )
        })
        .collect()
}

/// On a UTC clock the stamp is exactly the clock time of the buffer's running time.
#[test]
fn utc_stamp_is_clock_time_of_running_time() {
    let (samples, base) = run(Timescale::Utc, false, None);
    assert_eq!(samples.len(), 5);
    let off = HostTai::new().tai_minus_utc_ns();
    for s in &samples {
        let buf = s.buffer().unwrap();
        let expect = base + running_time(s);
        assert_eq!(metas(buf), vec![("timestamp/x-unix".to_string(), expect)]);
        // A consumer reads it as TAI = UTC + (TAI - UTC).
        assert_eq!(reference_tai_ns(buf, off), Some(expect + off));
    }
}

#[test]
fn tai_stamp_is_utc_plus_offset() {
    let (samples, base) = run(Timescale::Tai, false, None);
    let off = HostTai::new().tai_minus_utc_ns();
    for s in &samples {
        let expect = base + running_time(s) + off;
        assert_eq!(
            metas(s.buffer().unwrap()),
            vec![("timestamp/x-tai".to_string(), expect)]
        );
    }
}

/// A capture time set upstream in the same domain is kept, not overwritten.
#[test]
fn upstream_capture_time_wins() {
    let (samples, _) = run(Timescale::Utc, false, Some(("timestamp/x-unix", 42)));
    for s in &samples {
        assert_eq!(
            metas(s.buffer().unwrap()),
            vec![("timestamp/x-unix".to_string(), 42)]
        );
    }
}

/// The code row drawn into the luma plane decodes back to the stamp, and the
/// band leaves the rest of the picture alone.
#[test]
fn burn_in_code_reads_back_as_the_stamp() {
    let (samples, base) = run(Timescale::Utc, true, None);
    assert_eq!(samples.len(), 5);
    for s in &samples {
        assert_stamp_burnt_in(s, base + running_time(s));
    }
}

/// The code row in a sample's luma plane decodes to `stamp`, and the gray
/// test picture below the band is untouched.
fn assert_stamp_burnt_in(s: &gst::Sample, stamp: u64) {
    let layout = Layout::for_frame(W, H).unwrap();
    {
        let buf = s.buffer().unwrap();
        let info = gst_video::VideoInfo::from_caps(s.caps().unwrap()).unwrap();
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buf, &info).unwrap();
        let luma = frame.plane_data(0).unwrap();
        let stride = frame.plane_stride()[0] as usize;
        let y = (layout.code_y + layout.cell_h / 2) as usize;
        let cells: Vec<bool> = (0..CODE_CELLS)
            .map(|i| luma[y * stride + (i * layout.cell_w + layout.cell_w / 2) as usize] > 128)
            .collect();
        assert_eq!(decode_cells(&cells), Some(stamp / 1_000));
        // Outside the band the gray test picture is untouched.
        let below = (layout.band_y + layout.band_h + 1) as usize;
        assert!(luma[below * stride..below * stride + W as usize]
            .iter()
            .all(|&v| v != 16 && v != 235));
    }
}

/// A capture time upstream set in another domain (`x-ntp`, as rtpjitterbuffer
/// adds from RTCP) is what a UTC consumer gets, converted, not the pipeline
/// clock's time.
#[test]
fn upstream_capture_time_in_another_domain_is_converted() {
    const NTP_TO_UNIX_NS: u64 = 2_208_988_800 * 1_000_000_000;
    let unix = 1_795_529_318_680_000_000u64;
    let (samples, _) = run(
        Timescale::Utc,
        false,
        Some(("timestamp/x-ntp", unix + NTP_TO_UNIX_NS)),
    );
    assert_eq!(samples.len(), 5);
    for s in &samples {
        assert_eq!(
            metas(s.buffer().unwrap()),
            vec![
                ("timestamp/x-ntp".to_string(), unix + NTP_TO_UNIX_NS),
                ("timestamp/x-unix".to_string(), unix),
            ]
        );
    }
}

/// In a non-live flow the preroll frame passes before the pipeline has a clock
/// and gets no stamp; every frame after it does.
#[test]
fn preroll_frame_of_a_non_live_stream_is_not_stamped() {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 4i32)
        .build()
        .unwrap();
    let stamp = ReferenceTimestamper::new("stamp", Timescale::Utc);
    let sink = gst_app::AppSink::builder().sync(false).build();
    pipeline
        .add_many([&src, stamp.upcast_ref(), sink.upcast_ref()])
        .unwrap();
    gst::Element::link_many([&src, stamp.upcast_ref(), sink.upcast_ref()]).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let mut samples = Vec::new();
    while let Ok(s) = sink.pull_sample() {
        samples.push(s);
    }
    pipeline.set_state(gst::State::Null).unwrap();
    assert_eq!(samples.len(), 4);
    assert!(metas(samples[0].buffer().unwrap()).is_empty());
    for s in &samples[1..] {
        assert_eq!(metas(s.buffer().unwrap()).len(), 1);
    }
}

/// The real block, built by its builder with `burn_in` on, between `src` (the
/// last element of `upstream`) and an appsink, on a Realtime clock. Returns the
/// samples, the base time, and the error the flow failed with, if any.
fn run_block(upstream: &[gst::Element]) -> (Vec<gst::Sample>, u64, Option<String>) {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let clock: gst::Clock = gst::glib::Object::builder::<gst::SystemClock>()
        .property("clock-type", gst::ClockType::Realtime)
        .build()
        .upcast();
    pipeline.use_clock(Some(&clock));
    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let props = HashMap::from([("burn_in".to_string(), PropertyValue::Bool(true))]);
    let built = ReferenceTimestampBuilder.build("rt", &props, &ctx).unwrap();
    let by_id: HashMap<String, gst::Element> = built.elements.iter().cloned().collect();
    for (_, e) in &built.elements {
        pipeline.add(e).unwrap();
    }
    for (from, to) in &built.internal_links {
        by_id[&from.element_id]
            .link_pads(
                from.pad_name.as_deref(),
                &by_id[&to.element_id],
                to.pad_name.as_deref(),
            )
            .unwrap();
    }
    for setup in ctx.take_element_setups() {
        setup(
            uuid::Uuid::new_v4(),
            EventBroadcaster::new(10, false, false),
        );
    }
    let pads = ReferenceTimestampBuilder.get_external_pads(&props).unwrap();
    let input = &by_id[&format!("rt:{}", pads.inputs[0].internal_element_id)];
    let output = &by_id[&format!("rt:{}", pads.outputs[0].internal_element_id)];
    let sink = gst_app::AppSink::builder().sync(false).build();
    pipeline.add_many(upstream).unwrap();
    pipeline.add(&sink).unwrap();
    gst::Element::link_many(upstream).unwrap();
    upstream.last().unwrap().link(input).unwrap();
    output.link(&sink).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let mut samples = Vec::new();
    let mut error = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        // EOS from the bus, not `AppSink::is_eos`, which is also true before
        // the sink has started.
        if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos]) {
            if let gst::MessageView::Error(e) = msg.view() {
                error = Some(format!("{} ({:?})", e.error(), e.debug()));
            }
            break;
        }
        if let Some(s) = sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) {
            samples.push(s);
        }
    }
    while let Some(s) = sink.try_pull_sample(gst::ClockTime::ZERO) {
        samples.push(s);
    }
    let base = pipeline.base_time().map_or(0, |b| b.nseconds());
    pipeline.set_state(gst::State::Null).unwrap();
    (samples, base, error)
}

/// A live test source of `format`, through a capsfilter.
fn live_source(format: gst_video::VideoFormat) -> Vec<gst::Element> {
    gst::init().unwrap();
    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .property("num-buffers", 3i32)
        .property_from_str("pattern", "solid-color")
        .property("foreground-color", 0xff80_8080u32)
        .build()
        .unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst_video::VideoCapsBuilder::new()
                .format(format)
                .width(W as i32)
                .height(H as i32)
                .framerate(gst::Fraction::new(50, 1))
                .build(),
        )
        .build()
        .unwrap();
    vec![src, caps]
}

/// Guard test for the block's input adaptation: RGBA, which the burn-in
/// element does not take, still gets the band.
#[test]
fn block_burns_in_a_format_it_converts() {
    let (samples, base, error) = run_block(&live_source(gst_video::VideoFormat::Rgba));
    assert_eq!(error, None);
    assert_eq!(samples.len(), 3);
    for s in &samples {
        assert_stamp_burnt_in(s, base + running_time(s));
    }
}

/// Guard test: encoded video fails the flow with an element error that says
/// what arrived, not a bare not-negotiated.
#[test]
fn block_refuses_encoded_video_with_an_element_error() {
    gst::init().unwrap();
    let src = gst_app::AppSrc::builder()
        .caps(
            &gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        )
        .format(gst::Format::Time)
        .is_live(true)
        .build();
    let sent = std::sync::atomic::AtomicBool::new(false);
    src.set_callbacks(
        gst_app::AppSrcCallbacks::builder()
            .need_data(move |src, _| {
                if sent.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let mut buf = gst::Buffer::from_slice([0u8, 0, 0, 1, 0x09, 0x10]);
                buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
                let _ = src.push_buffer(buf);
                let _ = src.end_of_stream();
            })
            .build(),
    );
    let (samples, _, error) = run_block(&[src.upcast()]);
    assert!(samples.is_empty());
    let error = error.expect("the flow should fail");
    assert!(error.contains("video/x-h264"), "{error}");
    assert!(error.contains("not raw video"), "{error}");
}
