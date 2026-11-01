//! The encoder delay of AAC encoders, taken out of the timeline.
//!
//! `avenc_aac` and `fdkaacenc` start their output with their encoder delay
//! (priming samples) and stamp that first packet as if it were the first
//! sound, with no clipping meta, and no muxer is told. In Matroska, MP4 and
//! MPEG-TS the sound then plays that much behind the picture: 21.3 ms at
//! 48 kHz for `avenc_aac` (1024 samples, measured with a flash-and-beep clip,
//! decoded by FFmpeg and by GStreamer), 42.7 ms for `fdkaacenc` (2048, the
//! encoder delay libfdk-aac reports for AAC-LC).
//!
//! Muxers place buffers by running time, so moving the encoder's output one
//! delay earlier (a negative pad offset) puts the sound back where it was
//! heard. The rate is read from the encoder's caps event, not per buffer.
//! Every flow pipeline does this for each of these encoders in it, standalone or
//! inside a block, also those a block adds while it runs.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::debug;

/// Samples an encoder puts before the sound, by factory name (AAC-LC):
/// measured for `avenc_aac` and `faac`, libfdk-aac's reported encoder delay
/// for `fdkaacenc`. `voaacenc` is not measured and stays as it is.
fn delay_samples(factory: &str) -> Option<u64> {
    match factory {
        "avenc_aac" => Some(1024),
        "fdkaacenc" => Some(2048),
        "faac" => Some(1024),
        _ => None,
    }
}

/// Compensate every AAC encoder that is added to `pipeline` or to a bin
/// inside it, from now on.
pub fn compensate_all(pipeline: &gst::Pipeline) {
    pipeline.connect_deep_element_added(|_, _, element| compensate(element));
}

/// Move `encoder`'s output one encoder delay earlier in running time, once
/// its input rate is known. Encoders without a known delay stay as they are.
fn compensate(encoder: &gst::Element) {
    let Some(samples) = encoder
        .factory()
        .and_then(|f| delay_samples(f.name().as_str()))
    else {
        return;
    };
    let (Some(sink), Some(src)) = (encoder.static_pad("sink"), encoder.static_pad("src")) else {
        return;
    };

    // The sound now leaves one delay later than its running time says:
    // live muxers have to wait that much longer for it.
    let delay_ns = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reported = delay_ns.clone();
    src.add_probe(
        gst::PadProbeType::QUERY_UPSTREAM | gst::PadProbeType::PULL,
        move |_, info| {
            let Some(gst::PadProbeData::Query(query)) = &mut info.data else {
                return gst::PadProbeReturn::Ok;
            };
            let gst::QueryViewMut::Latency(latency) = query.view_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            let extra =
                gst::ClockTime::from_nseconds(reported.load(std::sync::atomic::Ordering::Relaxed));
            let (live, min, max) = latency.result();
            latency.set(live, min + extra, max.map(|m| m + extra));
            gst::PadProbeReturn::Ok
        },
    );

    // Weak: the probe lives on the encoder's own pad.
    let src = src.downgrade();
    let element = encoder.downgrade();
    let name = encoder.name();
    sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let rate = caps
            .caps()
            .structure(0)
            .and_then(|s| s.get::<i32>("rate").ok())
            .filter(|r| *r > 0);
        let (Some(rate), Some(src)) = (rate, src.upgrade()) else {
            return gst::PadProbeReturn::Ok;
        };

        let delay = gst::ClockTime::SECOND.nseconds() * samples / rate as u64;
        let before = delay_ns.swap(delay, std::sync::atomic::Ordering::Relaxed);
        src.set_offset(-(delay as i64));
        if before != delay {
            if let Some(element) = element.upgrade() {
                let _ = element.post_message(gst::message::Latency::new());
            }
        }
        debug!("{}: output {} ns earlier (encoder delay)", name, delay);
        gst::PadProbeReturn::Ok
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Running time of the first output buffers: one encoder delay earlier
    /// than their timestamps, also for an encoder inside a bin that joins
    /// the pipeline later. Without the compensation they are equal.
    #[test]
    fn avenc_aac_in_a_bin_runs_one_delay_early() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        compensate_all(&pipeline);
        let bin = gst::parse::bin_from_description(
            "audiotestsrc num-buffers=20 samplesperbuffer=1024 timestamp-offset=1000000000 \
             ! audio/x-raw,rate=48000 ! avenc_aac ! appsink name=out sync=false",
            false,
        )
        .unwrap();
        pipeline.add(&bin).unwrap();
        let out = bin
            .by_name("out")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let sample = out.pull_sample().unwrap();
        let pts = sample.buffer().unwrap().pts().unwrap();
        let segment = sample
            .segment()
            .unwrap()
            .downcast_ref::<gst::ClockTime>()
            .unwrap();
        let running = segment.to_running_time(pts).unwrap();
        pipeline.set_state(gst::State::Null).unwrap();

        // 1024 samples at 48 kHz.
        assert_eq!(pts - running, gst::ClockTime::from_nseconds(21_333_333));
    }
}
