//! Regression test for GStreamer rtpjitterbuffer packet_spacing corruption.
//!
//! Bug: after a mute gap (no RTP packets), `calculate_packet_spacing` sees the
//! large RTP timestamp jump as packet spacing. The exponential moving average
//! (75/25 bias towards larger values) makes it persist across many packets.
//! When packet loss occurs while spacing is corrupted, lost timers are scheduled
//! far in the future (proportional to the mute duration), causing the output to
//! stall for seconds or minutes.
//!
//! Upstream references (none merged as of 2026-04):
//!   - https://gitlab.freedesktop.org/gstreamer/gst-plugins-good/-/merge_requests/570
//!   - https://gitlab.freedesktop.org/gstreamer/gst-plugins-good/-/merge_requests/951
//!   - https://gitlab.freedesktop.org/gstreamer/gst-plugins-good/-/merge_requests/221
//!
//! Workaround: set `drop-on-latency=true` on the jitterbuffer. This causes the
//! chain function to drop queued packets exceeding the configured latency,
//! advancing `next_seqnum` past the stall point.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const CLOCK_RATE: u32 = 48000;
const FRAME_DURATION_SAMPLES: u32 = 960; // 20ms at 48kHz
const PT: u8 = 96;

/// Build a minimal RTP buffer with no DTS to trigger the estimated_dts=TRUE path
/// (matching the behavior of nicesrc inside webrtcbin).
fn make_rtp_buffer(seqnum: u16, rtp_timestamp: u32, ssrc: u32, payload_size: usize) -> gst::Buffer {
    let mut data = Vec::with_capacity(12 + payload_size);
    data.push(0x80); // V=2, P=0, X=0, CC=0
    data.push(PT); // M=0, PT=96
    data.push((seqnum >> 8) as u8);
    data.push((seqnum & 0xFF) as u8);
    data.push((rtp_timestamp >> 24) as u8);
    data.push((rtp_timestamp >> 16) as u8);
    data.push((rtp_timestamp >> 8) as u8);
    data.push(rtp_timestamp as u8);
    data.push((ssrc >> 24) as u8);
    data.push((ssrc >> 16) as u8);
    data.push((ssrc >> 8) as u8);
    data.push(ssrc as u8);
    data.extend(std::iter::repeat_n(0u8, payload_size));

    let mut buf = gst::Buffer::from_slice(data);
    {
        let buf_ref = buf.get_mut().unwrap();
        buf_ref.set_dts(gst::ClockTime::NONE);
        buf_ref.set_pts(gst::ClockTime::NONE);
    }
    buf
}

