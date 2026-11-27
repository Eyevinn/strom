//! Mapping from GStreamer running time to the TAI timeline of a TAMS store.
//!
//! Every TAMS Output in one pipeline must place a buffer with a given running time
//! at the same TAI position, so that synchronized angles recorded by one flow line
//! up frame for frame in the store (#966). Sampling a TAI clock per block when its
//! first fragment opens does not give that: each block samples at a different
//! moment, so the angles drift apart by however long their first buffers took to
//! arrive.
//!
//! Instead the mapping is `TAI = clock_time + offset`, where `clock_time =
//! base_time + running_time` is the pipeline clock's time for a buffer and
//! `offset` is fixed per pipeline:
//! - pipeline clock is TAI (a `Tai` system clock or a PTP clock): `offset = 0`;
//! - pipeline clock is UTC (a `Realtime` system clock): `offset = TAI - UTC`;
//! - any other clock (`Monotonic`, NTP): `offset` is sampled once per pipeline run
//!   from the system TAI clock and shared by every block in that pipeline.
//!
//! The `Other` offset is deliberately not re-sampled during a run. A monotonic
//! or NTP clock drifts against disciplined TAI by tens of ppm, so over hours the
//! store's TAI moves away from wall-clock capture time (about 1-2 s over 10 h at
//! 50 ppm). Angles of one pipeline share the offset and stay aligned with each
//! other; UTC addressing and joining stores of other pipelines suffer. Re-sampling
//! per block would break that alignment, since blocks would see different
//! samples at the same running time. Long recordings that need wall-clock
//! accuracy should run the pipeline on a TAI, PTP or Realtime clock, which map
//! by a fixed offset, or carry reference timestamps.
//!
//! The one exception is a jump: when the gap between `clock_time + offset` and
//! host TAI changes by more than [`REANCHOR_THRESHOLD_NS`] from one check to the
//! next, the offset is re-sampled. On macOS the monotonic clock stops while the
//! machine sleeps, so without this every segment after a wake was stamped as far
//! in the past as the sleep lasted. The new offset is stored in the shared entry
//! with a new [`PipelineTaiMapping::epoch`], so every block of the pipeline
//! re-anchors on the same offset and the angles stay aligned. Blocks that stamp
//! every buffer take the new offset from a clock time they all agree on (see
//! [`SharedStamps`]), so branches never stamp one running time differently.
//!
//! Drift is kept apart from jumps. The gap is checked two ways, and either one
//! re-anchors:
//! - against the gap at the previous check (every fragment): a step of more
//!   than the threshold, plus [`MAX_DRIFT_PPM`] of the time between the two
//!   checks, is a jump;
//! - against zero, the gap when the mapping was anchored: a total of more than
//!   the threshold plus [`MAX_DRIFT_PPM`] of the time since then is a jump too.
//!   Without this, steps each below the threshold (a few short sleeps) would
//!   add up without limit.
//!
//! Real drift stays within both allowances, since they grow at the largest
//! drift rate a clock is taken to have: at 50 ppm it reaches 5 s after about
//! 28 h, and re-anchoring then would cut a 5 s hole in the store, or put angles
//! 5 s apart. The accumulated drift is logged for each whole second it grows.
//!
//! A buffer that carries a capture time as a `GstReferenceTimestampMeta`
//! (`timestamp/x-tai`, `x-ptp`, `x-unix` or `x-ntp`) takes precedence: the block
//! anchors on it instead of the pipeline clock.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use strom_types::FlowId;
use tracing::{info, warn};

/// TAI - UTC (37 s, fixed since the 2026 CGPM decision). Used as the fallback
/// when the kernel has no TAI offset set (macOS, or Linux without chrony
/// `leapsectz`).
pub use strom_types::tai::TAI_MINUS_UTC_NS;

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_TO_UNIX_NS: u64 = 2_208_988_800 * 1_000_000_000;

/// Below this, CLOCK_TAI and CLOCK_REALTIME are taken to be the same clock (the
/// kernel's TAI offset is unset).
const UNSET_OFFSET_THRESHOLD_NS: u64 = 1_000_000_000;

/// How far an [`ClockDomain::Other`] mapping may stray from host TAI before it is
/// re-sampled (the pipeline clock stopped during a sleep, or the wall clock was
/// stepped).
///
/// Compared with how much the gap changed since the previous check, not with
/// the gap itself, so drift does not count (see the module docs).
pub const REANCHOR_THRESHOLD_NS: u64 = 5_000_000_000;

/// Largest drift of a pipeline clock against host TAI taken as drift, in parts
/// per million of the time between two checks. Crystal oscillators are
/// specified to 50-100 ppm.
pub const MAX_DRIFT_PPM: u128 = 100;

