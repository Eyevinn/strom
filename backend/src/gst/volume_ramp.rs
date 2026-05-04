//! Smooth volume transitions for audio `volume` elements.
//!
//! Sets up a `GstInterpolationControlSource` on the `volume` property so the
//! GStreamer `volume` element samples the value per-sample (see
//! `volume_transform_ip` in `gstvolume.c`), eliminating the zipper noise that
//! a direct `set_property` step change produces.
//!
//! Also provides anti-click mute handling: a brief volume ramp toward zero is
//! applied before the `mute` boolean is toggled, masking the click that would
//! otherwise occur when a hard mute lands mid-waveform.
//!
//! Long ramps (> `LONG_RAMP_THRESHOLD_MS`) are laid out along a dB-linear
//! curve. Linear amplitude interpolation sounds wrong over long durations
//! (the last quarter contains barely-audible energy); dB-linear sounds even,
//! matching how broadcast/DAW gear behaves.
//!
//! Lifecycle: bindings are owned by the `gst::Element` they attach to, so they
//! are torn down with the element. `clear()` drops the cached control sources
//! when the pipeline manager is shut down.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_controller::prelude::*;
use gstreamer_controller::{DirectControlBinding, InterpolationControlSource, InterpolationMode};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tracing::{debug, warn};

/// Below this duration we use straight linear amplitude interpolation (2
/// keyframes). The audible difference vs. dB-linear is negligible for
/// short anti-zipper / anti-click ramps, and avoids overhead.
const LONG_RAMP_THRESHOLD_MS: u32 = 50;

/// Number of keyframes laid out along the dB-linear curve for long ramps.
/// 12 gives smooth perceptual fades — the volume element still interpolates
/// per-sample between them, so the curve is continuous, not stepped.
const LOG_KEYFRAME_COUNT: usize = 12;

/// dB floor for log conversion. Amplitudes below this are treated as zero.
/// −60 dB ≈ 0.001 in amplitude — well below audible for a fade target.
const LOG_DB_FLOOR: f64 = -60.0;

/// Manages per-element volume control sources and pre-mute volume state.
///
/// One instance per `PipelineManager`. Element IDs are unique within a flow,
/// so we don't need flow-scoped keys here.
pub struct VolumeRampManager {
    /// Active volume control sources keyed by element id. Kept alive while the
    /// binding is attached to the element.
    sources: Mutex<HashMap<String, InterpolationControlSource>>,
    /// Pre-mute volume per element id, captured when entering mute so we can
    /// restore it on unmute.
    pre_mute: Mutex<HashMap<String, f64>>,
}

impl VolumeRampManager {
    pub fn new() -> Self {
        Self {
            sources: Mutex::new(HashMap::new()),
            pre_mute: Mutex::new(HashMap::new()),
        }
    }

    /// True if `element` is a standalone `volume` element. Other elements may
    /// expose a `volume` property (e.g. `audiomixer` sink pads), but this
    /// module only handles the dedicated `volume` element used by the audio
    /// mixer block.
    pub fn is_volume_element(element: &gst::Element) -> bool {
        element
            .factory()
            .map(|f| f.name() == "volume")
            .unwrap_or(false)
    }

    /// Schedule a smooth interpolation of the `volume` property to `target`
    /// over `ramp_ms` milliseconds. Starts from the current interpolated
    /// value (or the property value on first call), so chained drag updates
    /// don't produce hops. Returns `false` if the element does not yet have
    /// a stream-time position — caller should fall back to a direct
    /// `set_property`.
    pub fn apply_volume_ramp(
        &self,
        element: &gst::Element,
        element_id: &str,
        target: f64,
        ramp_ms: u32,
    ) -> bool {
        self.apply_volume_ramp_inner(element, element_id, None, target, ramp_ms)
    }

    /// Same as `apply_volume_ramp` but forces the ramp to start from
    /// `start_value` regardless of the current control-source state. Used by
    /// `apply_mute(false)` to force `0.0 → target` even when the control
    /// source already has keyframes at `target` from a fader change made
    /// while muted (which would otherwise produce a step-on instead of a
    /// fade-in).
    pub fn apply_volume_ramp_from(
        &self,
        element: &gst::Element,
        element_id: &str,
        start_value: f64,
        target: f64,
        ramp_ms: u32,
    ) -> bool {
        self.apply_volume_ramp_inner(element, element_id, Some(start_value), target, ramp_ms)
    }

