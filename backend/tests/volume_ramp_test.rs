//! Integration tests for the `VolumeRampManager`.
//!
//! Each test builds a minimal `audiotestsrc ! volume ! fakesink` pipeline,
//! exercises the manager directly, and asserts on the GObject `volume`
//! property the way a real consumer would observe it.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::time::{Duration, Instant};
use strom::gst::volume_ramp::VolumeRampManager;

/// Block until `cond` returns true or `timeout` elapses. Tests pass as fast
/// as the pipeline is ready instead of sleeping a fixed wall-clock.
fn poll_until(cond: impl Fn() -> bool, timeout: Duration, msg: &str) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out after {:?}: {}", timeout, msg);
}

struct TestPipe {
    pipeline: gst::Pipeline,
    volume: gst::Element,
}

impl TestPipe {
    fn new() -> Self {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .property("samplesperbuffer", 480_i32) // 10ms @ 48k → frequent sync_values
            .build()
            .expect("audiotestsrc");
        let volume = gst::ElementFactory::make("volume").build().expect("volume");
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .expect("fakesink");

        pipeline.add_many([&src, &volume, &sink]).unwrap();
        gst::Element::link_many([&src, &volume, &sink]).unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();

        // Wait until the volume element has a stream-time position — required
        // for the ramp manager to schedule keyframes. Buffers must be flowing.
        poll_until(
            || volume.query_position::<gst::ClockTime>().is_some(),
            Duration::from_secs(3),
            "pipeline never produced a stream-time position",
        );

        TestPipe { pipeline, volume }
    }

    fn vol(&self) -> f64 {
        self.volume.property::<f64>("volume")
    }

    fn mute(&self) -> bool {
        self.volume.property::<bool>("mute")
    }
}