/// Which timescale a pipeline clock runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockDomain {
    /// The clock reads TAI nanoseconds since 1970-01-01 TAI.
    Tai,
    /// The clock reads UTC (Unix) nanoseconds since 1970-01-01.
    Utc,
    /// Anything else (monotonic, NTP-disciplined network clock, unknown).
    Other,
}

/// Classify a pipeline clock.
pub fn clock_domain(clock: &gst::Clock) -> ClockDomain {
    match clock.type_().name() {
        // The PTP timescale is TAI.
        "GstPtpClock" => ClockDomain::Tai,
        "GstSystemClock" => match clock.property::<gst::ClockType>("clock-type") {
            // GStreamer falls back to the monotonic clock for `Tai` where the OS
            // has no CLOCK_TAI (macOS), so check that it reads TAI before using it.
            gst::ClockType::Tai if is_plausible_tai(clock.time().nseconds(), utc_now_ns()) => {
                ClockDomain::Tai
            }
            gst::ClockType::Realtime => ClockDomain::Utc,
            _ => ClockDomain::Other,
        },
        _ => ClockDomain::Other,
    }
}

/// Reads TAI from the host clocks, with the [`TAI_MINUS_UTC_NS`] fallback when the
/// kernel has no TAI offset.
pub struct HostTai {
    tai: gst::Clock,
    realtime: gst::Clock,
}

impl Default for HostTai {
    fn default() -> Self {
        Self::new()
    }
}

impl HostTai {
    pub fn new() -> Self {
        let make = |t: gst::ClockType| -> gst::Clock {
            gst::glib::Object::builder::<gst::SystemClock>()
                .property("clock-type", t)
                .build()
                .upcast()
        };
        Self {
            tai: make(gst::ClockType::Tai),
            realtime: make(gst::ClockType::Realtime),
        }
    }

    /// Current TAI in nanoseconds since 1970-01-01 TAI.
    pub fn now_ns(&self) -> u64 {
        let tai = self.tai.time().nseconds();
        let utc = self.realtime.time().nseconds();
        tai_from_host_clocks(tai, utc)
    }

    /// Current TAI - UTC of this host in nanoseconds (the kernel's offset, or the
    /// [`TAI_MINUS_UTC_NS`] fallback). Rounded to whole seconds, as TAI - UTC
    /// always is, so the two clock reads taken one after the other do not leak in.
    pub fn tai_minus_utc_ns(&self) -> u64 {
        let tai = self.tai.time().nseconds();
        let utc = self.realtime.time().nseconds();
        let diff = tai_from_host_clocks(tai, utc).saturating_sub(utc);
        (diff + 500_000_000) / 1_000_000_000 * 1_000_000_000
    }
}

/// Upper bound on a believable TAI - UTC reading. Anything further from UTC is not
/// a TAI clock at all.
const MAX_TAI_MINUS_UTC_NS: u64 = 100_000_000_000;

fn utc_now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Whether a clock reading looks like TAI with a kernel offset set: ahead of UTC
/// by more than [`UNSET_OFFSET_THRESHOLD_NS`] and at most [`MAX_TAI_MINUS_UTC_NS`].
fn is_plausible_tai(tai_ns: u64, utc_ns: u64) -> bool {
    tai_ns > utc_ns
        && (UNSET_OFFSET_THRESHOLD_NS..=MAX_TAI_MINUS_UTC_NS).contains(&(tai_ns - utc_ns))
}

/// Combine CLOCK_TAI and CLOCK_REALTIME readings into TAI. When CLOCK_TAI does not
/// read TAI (the kernel offset is unset so it equals UTC, or the OS has no
/// CLOCK_TAI and GStreamer fell back to the monotonic clock, as on macOS), TAI is
/// UTC + [`TAI_MINUS_UTC_NS`].
fn tai_from_host_clocks(tai_ns: u64, utc_ns: u64) -> u64 {
    if is_plausible_tai(tai_ns, utc_ns) {
        return tai_ns;
    }
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        info!(
            "TAMS: host CLOCK_TAI does not read TAI (no kernel TAI offset, or no CLOCK_TAI \
             on this OS); using TAI = UTC + {} s (leap seconds ended by CGPM 2026 \
             Resolution 5)",
            TAI_MINUS_UTC_NS / 1_000_000_000
        );
    }
    utc_ns + TAI_MINUS_UTC_NS
}

/// `TAI = clock_time + offset_ns` for one pipeline run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineTaiMapping {
    pub offset_ns: i128,
    pub domain: ClockDomain,
    /// Bumped each time the shared mapping of a pipeline run is re-sampled after a
    /// jump. A block anchored on an older epoch re-anchors.
    pub epoch: u32,
}

