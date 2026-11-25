use super::burn_in::{decode_cells, Layout, TimestampBurnIn, CODE_CELLS};
use super::stamper::{ReferenceTimestamper, Timescale};
use crate::tams::tai_mapping::{reference_tai_ns, HostTai};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

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
        // TAMS Output anchors on it as TAI = UTC + (TAI - UTC).
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
    let layout = Layout::for_frame(W, H);
    for s in &samples {
        let buf = s.buffer().unwrap();
        let info = gst_video::VideoInfo::from_caps(s.caps().unwrap()).unwrap();
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buf, &info).unwrap();
        let luma = frame.plane_data(0).unwrap();
        let stride = frame.plane_stride()[0] as usize;
        let y = (layout.code_y + layout.cell_h / 2) as usize;
        let cells: Vec<bool> = (0..CODE_CELLS)
            .map(|i| luma[y * stride + (i * layout.cell_w + layout.cell_w / 2) as usize] > 128)
            .collect();
        let stamp = base + running_time(s);
        assert_eq!(decode_cells(&cells), Some(stamp / 1_000));
        // Outside the band the gray test picture is untouched.
        let below = (layout.band_y + layout.band_h + 1) as usize;
        assert!(luma[below * stride..below * stride + W as usize]
            .iter()
            .all(|&v| v != 16 && v != 235));
    }
}