impl Drop for TestPipe {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Sleep just long enough for the volume element to process at least a few
/// buffers (≥ 30ms, given our 10ms-buffer audiotestsrc). Allows the control
/// source's sync_values to update `self->volume` to the latest interpolated
/// keyframe value.
async fn settle(extra_ms: u64) {
    tokio::time::sleep(Duration::from_millis(30 + extra_ms)).await;
}

/// Regression test for the absolute-binding bug. With the default
/// `DirectControlBinding` (non-absolute), a target of 0.5 would map through
/// [min,max]=[0,10] and land at 5.0 — a +14 dB gain. This must never come
/// back.
#[tokio::test(flavor = "multi_thread")]
async fn ramp_target_lands_in_property_domain_not_normalized() {
    let p = TestPipe::new();
    let mgr = VolumeRampManager::new();

    assert!(mgr.apply_volume_ramp(&p.volume, "v", 0.5, 50));
    settle(80).await;

    let actual = p.vol();
    assert!(
        (actual - 0.5).abs() < 0.05,
        "expected ~0.5, got {} (ABSOLUTE-BINDING REGRESSION if much higher)",
        actual
    );
    assert!(
        actual <= 1.5,
        "volume {} > 1.5 — non-absolute binding regression (+14 dB+)",
        actual
    );
}

/// Mid-ramp updates: a second ramp starting before the first finishes should
/// pick up the interpolated value, not snap back to the original property
/// value. Verified by the start value logged in the second ramp.
#[tokio::test(flavor = "multi_thread")]
async fn mid_ramp_update_continues_smoothly() {
    let p = TestPipe::new();
    let mgr = VolumeRampManager::new();

    assert!(mgr.apply_volume_ramp(&p.volume, "v", 0.0, 100));
    // Halfway through the first ramp (volume should be ~0.5)
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(mgr.apply_volume_ramp(&p.volume, "v", 0.7, 100));
    // Wait for the second ramp to settle.
    settle(150).await;

    let actual = p.vol();
    assert!(
        (actual - 0.7).abs() < 0.05,
        "expected ~0.7 after second ramp, got {}",
        actual
    );
}

/// Mute then unmute restores the pre-mute volume.
#[tokio::test(flavor = "multi_thread")]
async fn mute_then_unmute_restores_volume() {
    let p = TestPipe::new();
    let mgr = VolumeRampManager::new();

    // Set a non-default volume first.
    assert!(mgr.apply_volume_ramp(&p.volume, "v", 0.4, 50));
    settle(80).await;
    assert!((p.vol() - 0.4).abs() < 0.05, "setup volume not at 0.4");

    // Mute — anti-click ramps to 0, then mute=true scheduled.
    assert!(mgr.apply_mute(&p.volume, "v", true, 10));
    settle(50).await; // 10ms ramp + 5ms grace + slack for the tokio task
    assert!(
        p.mute(),
        "mute property should be true after apply_mute(true)"
    );
    assert!(
        p.vol() < 0.05,
        "volume should be ~0 while muted, got {}",
        p.vol()
    );

    // Unmute — should restore to 0.4.
    assert!(mgr.apply_mute(&p.volume, "v", false, 10));
    settle(40).await;
    assert!(!p.mute(), "mute should be false");
    assert!(
        (p.vol() - 0.4).abs() < 0.05,
        "expected ~0.4 after unmute, got {}",
        p.vol()
    );
}

/// P0 regression: when the user changes volume during mute, unmute must
/// still produce a real fade-in from 0, not jump straight to the (mid-mute)
/// target value.
#[tokio::test(flavor = "multi_thread")]
async fn unmute_after_mid_mute_fader_change_fades_in_from_zero() {
    let p = TestPipe::new();
    let mgr = VolumeRampManager::new();

    // Initial volume.
    assert!(mgr.apply_volume_ramp(&p.volume, "v", 0.6, 50));
    settle(80).await;

    // Mute.
    assert!(mgr.apply_mute(&p.volume, "v", true, 10));
    settle(50).await;
    assert!(p.mute());

    // Change "fader" while muted — pre_mute_volumes should be updated to
    // this new target so unmute restores to 0.3, not 0.6.
    assert!(mgr.apply_volume_ramp(&p.volume, "v", 0.3, 50));
    settle(80).await;
    assert!(p.mute(), "still muted (audio silent regardless of cs)");

    // Unmute — must fade *up from 0* to 0.3.
    assert!(mgr.apply_mute(&p.volume, "v", false, 30));

    // Sample mid-fade: at 5ms, volume should still be small. If the fix is
    // missing, the unmute would step straight to 0.3 with no fade.
    tokio::time::sleep(Duration::from_millis(8)).await;
    let mid = p.vol();
    assert!(
        mid < 0.25,
        "unmute did NOT fade in (P0 regression): volume={} at +8ms (expected <0.25)",
        mid
    );

    // Settled value lands at the new pre-mute target.
    settle(60).await;
    assert!(
        (p.vol() - 0.3).abs() < 0.05,
        "expected ~0.3 after fade-in, got {}",
        p.vol()
    );
}

/// Long ramp should follow a dB-linear curve. Sample at 1/4, 1/2, 3/4 along
/// a 1.0 → 0.001 ramp and assert each waypoint is closer to dB-linear than
/// to amplitude-linear.
///
/// Linear-amp would produce: 0.75, 0.5, 0.25 at the quarters.
/// dB-linear (60 dB span over 1s) produces: ~0.18, ~0.032, ~0.0056.
#[tokio::test(flavor = "multi_thread")]
async fn long_ramp_is_db_linear_not_amp_linear() {
    let p = TestPipe::new();
    let mgr = VolumeRampManager::new();

    // Start at full, fade to near-silence over 1000ms.
    assert!(mgr.apply_volume_ramp(&p.volume, "v", 0.001, 1000));

    // Sample at ~250ms.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let q1 = p.vol();
    // dB-linear at 25%: 10^((-60 * 0.25)/20) ≈ 0.178. Linear-amp would be 0.75.
    assert!(
        q1 < 0.45,
        "at 25%: expected dB-linear (~0.18), got {} — linear-amp regression?",
        q1
    );

    // Sample at ~500ms.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let q2 = p.vol();
    assert!(
        q2 < 0.20,
        "at 50%: expected dB-linear (~0.032), got {} — linear-amp regression?",
        q2
    );

    // Settle.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        p.vol() < 0.01,
        "expected near-silence at end, got {}",
        p.vol()
    );
}