impl PipelineTaiMapping {
    /// Build the mapping for `clock`. Only an [`ClockDomain::Other`] clock is
    /// sampled against host TAI; TAI and UTC clocks map by a fixed offset, so every
    /// node on a shared clock computes the same mapping.
    pub fn for_clock(clock: &gst::Clock, tai: &HostTai) -> Self {
        let domain = clock_domain(clock);
        let offset_ns = match domain {
            ClockDomain::Tai => 0,
            ClockDomain::Utc => tai.tai_minus_utc_ns() as i128,
            ClockDomain::Other => {
                let (clock_ns, tai_ns) = sample_other(clock, tai);
                tai_ns as i128 - clock_ns
            }
        };
        Self {
            offset_ns,
            domain,
            epoch: 0,
        }
    }

    /// TAI of a buffer with this running time in a pipeline with this base time.
    pub fn tai_ns(&self, base_time_ns: u64, running_time_ns: u64) -> u64 {
        clamp_ns(base_time_ns as i128 + running_time_ns as i128 + self.offset_ns)
    }
}

/// Read the pipeline clock and host TAI at (about) the same moment: the clock is
/// read before and after host TAI and averaged.
fn sample_other(clock: &gst::Clock, tai: &HostTai) -> (i128, u64) {
    let before = clock.time().nseconds() as i128;
    let tai_now = tai.now_ns();
    let after = clock.time().nseconds() as i128;
    ((before + after) / 2, tai_now)
}

struct RegistryEntry {
    base_time_ns: u64,
    clock: gst::glib::WeakRef<gst::Clock>,
    mapping: PipelineTaiMapping,
    /// Host TAI and the gap (host TAI minus mapped clock time) at the last
    /// check of an [`ClockDomain::Other`] mapping.
    last_check: (u64, i128),
    /// Host TAI when the mapping was (re)anchored; the gap was zero then.
    anchored_tai_ns: u64,
    /// Whole seconds of drift already logged.
    drift_logged_s: u128,
    /// When a block last asked for the mapping.
    last_used: Instant,
    /// The offsets stampers of this run apply, and from which clock time.
    stamps: Arc<SharedStamps>,
}

/// Offsets a [`SharedStamps`] keeps, the one in force included. Older ones are
/// only needed by a branch that runs more re-anchors behind the others.
const STAMP_HISTORY: usize = 8;

/// Running time -> TAI for stamping buffers, shared by every stamper of one
/// pipeline run, so equal running times get equal stamps in every branch,
/// across a re-anchor too.
///
/// A re-anchor is noticed by whichever block asks the registry first. Another
/// branch, which has not asked since, would go on stamping with the old offset,
/// so for a while the same running time would get stamps seconds apart. So a
/// new offset applies from a clock time every stamper agrees on: one past the
/// latest clock time any of them has stamped. Clock times up to there keep the
/// old offset, whichever branch stamps them and when; later ones get the new
/// one. Each buffer goes through [`Self::tai_ns`], which records its clock time
/// and picks the offset in force for it.
#[derive(Debug)]
pub struct SharedStamps {
    inner: Mutex<StampHistory>,
}

#[derive(Debug)]
struct StampHistory {
    /// The latest clock time (`base_time + running_time`) stamped so far.
    stamped_through_ns: Option<u64>,
    /// `(from clock time, offset)`, oldest first. The first entry also covers
    /// clock times before its own.
    offsets: Vec<(u64, i128)>,
}

impl SharedStamps {
    fn new(offset_ns: i128) -> Self {
        Self {
            inner: Mutex::new(StampHistory {
                stamped_through_ns: None,
                offsets: vec![(0, offset_ns)],
            }),
        }
    }

    /// TAI of a buffer with this running time in a pipeline with this base time.
    /// Records the buffer's clock time as stamped.
    pub fn tai_ns(&self, base_time_ns: u64, running_time_ns: u64) -> u64 {
        let clock_ns = base_time_ns.saturating_add(running_time_ns);
        let mut h = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        h.stamped_through_ns = Some(h.stamped_through_ns.map_or(clock_ns, |t| t.max(clock_ns)));
        let offset = h
            .offsets
            .iter()
            .rev()
            .find(|(from, _)| *from <= clock_ns)
            .unwrap_or(&h.offsets[0])
            .1;
        clamp_ns(clock_ns as i128 + offset)
    }

    /// Apply `offset_ns` from one past the latest clock time stamped so far.
    fn reanchor(&self, offset_ns: i128) {
        let mut h = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match h.stamped_through_ns {
            None => h.offsets = vec![(0, offset_ns)],
            Some(t) => {
                let from = t.saturating_add(1);
                h.offsets.retain(|(f, _)| *f < from);
                h.offsets.push((from, offset_ns));
                if h.offsets.len() > STAMP_HISTORY {
                    h.offsets.remove(0);
                }
            }
        }
    }
}