/// Run the jitterbuffer mute/unmute test.
///
/// Sends `pre_mute_packets` at 50 pps, then mutes for `mute_duration_sec`,
/// then sends `good_after_unmute` good packets, skips `lost_after_good`
/// (simulating network loss), then sends `post_loss_packets`.
///
/// Returns total output buffer count.
/// Returns total output buffer count.
fn run_jitterbuffer_test(
    pre_mute_packets: u32,
    mute_duration_sec: f64,
    good_after_unmute: u32,
    lost_after_good: u32,
    post_loss_packets: u32,
    drop_on_latency: bool,
) -> u64 {
    gst::init().unwrap();

    let pipeline = gst::Pipeline::new();

    let appsrc = gst_app::AppSrc::builder()
        .name("src")
        .format(gst::Format::Time)
        .is_live(true)
        .do_timestamp(false)
        .build();

    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("encoding-name", "OPUS")
        .field("clock-rate", CLOCK_RATE as i32)
        .field("payload", PT as i32)
        .build();
    appsrc.set_caps(Some(&caps));

    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()
        .expect("capsfilter");

    let jitterbuf = gst::ElementFactory::make("rtpjitterbuffer")
        .property("latency", 40u32)
        .property("do-lost", true)
        .property("do-retransmission", false)
        .property("drop-on-latency", drop_on_latency)
        .build()
        .expect("rtpjitterbuffer");

    let fakesink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .property("async", false)
        .build()
        .expect("fakesink");

    pipeline
        .add_many([appsrc.upcast_ref(), &capsfilter, &jitterbuf, &fakesink])
        .unwrap();
    gst::Element::link_many([appsrc.upcast_ref(), &capsfilter, &jitterbuf, &fakesink]).unwrap();

    let output_count = Arc::new(AtomicU64::new(0));
    let count_clone = output_count.clone();

    let srcpad = jitterbuf.static_pad("src").unwrap();
    srcpad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
        count_clone.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });

    pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));

    let ssrc: u32 = 0x12345678;
    let mut seqnum: u16 = 10000;
    let mut rtp_ts: u32 = 3_600_000_000;

    // Phase 1: pre-mute audio
    for _ in 0..pre_mute_packets {
        let buf = make_rtp_buffer(seqnum, rtp_ts, ssrc, 100);
        if appsrc.push_buffer(buf).is_err() {
            break;
        }
        seqnum = seqnum.wrapping_add(1);
        rtp_ts = rtp_ts.wrapping_add(FRAME_DURATION_SAMPLES);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // Phase 2: mute (advance RTP timestamps, no packets)
    let mute_samples = (mute_duration_sec * CLOCK_RATE as f64) as u32;
    rtp_ts = rtp_ts.wrapping_add(mute_samples);
    std::thread::sleep(std::time::Duration::from_secs_f64(mute_duration_sec));

    // Phase 3: unmute — good packets, then loss, then continuous
    for _ in 0..good_after_unmute {
        let buf = make_rtp_buffer(seqnum, rtp_ts, ssrc, 100);
        let _ = appsrc.push_buffer(buf);
        seqnum = seqnum.wrapping_add(1);
        rtp_ts = rtp_ts.wrapping_add(FRAME_DURATION_SAMPLES);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // Skip lost packets
    seqnum = seqnum.wrapping_add(lost_after_good as u16);
    rtp_ts = rtp_ts.wrapping_add(FRAME_DURATION_SAMPLES * lost_after_good);

    for _ in 0..post_loss_packets {
        let buf = make_rtp_buffer(seqnum, rtp_ts, ssrc, 100);
        if appsrc.push_buffer(buf).is_err() {
            break;
        }
        seqnum = seqnum.wrapping_add(1);
        rtp_ts = rtp_ts.wrapping_add(FRAME_DURATION_SAMPLES);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    std::thread::sleep(std::time::Duration::from_secs(2));
    let total = output_count.load(Ordering::Relaxed);
    pipeline.set_state(gst::State::Null).unwrap();
    total
}

/// Verify that the packet_spacing bug causes a stall WITHOUT the workaround.
#[test]
#[ignore = "GStreamer version-dependent: bug may be fixed in the installed version"]
fn test_jitterbuffer_stalls_without_drop_on_latency() {
    let total = run_jitterbuffer_test(50, 10.0, 2, 2, 100, false);

    // Without drop-on-latency, the jitterbuffer holds post-loss packets
    // because lost timers are scheduled based on the corrupted packet_spacing.
    // We expect roughly 50 (pre-mute) + 2 (good after unmute) = 52 output.
    assert!(
        total < 100,
        "Expected stall (< 100 output), got {}. Bug may be fixed upstream.",
        total
    );
}

/// Verify that drop-on-latency=true resolves the stall.
#[test]
fn test_jitterbuffer_recovers_with_drop_on_latency() {
    let total = run_jitterbuffer_test(50, 10.0, 2, 2, 100, true);

    let expected_min = 140u64; // 50 + 2 + ~100, minus small margin for timing
    assert!(
        total >= expected_min,
        "Stall not resolved! Only {} buffers output with drop-on-latency=true, \
         expected at least {}.",
        total,
        expected_min
    );
}