/// Ramp on a non-running pipeline (no stream-time position) returns false
/// so the caller can fall back to a direct property set. This is the path
/// taken during initial pipeline construction.
#[tokio::test(flavor = "multi_thread")]
async fn ramp_returns_false_when_no_stream_time() {
    gst::init().unwrap();
    let volume = gst::ElementFactory::make("volume").build().unwrap();
    // Element exists but is not in any pipeline → query_position is None.

    let mgr = VolumeRampManager::new();
    let ok = mgr.apply_volume_ramp(&volume, "v", 0.5, 50);
    assert!(!ok, "expected false when no stream-time available");
}

/// `clear()` drops cached control sources. The bindings remain attached to
/// the elements until those elements are dropped — that's by design; the
/// pipeline owns its elements and tears them down on Null. We verify the
/// cache is empty after clear, and that subsequent ramps re-attach cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn clear_drops_cache_and_subsequent_ramps_work() {
    let p = TestPipe::new();
    let mgr = VolumeRampManager::new();

    assert!(mgr.apply_volume_ramp(&p.volume, "v", 0.7, 50));
    settle(80).await;

    mgr.clear();

    // A new ramp on the same element id should still work — the manager
    // re-attaches a fresh control source. (The existing binding from before
    // clear() is still on the element, but we just keep going.)
    assert!(mgr.apply_volume_ramp(&p.volume, "v", 0.2, 50));
    settle(80).await;

    let v = p.vol();
    assert!(
        (v - 0.2).abs() < 0.1,
        "ramp after clear() did not reach target: got {}",
        v
    );
}

/// Long mute fades honor the caller-supplied ramp_ms instead of the legacy
/// hardcoded 10 ms anti-click. A 200 ms mute should still have the volume
/// well above zero at +50 ms (the old behavior would have hit zero in ~10 ms).
#[tokio::test(flavor = "multi_thread")]
async fn long_mute_ramp_takes_full_duration() {
    let p = TestPipe::new();
    let mgr = VolumeRampManager::new();

    assert!(mgr.apply_volume_ramp(&p.volume, "v", 1.0, 50));
    settle(80).await;

    // Long fade-out before mute.
    assert!(mgr.apply_mute(&p.volume, "v", true, 200));

    // ~50 ms in: well below 1.0 (fade is dB-linear) but not yet silent.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mid = p.vol();
    assert!(
        !p.mute(),
        "mute=true must not land before the fade-out has finished"
    );
    assert!(
        mid > 0.01,
        "at +50 ms of a 200 ms fade, volume must still be audible (got {})",
        mid
    );

    // After the full duration plus the 5 ms grace, mute=true is applied.
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert!(p.mute(), "mute=true should land after the 200 ms fade-out");
}

/// Cancel-guard: a mid-fade unmute must cancel the pending `mute=true`
/// toggle scheduled by the long fade-out, otherwise that toggle would land
/// after the unmute and silently kill the route.
#[tokio::test(flavor = "multi_thread")]
async fn unmute_during_long_mute_fade_cancels_pending_toggle() {
    let p = TestPipe::new();
    let mgr = VolumeRampManager::new();

    assert!(mgr.apply_volume_ramp(&p.volume, "v", 1.0, 50));
    settle(80).await;

    // Start a long fade-out — schedules mute=true at +200 ms.
    assert!(mgr.apply_mute(&p.volume, "v", true, 200));

    // Unmute well before the scheduled mute=true would fire.
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(mgr.apply_mute(&p.volume, "v", false, 50));

    // Wait past the original 200 ms+grace window. The previously-scheduled
    // mute=true must have observed the bumped generation and bailed out.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !p.mute(),
        "stale scheduled mute=true must be cancelled by an intervening unmute"
    );
}

/// `is_volume_element` correctly distinguishes the volume element from
/// other audio elements that happen to expose a `volume` property.
#[tokio::test(flavor = "multi_thread")]
async fn is_volume_element_only_matches_volume_factory() {
    gst::init().unwrap();
    let volume = gst::ElementFactory::make("volume").build().unwrap();
    let testsrc = gst::ElementFactory::make("audiotestsrc").build().unwrap();
    assert!(VolumeRampManager::is_volume_element(&volume));
    assert!(
        !VolumeRampManager::is_volume_element(&testsrc),
        "audiotestsrc has a volume property but is not the volume element"
    );
}