/// Gap a clock may drift in `since_ns` of host TAI before it counts as a jump.
fn drift_allowance(since_ns: u64) -> u128 {
    REANCHOR_THRESHOLD_NS as u128 + since_ns as u128 * MAX_DRIFT_PPM / 1_000_000
}

/// Whether the gap between mapped clock time and host TAI moved by more than
/// drift explains: `gap` now, `last_gap` at the last check, `since_check_ns`
/// and `since_anchor_ns` of host TAI since the last check and since the
/// mapping was anchored (when the gap was zero).
fn is_jump(gap: i128, last_gap: i128, since_check_ns: u64, since_anchor_ns: u64) -> bool {
    (gap - last_gap).unsigned_abs() > drift_allowance(since_check_ns)
        || gap.unsigned_abs() > drift_allowance(since_anchor_ns)
}

/// An entry no block has asked for in this long is dropped when another
/// pipeline run registers. A running pipeline asks at least every fragment;
/// a paused one gets a new base time, and so a new entry, when it resumes.
const REGISTRY_IDLE: Duration = Duration::from_secs(3600);

/// One entry per flow, for its current pipeline run. Pruned of idle entries
/// (flows that stopped or were deleted) whenever an entry is added.
static REGISTRY: LazyLock<Mutex<HashMap<FlowId, RegistryEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The TAI mapping shared by all TAMS Outputs of one pipeline run.
///
/// The first block to ask samples it; later calls with the same flow, clock and
/// base time get the same mapping. A restarted pipeline has a new base time (or
/// clock), so it gets a fresh one.
///
/// Each call also checks an [`ClockDomain::Other`] mapping against host TAI and
/// re-samples it, with a new epoch, when the two have moved apart by more than
/// [`REANCHOR_THRESHOLD_NS`] (see the module docs). Callers that cache the
/// mapping should ask again now and then (every fragment, or every second).
pub fn shared_pipeline_mapping(
    flow_id: FlowId,
    clock: &gst::Clock,
    base_time_ns: u64,
    tai: &HostTai,
) -> PipelineTaiMapping {
    shared_pipeline_mapping_with(flow_id, clock, base_time_ns, tai, || {
        sample_other(clock, tai)
    })
}

/// [`shared_pipeline_mapping`], together with the [`SharedStamps`] of the same
/// pipeline run. A block that stamps buffers stamps them through it, so every
/// branch applies a re-anchor from the same clock time. It still asks again now
/// and then, since jumps are noticed here.
pub fn shared_pipeline_stamps(
    flow_id: FlowId,
    clock: &gst::Clock,
    base_time_ns: u64,
    tai: &HostTai,
) -> (PipelineTaiMapping, Arc<SharedStamps>) {
    shared_pipeline_entry_with(flow_id, clock, base_time_ns, tai, || {
        sample_other(clock, tai)
    })
}

/// [`shared_pipeline_mapping`] with the jump check reading `(pipeline clock time,
/// host TAI)` from `sample`, so tests can simulate a sleep.
fn shared_pipeline_mapping_with(
    flow_id: FlowId,
    clock: &gst::Clock,
    base_time_ns: u64,
    tai: &HostTai,
    sample: impl Fn() -> (i128, u64),
) -> PipelineTaiMapping {
    shared_pipeline_entry_with(flow_id, clock, base_time_ns, tai, sample).0
}