    fn apply_volume_ramp_inner(
        &self,
        element: &gst::Element,
        element_id: &str,
        forced_start: Option<f64>,
        target: f64,
        ramp_ms: u32,
    ) -> bool {
        let Some(now) = element.query_position::<gst::ClockTime>() else {
            debug!(
                "volume_ramp[{}]: no stream-time position, falling back to direct set",
                element_id
            );
            return false;
        };
        let duration = gst::ClockTime::from_mseconds(ramp_ms as u64);

        let mut sources = self.sources.lock().unwrap();
        let cs = sources.entry(element_id.to_string()).or_insert_with(|| {
            let cs = InterpolationControlSource::new();
            cs.set_mode(InterpolationMode::Linear);
            // ABSOLUTE binding: keyframe values pass through directly as the
            // property value. The non-absolute DirectControlBinding maps
            // [0,1] → [min, max], which would silently scale our intended
            // gain by the volume element's max (10.0 = +20 dB) and cause
            // distortion. We're storing keyframes in property domain, so
            // absolute is the correct mode.
            let binding = DirectControlBinding::new_absolute(element, "volume", &cs);
            if let Err(e) = element.add_control_binding(&binding) {
                warn!(
                    "volume_ramp[{}]: failed to attach control binding: {}",
                    element_id, e
                );
            } else {
                debug!(
                    "volume_ramp[{}]: attached InterpolationControlSource on volume (absolute)",
                    element_id
                );
            }
            cs
        });

        // Resolve start value: explicit override (used by unmute), otherwise
        // the interpolated control-source value at `now` (mid-ramp continuity),
        // falling back to the property value on first attach.
        let start = forced_start.unwrap_or_else(|| {
            // Disambiguated via ControlSourceExt — `value` also lives on GstObjectExt.
            gstreamer::prelude::ControlSourceExt::value(cs, now)
                .unwrap_or_else(|| element.property::<f64>("volume"))
        });

        // Reset accumulated keyframes to keep memory bounded across drags.
        cs.unset_all();
        if !lay_out_keyframes(cs, now, duration, start, target, ramp_ms) {
            warn!("volume_ramp[{}]: failed to set keyframes", element_id);
            return false;
        }

        // Mirror the new target onto pre_mute when the element is currently
        // muted, so a later unmute restores the value the user expected.
        if element.property::<bool>("mute") && target > 0.0 {
            self.pre_mute
                .lock()
                .unwrap()
                .insert(element_id.to_string(), target);
        }

        debug!(
            "volume_ramp[{}]: {:.4} -> {:.4} over {}ms ({})",
            element_id,
            start,
            target,
            ramp_ms,
            if ramp_ms > LONG_RAMP_THRESHOLD_MS {
                "log"
            } else {
                "linear"
            }
        );
        true
    }

    /// Toggle `mute` with anti-click protection. Ramps `volume` toward zero
    /// before `mute=true` is applied (masking the discontinuity click), and
    /// restores the pre-mute volume on unmute.
    ///
    /// Falls back to a direct `set_property` if the pipeline doesn't have a
    /// running stream-time yet.
    pub fn apply_mute(
        &self,
        element: &gst::Element,
        element_id: &str,
        target_mute: bool,
        anticlick_ms: u32,
    ) -> bool {
        if target_mute {
            // Capture pre-mute volume only if not already muted (avoid
            // overwriting on repeat-mute).
            let already_muted = element.property::<bool>("mute");
            if !already_muted {
                let current = element.property::<f64>("volume");
                if current > 0.0 {
                    self.pre_mute
                        .lock()
                        .unwrap()
                        .insert(element_id.to_string(), current);
                }
            }

            // Ramp to silence first.
            if !self.apply_volume_ramp(element, element_id, 0.0, anticlick_ms) {
                element.set_property("mute", true);
                return true;
            }

            // Schedule the boolean toggle to land just after the ramp ends.
            // Using tokio because the API path runs inside the tokio runtime.
            // A small extra margin (5ms) ensures the volume control source
            // has reached zero before mute kicks in — otherwise the hard
            // zeroing of the volume array would still produce a click.
            let element_weak = element.downgrade();
            let element_id_owned = element_id.to_string();
            let delay = Duration::from_millis(anticlick_ms as u64 + 5);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Some(elem) = element_weak.upgrade() {
                    elem.set_property("mute", true);
                    debug!(
                        "volume_ramp[{}]: mute=true applied after anti-click ramp",
                        element_id_owned
                    );
                }
            });
        } else {
            // Unmute: clear the boolean first so the controlled-volume path
            // in volume_transform_ip is not bypassed by the muted shortcut.
            element.set_property("mute", false);

            let target = self
                .pre_mute
                .lock()
                .unwrap()
                .remove(element_id)
                .unwrap_or(1.0);

            // Force start from 0 — if the user changed volume during mute,
            // the control source's keyframes may already point at `target`,
            // and a normal apply_volume_ramp would produce a flat ramp
            // (instant unmute = click). Forcing start=0 guarantees a real
            // 0→target fade-in.
            if !self.apply_volume_ramp_from(element, element_id, 0.0, target, anticlick_ms) {
                element.set_property("volume", target);
            }
        }
        true
    }

    /// Drop all cached control sources. Bindings are owned by the elements
    /// and are released when the pipeline drops; this just releases our side.
    pub fn clear(&self) {
        self.sources.lock().unwrap().clear();
        self.pre_mute.lock().unwrap().clear();
    }
}