fn shared_pipeline_entry_with(
    flow_id: FlowId,
    clock: &gst::Clock,
    base_time_ns: u64,
    tai: &HostTai,
    sample: impl Fn() -> (i128, u64),
) -> (PipelineTaiMapping, Arc<SharedStamps>) {
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = reg.get_mut(&flow_id) {
        let same_clock = entry.clock.upgrade().as_ref() == Some(clock);
        if entry.base_time_ns == base_time_ns && same_clock {
            entry.last_used = Instant::now();
            if entry.mapping.domain == ClockDomain::Other {
                let (clock_ns, tai_ns) = sample();
                let gap = tai_ns as i128 - (clock_ns + entry.mapping.offset_ns);
                let (last_tai, last_gap) = entry.last_check;
                if is_jump(
                    gap,
                    last_gap,
                    tai_ns.saturating_sub(last_tai),
                    tai_ns.saturating_sub(entry.anchored_tai_ns),
                ) {
                    entry.mapping.offset_ns = tai_ns as i128 - clock_ns;
                    entry.mapping.epoch = entry.mapping.epoch.wrapping_add(1);
                    entry.stamps.reanchor(entry.mapping.offset_ns);
                    entry.last_check = (tai_ns, 0);
                    entry.anchored_tai_ns = tai_ns;
                    entry.drift_logged_s = 0;
                    warn!(
                        "TAMS: flow {} pipeline clock and host TAI moved apart (sleep or \
                         clock step; {} ms since the last check, {} ms in all): running \
                         time -> TAI re-anchored, offset {} ns (epoch {})",
                        flow_id,
                        (gap - last_gap) / 1_000_000,
                        gap / 1_000_000,
                        entry.mapping.offset_ns,
                        entry.mapping.epoch
                    );
                } else {
                    entry.last_check = (tai_ns, gap);
                    let drift_s = gap.unsigned_abs() / 1_000_000_000;
                    if drift_s > entry.drift_logged_s {
                        entry.drift_logged_s = drift_s;
                        info!(
                            "TAMS: flow {} pipeline clock has drifted {} ms from host TAI \
                             since it was anchored; segments follow the pipeline clock",
                            flow_id,
                            gap / 1_000_000
                        );
                    }
                }
            }
            return (entry.mapping, Arc::clone(&entry.stamps));
        }
    }
    let mapping = PipelineTaiMapping::for_clock(clock, tai);
    if mapping.domain == ClockDomain::Other {
        info!(
            "TAMS: flow {} runs on a {} clock: its TAI offset is sampled once and will \
             drift with that clock over long runs (use a TAI, PTP or Realtime pipeline \
             clock, or reference timestamps, for wall-clock accuracy)",
            flow_id,
            clock.type_().name()
        );
    }
    info!(
        "TAMS: flow {} running time -> TAI mapping: clock {} ({:?}), base_time {} ns, \
         offset {} ns",
        flow_id,
        clock.type_().name(),
        mapping.domain,
        base_time_ns,
        mapping.offset_ns
    );
    let now = Instant::now();
    reg.retain(|_, e| {
        e.clock.upgrade().is_some() && now.duration_since(e.last_used) < REGISTRY_IDLE
    });
    let tai_now = tai.now_ns();
    let stamps = Arc::new(SharedStamps::new(mapping.offset_ns));
    reg.insert(
        flow_id,
        RegistryEntry {
            base_time_ns,
            clock: clock.downgrade(),
            mapping,
            last_check: (tai_now, 0),
            anchored_tai_ns: tai_now,
            drift_logged_s: 0,
            last_used: now,
            stamps: Arc::clone(&stamps),
        },
    );
    (mapping, stamps)
}

/// Capture time of a buffer from its `GstReferenceTimestampMeta`, as TAI ns.
///
/// Recognizes `timestamp/x-tai` and `timestamp/x-ptp` (TAI), `timestamp/x-unix`
/// (UTC since 1970) and `timestamp/x-ntp` (UTC since 1900). The first matching
/// meta wins.
pub fn reference_tai_ns(buffer: &gst::BufferRef, tai_minus_utc_ns: u64) -> Option<u64> {
    buffer
        .iter_meta::<gst::ReferenceTimestampMeta>()
        .find_map(|meta| {
            let name = meta.reference().structure(0)?.name().to_string();
            reference_to_tai(&name, meta.timestamp().nseconds(), tai_minus_utc_ns)
        })
}

/// Convert a reference timestamp in the domain named by its caps to TAI ns.
pub fn reference_to_tai(caps_name: &str, ts_ns: u64, tai_minus_utc_ns: u64) -> Option<u64> {
    match caps_name {
        "timestamp/x-tai" | "timestamp/x-ptp" => Some(ts_ns),
        "timestamp/x-unix" => Some(ts_ns + tai_minus_utc_ns),
        "timestamp/x-ntp" => ts_ns
            .checked_sub(NTP_TO_UNIX_NS)
            .map(|unix| unix + tai_minus_utc_ns),
        _ => None,
    }
}