impl Default for VolumeRampManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Insert keyframes between `(now, start)` and `(now+duration, target)`.
/// Short ramps get a straight linear pair; long ramps get a dB-linear curve
/// with `LOG_KEYFRAME_COUNT` waypoints.
fn lay_out_keyframes(
    cs: &InterpolationControlSource,
    now: gst::ClockTime,
    duration: gst::ClockTime,
    start: f64,
    target: f64,
    ramp_ms: u32,
) -> bool {
    let end = now + duration;

    // Short ramp: just two keyframes. Linear amplitude is fine.
    if ramp_ms <= LONG_RAMP_THRESHOLD_MS {
        return cs.set(now, start) && cs.set(end, target);
    }

    // Long ramp: dB-linear curve with multiple keyframes. The volume element
    // still interpolates per-sample between them (linearly in amplitude),
    // but with enough keyframes the audible result follows the dB curve.
    let start_db = amp_to_db(start);
    let end_db = amp_to_db(target);
    let target_is_zero = target <= 0.0;
    let dur_ns = duration.nseconds() as f64;

    for i in 0..=LOG_KEYFRAME_COUNT {
        let t = i as f64 / LOG_KEYFRAME_COUNT as f64;

        // Approach to true silence: a dB curve never reaches 0, so reserve
        // the last 5% of the ramp for a linear segment from `db_to_amp(95%)`
        // down to 0. Avoids a trailing inaudible tail.
        let amp = if target_is_zero && t >= 0.95 {
            let amp_at_95 = db_to_amp(start_db + 0.95 * (end_db - start_db));
            let linear_t = (t - 0.95) / 0.05;
            amp_at_95 * (1.0 - linear_t)
        } else {
            db_to_amp(start_db + t * (end_db - start_db))
        };

        let time = now + gst::ClockTime::from_nseconds((dur_ns * t) as u64);
        if !cs.set(time, amp) {
            return false;
        }
    }
    true
}

fn amp_to_db(amp: f64) -> f64 {
    if amp <= 0.0 {
        LOG_DB_FLOOR
    } else {
        (20.0 * amp.log10()).max(LOG_DB_FLOOR)
    }
}

fn db_to_amp(db: f64) -> f64 {
    if db <= LOG_DB_FLOOR {
        0.0
    } else {
        10f64.powf(db / 20.0)
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn db_amp_round_trip() {
        // Values at or below LOG_DB_FLOOR map to silence by design and are
        // not expected to round-trip — see `db_handles_zero_and_below_floor`.
        for amp in [0.01, 0.1, 0.5, 1.0, 2.0, 10.0] {
            let db = amp_to_db(amp);
            let back = db_to_amp(db);
            assert!(
                (back - amp).abs() / amp < 1e-9,
                "round-trip failed: {} -> {} -> {}",
                amp,
                db,
                back
            );
        }
    }

    #[test]
    fn db_handles_zero_and_below_floor() {
        assert_eq!(amp_to_db(0.0), LOG_DB_FLOOR);
        assert_eq!(amp_to_db(-0.5), LOG_DB_FLOOR);
        assert_eq!(db_to_amp(LOG_DB_FLOOR), 0.0);
        assert_eq!(db_to_amp(-100.0), 0.0);
    }
}