fn clamp_ns(v: i128) -> u64 {
    v.clamp(0, u64::MAX as i128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_clock(t: gst::ClockType) -> gst::Clock {
        gst::glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", t)
            .build()
            .upcast()
    }

    #[test]
    fn host_clocks_without_tai_offset_fall_back_to_37s() {
        let utc = 1_800_000_000_000_000_000;
        assert_eq!(tai_from_host_clocks(utc, utc), utc + TAI_MINUS_UTC_NS);
        assert_eq!(tai_from_host_clocks(utc + 5, utc), utc + TAI_MINUS_UTC_NS);
        // A kernel with the offset set is trusted as is.
        assert_eq!(
            tai_from_host_clocks(utc + 37_000_000_000, utc),
            utc + 37_000_000_000
        );
        // No CLOCK_TAI (macOS): GStreamer's Tai clock reads monotonic time.
        assert_eq!(
            tai_from_host_clocks(393_075_748_386_958, utc),
            utc + TAI_MINUS_UTC_NS
        );
    }

    #[test]
    fn clock_domains() {
        gst::init().unwrap();
        // Only TAI where the host has a CLOCK_TAI with an offset (not macOS).
        let tai_clock = system_clock(gst::ClockType::Tai);
        let expect = if is_plausible_tai(tai_clock.time().nseconds(), utc_now_ns()) {
            ClockDomain::Tai
        } else {
            ClockDomain::Other
        };
        assert_eq!(clock_domain(&tai_clock), expect);
        assert_eq!(
            clock_domain(&system_clock(gst::ClockType::Realtime)),
            ClockDomain::Utc
        );
        assert_eq!(
            clock_domain(&system_clock(gst::ClockType::Monotonic)),
            ClockDomain::Other
        );
    }

    #[test]
    fn tai_and_utc_clocks_map_by_fixed_offset() {
        gst::init().unwrap();
        let tai = HostTai::new();
        let m = PipelineTaiMapping {
            offset_ns: 0,
            domain: ClockDomain::Tai,
            epoch: 0,
        };
        assert_eq!(m.tai_ns(1_000, 500), 1_500);
        // Whatever the host, a Tai clock maps to host TAI.
        let clock = system_clock(gst::ClockType::Tai);
        let m = PipelineTaiMapping::for_clock(&clock, &tai);
        let mapped = m.tai_ns(clock.time().nseconds(), 0);
        assert!(mapped.abs_diff(tai.now_ns()) < 50_000_000);
        let m = PipelineTaiMapping::for_clock(&system_clock(gst::ClockType::Realtime), &tai);
        // Whole seconds: the kernel offset or the 37 s fallback.
        assert_eq!(m.offset_ns % 1_000_000_000, 0, "offset {}", m.offset_ns);
        assert!(m.offset_ns >= 37_000_000_000);
    }

    #[test]
    fn monotonic_mapping_tracks_host_tai() {
        gst::init().unwrap();
        let tai = HostTai::new();
        let clock = system_clock(gst::ClockType::Monotonic);
        let m = PipelineTaiMapping::for_clock(&clock, &tai);
        let mapped = m.tai_ns(clock.time().nseconds(), 0);
        let now = tai.now_ns();
        assert!(mapped.abs_diff(now) < 50_000_000, "{} vs {}", mapped, now);
    }

    /// Two blocks of one pipeline that ask for the mapping at different times get
    /// the same one; a restart (new base time) gets a new sample.
    #[test]
    fn mapping_is_shared_per_pipeline_run() {
        gst::init().unwrap();
        let tai = HostTai::new();
        let clock = system_clock(gst::ClockType::Monotonic);
        let flow = uuid::Uuid::new_v4();
        let a = shared_pipeline_mapping(flow, &clock, 1_000, &tai);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let b = shared_pipeline_mapping(flow, &clock, 1_000, &tai);
        assert_eq!(a, b);
        assert_eq!(a.tai_ns(1_000, 40_000_000), b.tai_ns(1_000, 40_000_000));

        std::thread::sleep(std::time::Duration::from_millis(5));
        let c = shared_pipeline_mapping(flow, &clock, 2_000, &tai);
        assert_ne!(a.offset_ns, c.offset_ns, "restart must resample");
        // Another clock instance (pipeline rebuilt) also resamples.
        let other = system_clock(gst::ClockType::Monotonic);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let d = shared_pipeline_mapping(flow, &other, 2_000, &tai);
        assert_ne!(c.offset_ns, d.offset_ns);
    }

    /// The pipeline clock stops while the Mac sleeps (16 h here) and host TAI goes
    /// on. The next call notices and re-samples the shared mapping with a new
    /// epoch; small drift does not.
    #[test]
    fn sleep_jump_reanchors_the_shared_mapping() {
        gst::init().unwrap();
        const H16: u64 = 16 * 3600 * 1_000_000_000;
        let tai = HostTai::new();
        let clock = system_clock(gst::ClockType::Monotonic);
        let flow = uuid::Uuid::new_v4();
        let a = shared_pipeline_mapping(flow, &clock, 1_000, &tai);
        assert_eq!(a.epoch, 0);
        let read = |extra: u64| {
            let (c, t) = sample_other(&clock, &tai);
            (c, t + extra)
        };
        // One second of drift: below the threshold, nothing changes.
        let b = shared_pipeline_mapping_with(flow, &clock, 1_000, &tai, || read(1_000_000_000));
        assert_eq!(a, b);
        // Wake after 16 h.
        let c = shared_pipeline_mapping_with(flow, &clock, 1_000, &tai, || read(H16));
        assert_eq!(c.epoch, 1);
        let moved = c.offset_ns - a.offset_ns;
        assert!(
            moved.abs_diff(H16 as i128) < 50_000_000,
            "offset moved {} ns",
            moved
        );
        // The re-sampled mapping holds: later calls (by any block) get the same.
        let d = shared_pipeline_mapping_with(flow, &clock, 1_000, &tai, || read(H16));
        assert_eq!(c, d);
    }

    /// Two branches of one pipeline stamp the same running times, one running
    /// half a second ahead. The one ahead notices a 16 h sleep and re-anchors;
    /// the other has not asked since. Both still stamp every running time alike:
    /// up to the latest one stamped before the re-anchor with the old offset,
    /// after it with the new one. With each branch on its own cached mapping,
    /// the one behind stamped 16 h off until its next check.
    #[test]
    fn branches_stamp_alike_across_a_reanchor() {
        gst::init().unwrap();
        const MS: u64 = 1_000_000;
        const H16: u64 = 16 * 3600 * 1_000_000_000;
        const BASE: u64 = 1_000;
        let tai = HostTai::new();
        let clock = system_clock(gst::ClockType::Monotonic);
        let flow = uuid::Uuid::new_v4();
        let (old, ahead) = shared_pipeline_stamps(flow, &clock, BASE, &tai);
        let (_, behind) = shared_pipeline_stamps(flow, &clock, BASE, &tai);
        let rt = |frame: u64| frame * 20 * MS;
        // Ahead has stamped through 10 s, behind through 9.5 s.
        let stamped: Vec<u64> = (0..=500).map(|f| ahead.tai_ns(BASE, rt(f))).collect();
        for (f, &s) in stamped.iter().enumerate().take(476) {
            assert_eq!(behind.tai_ns(BASE, rt(f as u64)), s);
        }
        assert_eq!(stamped[500], old.tai_ns(BASE, rt(500)));

        let read = || {
            let (c, t) = sample_other(&clock, &tai);
            (c, t + H16)
        };
        let new = shared_pipeline_mapping_with(flow, &clock, BASE, &tai, read);
        assert_eq!(new.epoch, 1);
        let next = ahead.tai_ns(BASE, rt(501));
        assert_eq!(next, new.tai_ns(BASE, rt(501)));
        assert_ne!(next, old.tai_ns(BASE, rt(501)));

        // Behind catches up: 9.52 s to 10 s as ahead stamped them, then the new
        // offset, without having asked the registry.
        for (f, &s) in stamped.iter().enumerate().skip(476) {
            assert_eq!(behind.tai_ns(BASE, rt(f as u64)), s, "frame {f}");
        }
        assert_eq!(behind.tai_ns(BASE, rt(501)), next);
    }

    /// A re-anchor before anything was stamped applies from the start.
    #[test]
    fn reanchor_before_any_stamp_applies_from_the_start() {
        let stamps = SharedStamps::new(5);
        stamps.reanchor(7);
        assert_eq!(stamps.tai_ns(0, 0), 7);
        assert_eq!(stamps.tai_ns(0, 100), 107);
        stamps.reanchor(9);
        // From 101 on; 100 and earlier keep 7.
        assert_eq!(stamps.tai_ns(0, 100), 107);
        assert_eq!(stamps.tai_ns(0, 101), 110);
    }

    /// Drift that adds up past the threshold, a little at each check, does not
    /// re-anchor: re-anchoring after about 28 h at 50 ppm would cut a 5 s hole
    /// in the store. A sudden step of the same size still does.
    #[test]
    fn slow_drift_does_not_reanchor() {
        gst::init().unwrap();
        const S: u64 = 1_000_000_000;
        const H28: u64 = 28 * 3600 * S;
        let tai = HostTai::new();
        let clock = system_clock(gst::ClockType::Monotonic);
        let flow = uuid::Uuid::new_v4();
        let a = shared_pipeline_mapping(flow, &clock, 1_000, &tai);
        // `elapsed` of host TAI goes by, and the pipeline clock falls `drift`
        // behind it.
        let read = |elapsed: u64, drift: u64| {
            let (c, t) = sample_other(&clock, &tai);
            (c + elapsed as i128 - drift as i128, t + elapsed)
        };
        // 28 h at 50 ppm (5.04 s) in 16 checks.
        let mut last = (0, 0);
        for step in 1..=16u64 {
            let elapsed = H28 * step / 16;
            let drift = elapsed / 20_000;
            last = (elapsed, drift);
            let m =
                shared_pipeline_mapping_with(flow, &clock, 1_000, &tai, || read(elapsed, drift));
            assert_eq!(m, a, "drift of {} ms re-anchored", drift / 1_000_000);
        }
        // A 6 s step on top of that is a jump.
        let b = shared_pipeline_mapping_with(flow, &clock, 1_000, &tai, || {
            read(last.0, last.1 + 6 * S)
        });
        assert_eq!(b.epoch, 1);
    }

    /// Steps that are each below the threshold (short sleeps) but add up past
    /// it, in far less time than drift needs for that, re-anchor.
    #[test]
    fn small_steps_that_add_up_reanchor() {
        gst::init().unwrap();
        const S: u64 = 1_000_000_000;
        let tai = HostTai::new();
        let clock = system_clock(gst::ClockType::Monotonic);
        let flow = uuid::Uuid::new_v4();
        let a = shared_pipeline_mapping(flow, &clock, 1_000, &tai);
        let read = |extra: u64| {
            let (c, t) = sample_other(&clock, &tai);
            (c, t + extra)
        };
        // Three 4 s sleeps: the first is within the threshold.
        let b = shared_pipeline_mapping_with(flow, &clock, 1_000, &tai, || read(4 * S));
        assert_eq!(b, a);
        // The second brings the total to 8 s: re-anchored.
        let c = shared_pipeline_mapping_with(flow, &clock, 1_000, &tai, || read(8 * S));
        assert_eq!(c.epoch, 1);
        // The third counts from the new anchor: 4 s again, within the threshold.
        let d = shared_pipeline_mapping_with(flow, &clock, 1_000, &tai, || read(12 * S));
        assert_eq!(d, c);
    }

    #[test]
    fn drift_allowance_grows_with_time_between_checks() {
        const S: u64 = 1_000_000_000;
        const H28: u64 = 28 * 3600 * S;
        assert!(!is_jump(4 * S as i128, 0, 0, 0));
        assert!(is_jump(6 * S as i128, 0, 0, 0));
        // 6 s over 28 h without a check is within 5 s + 100 ppm (10 s).
        assert!(!is_jump(6 * S as i128, 0, H28, H28));
        assert!(is_jump(-16 * S as i128, 0, H28, H28));
        // Steps within the allowance that add up past the total allowance.
        assert!(!is_jump(8 * S as i128, 4 * S as i128, 0, H28));
        assert!(is_jump(8 * S as i128, 4 * S as i128, 0, 0));
    }

    /// Idle entries (a flow that stopped or was deleted) are dropped when
    /// another pipeline run registers; an entry in use is kept.
    #[test]
    fn idle_registry_entries_are_pruned() {
        gst::init().unwrap();
        let tai = HostTai::new();
        let clock = system_clock(gst::ClockType::Monotonic);
        let (idle, busy) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        shared_pipeline_mapping(idle, &clock, 1_000, &tai);
        shared_pipeline_mapping(busy, &clock, 1_000, &tai);
        {
            let mut reg = REGISTRY.lock().unwrap();
            let past = Instant::now()
                .checked_sub(REGISTRY_IDLE + Duration::from_secs(1))
                .unwrap();
            reg.get_mut(&idle).unwrap().last_used = past;
        }
        shared_pipeline_mapping(uuid::Uuid::new_v4(), &clock, 1_000, &tai);
        let reg = REGISTRY.lock().unwrap();
        assert!(!reg.contains_key(&idle));
        assert!(reg.contains_key(&busy));
    }

    #[test]
    fn reference_domains() {
        let off = TAI_MINUS_UTC_NS;
        assert_eq!(reference_to_tai("timestamp/x-tai", 10, off), Some(10));
        assert_eq!(reference_to_tai("timestamp/x-ptp", 10, off), Some(10));
        assert_eq!(
            reference_to_tai("timestamp/x-unix", 10, off),
            Some(10 + off)
        );
        assert_eq!(
            reference_to_tai("timestamp/x-ntp", NTP_TO_UNIX_NS + 10, off),
            Some(10 + off)
        );
        assert_eq!(reference_to_tai("timestamp/x-ntp", 5, off), None);
        assert_eq!(reference_to_tai("timestamp/x-foo", 10, off), None);
    }

    #[test]
    fn reference_meta_on_buffer() {
        gst::init().unwrap();
        let mut buffer = gst::Buffer::new();
        {
            let b = buffer.get_mut().unwrap();
            gst::ReferenceTimestampMeta::add(
                b,
                &gst::Caps::new_empty_simple("timestamp/x-unix"),
                gst::ClockTime::from_nseconds(1_000),
                gst::ClockTime::NONE,
            );
        }
        assert_eq!(
            reference_tai_ns(buffer.as_ref(), TAI_MINUS_UTC_NS),
            Some(1_000 + TAI_MINUS_UTC_NS)
        );
        assert_eq!(reference_tai_ns(gst::Buffer::new().as_ref(), 0), None);
    }
}
