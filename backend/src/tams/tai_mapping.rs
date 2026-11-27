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
//! re-anchors on the same offset and the angles stay aligned.
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
use std::sync::{LazyLock, Mutex};
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

/// [`shared_pipeline_mapping`] with the jump check reading `(pipeline clock time,
/// host TAI)` from `sample`, so tests can simulate a sleep.
fn shared_pipeline_mapping_with(
    flow_id: FlowId,
    clock: &gst::Clock,
    base_time_ns: u64,
    tai: &HostTai,
    sample: impl Fn() -> (i128, u64),
) -> PipelineTaiMapping {
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
            return entry.mapping;
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
        },
    );
    mapping
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

/// Why a block (re)anchored its timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    /// Capture time from a `GstReferenceTimestampMeta`.
    Reference,
    /// The shared pipeline mapping.
    Pipeline,
    /// Host TAI "now": the block has no pipeline clock yet.
    Arrival,
}

/// One block's timeline: `TAI = running_time + offset_ns`.
///
/// Anchored on the first fragment, then advanced by running time, so fragments are
/// contiguous: each fragment ends exactly where the next one starts.
#[derive(Debug, Default)]
pub struct FragmentTimeline {
    offset_ns: Option<i128>,
    last_running_time_ns: u64,
    last_start_ns: u64,
    /// TAI of the grid point the last fragment was planned to end on.
    planned_end_ns: Option<u64>,
    /// Fragments placed since the timeline was last (re)anchored.
    since_anchor: u32,
    /// Epoch of the shared pipeline mapping last seen (see [`Self::note_pipeline_epoch`]).
    pipeline_epoch: Option<u32>,
    /// The shared mapping was re-sampled after a jump: re-anchor on the next
    /// fragment that has no reference timestamp.
    reanchor_pending: bool,
}

/// Outcome of placing one fragment on the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentStart {
    pub start_ns: u64,
    /// Where the previous fragment ends: this fragment's start, except after a
    /// backward jump, where it is the grid point the previous fragment was
    /// planned to end on.
    pub prev_end_ns: u64,
    /// Set when this fragment (re)anchored the timeline.
    pub anchored: Option<AnchorKind>,
    /// Set when running time went backward and the timeline was re-anchored.
    pub went_backward: bool,
    /// Set when the timeline was re-anchored because the shared pipeline
    /// mapping jumped (a sleep or clock step), see
    /// [`FragmentTimeline::note_pipeline_epoch`].
    pub jumped: bool,
    /// After a backward jump: how far the start was moved forward so it does
    /// not overlap the previous fragment.
    pub clamped_by_ns: Option<u64>,
    /// Index of this fragment since the timeline was last (re)anchored (0 for
    /// the anchoring fragment).
    pub since_anchor: u32,
}

/// How far ahead of a fragment's next split TAMS Output asks upstream for
/// keyframes on the grid. It must exceed the upstream encoder's latency (x264
/// with lookahead holds most of a second), or the request comes after the
/// encoder has passed that point and forces a keyframe off the grid. With 1 s
/// segments this is one grid point ahead, as before sub-second segments.
pub const KEYFRAME_LEAD_NS: u64 = 1_000_000_000;

/// Index (since the (re)anchoring fragment, which is 0) of the first fragment
/// whose start keyframe was requested at least [`KEYFRAME_LEAD_NS`] ahead, so
/// that it should be on the grid. Fragment 1 starts at a split scheduled only
/// when fragment 0 opened. The keyframes for the splits after it were
/// requested at that same moment, fragment `k`'s only `k - 1` segments plus
/// part of one ahead: less than the encoder's latency until `k - 1` segments
/// reach the lead. With 1 s segments that is fragment 2. With 500 ms segments
/// it is fragment 3: fragment 2's keyframe is asked for only 0.5 to 1 s ahead,
/// x264 is often past that point already and forces one at once, off the grid.
pub fn first_on_grid_fragment(segment_ns: u64) -> u32 {
    let lead = KEYFRAME_LEAD_NS.div_ceil(segment_ns.max(1)).max(1);
    u32::try_from(lead).unwrap_or(u32::MAX).saturating_add(1)
}

/// How far past its grid point a fragment starting at `start_ns` lies, when that
/// is more than `tolerance_ns` (one frame: splits land on the first keyframe at
/// or after the grid point). `None` when on the grid or with no grid.
pub fn grid_miss_ns(start_ns: u64, segment_ns: u64, tolerance_ns: u64) -> Option<u64> {
    if segment_ns == 0 {
        return None;
    }
    let past = start_ns % segment_ns;
    (past > tolerance_ns).then_some(past)
}

/// First multiple of `segment_ns` strictly after `tai_ns`.
fn next_grid_point(tai_ns: i128, segment_ns: u64) -> i128 {
    let seg = segment_ns as i128;
    (tai_ns.div_euclid(seg) + 1) * seg
}

impl FragmentTimeline {
    /// Tell the timeline the current epoch of the shared pipeline mapping. When
    /// it changed since the last call (the mapping was re-sampled after a sleep or
    /// clock step), the next fragment without a reference timestamp re-anchors on
    /// the mapping instead of advancing by running time. Every block of the
    /// pipeline sees the same new offset, so they stay aligned.
    pub fn note_pipeline_epoch(&mut self, epoch: u32) {
        if self.offset_ns.is_some() && self.pipeline_epoch.is_some_and(|e| e != epoch) {
            self.reanchor_pending = true;
        }
        self.pipeline_epoch = Some(epoch);
    }

    /// Place a fragment whose first buffer has `running_time_ns`, on a grid of
    /// `segment_ns` (0 = no grid).
    ///
    /// `reference_ns` is the buffer's capture time as TAI (its reference
    /// timestamp), when it has one. Then the fragment starts at that time,
    /// always: the frame's own absolute time is the truth, and the TAI grid only
    /// decides where fragments split. The timeline is re-anchored on it for the
    /// next split, so a source that restarts (with or without a gap, with
    /// running time going back or not) never inherits the previous run's phase.
    /// The previous fragment ends at this start when the two are contiguous
    /// (within `contiguous_ns`, e.g. one frame, of where running time puts it),
    /// and otherwise where running time says its media ended, so a gap stays a
    /// gap. It is cut short only when the new capture time lies before its
    /// planned end: the timeline never overlaps.
    ///
    /// Without a reference timestamp, `anchor` gives the TAI of this buffer and
    /// how it was found: the mapping shared by every block of the pipeline run,
    /// else host TAI now. It is asked when the timeline is not anchored yet, and
    /// again when running time went backward. Within one pipeline run the shared
    /// mapping stays valid across such a jump, so every block that sees it
    /// re-anchors on the same TAI and the angles stay aligned (host "now" would
    /// differ per block). After such a backward jump the start is never earlier
    /// than the grid point the previous fragment was planned to end on, so that
    /// fragment keeps its full timerange rather than being cut to where the new
    /// one begins. Blocks on one grid clamp to the same point.
    pub fn place(
        &mut self,
        running_time_ns: u64,
        segment_ns: u64,
        reference_ns: Option<u64>,
        contiguous_ns: u64,
        anchor: impl FnOnce() -> (u64, AnchorKind),
    ) -> FragmentStart {
        let mut anchored = None;
        let mut went_backward = false;
        let mut clamped_by_ns = None;
        let mut prev_end_ns = None;
        let mut jumped = false;
        let backward = self.offset_ns.is_some() && running_time_ns < self.last_running_time_ns;
        match (reference_ns, self.offset_ns) {
            (Some(tai), None) => {
                self.offset_ns = Some(tai as i128 - running_time_ns as i128);
                anchored = Some(AnchorKind::Reference);
            }
            (Some(tai), Some(_)) if backward => {
                let planned = self
                    .planned_end_ns
                    .unwrap_or(self.last_start_ns.saturating_add(1));
                self.offset_ns = Some(tai as i128 - running_time_ns as i128);
                prev_end_ns = Some(planned.min(tai));
                anchored = Some(AnchorKind::Reference);
                went_backward = true;
            }
            (Some(tai), Some(offset)) => {
                // Where running time puts this buffer, i.e. where the previous
                // fragment's media ends.
                let expected = clamp_ns(running_time_ns as i128 + offset);
                if tai.abs_diff(expected) > contiguous_ns {
                    // Capture time jumped against running time: a new run of
                    // the source behind a running pipeline.
                    self.offset_ns = Some(tai as i128 - running_time_ns as i128);
                    anchored = Some(AnchorKind::Reference);
                    prev_end_ns = Some(expected.min(tai));
                } else {
                    // Follow the capture time exactly, also through drift or
                    // jitter against the pipeline clock.
                    self.offset_ns = Some(tai as i128 - running_time_ns as i128);
                }
            }
            (None, None) => {
                let (tai, kind) = anchor();
                self.offset_ns = Some(tai as i128 - running_time_ns as i128);
                anchored = Some(kind);
            }
            (None, Some(_)) if backward => {
                let (tai, kind) = anchor();
                let floor = self
                    .planned_end_ns
                    .unwrap_or(self.last_start_ns.saturating_add(1));
                let start = tai.max(floor);
                if start > tai {
                    clamped_by_ns = Some(start - tai);
                }
                self.offset_ns = Some(start as i128 - running_time_ns as i128);
                prev_end_ns = Some(floor.min(start));
                anchored = Some(kind);
                went_backward = true;
            }
            (None, Some(offset)) if self.reanchor_pending => {
                // The mapping jumped. The previous fragment's media ends where the
                // old offset puts this buffer. A forward jump (sleep) leaves a gap;
                // a backward one is not followed past that point, so the store
                // never gets overlapping segments.
                let (tai, kind) = anchor();
                let expected = clamp_ns(running_time_ns as i128 + offset);
                let start = tai.max(expected);
                if start > tai {
                    clamped_by_ns = Some(start - tai);
                }
                self.offset_ns = Some(start as i128 - running_time_ns as i128);
                prev_end_ns = Some(expected);
                anchored = Some(kind);
                jumped = true;
            }
            (None, Some(_)) => {}
        }
        self.reanchor_pending = false;
        let offset = self.offset_ns.unwrap_or(0);
        let start_ns = clamp_ns(running_time_ns as i128 + offset);
        self.last_running_time_ns = running_time_ns;
        self.last_start_ns = start_ns;
        self.planned_end_ns =
            (segment_ns > 0).then(|| clamp_ns(next_grid_point(start_ns as i128, segment_ns)));
        self.since_anchor = if anchored.is_some() {
            0
        } else {
            self.since_anchor.saturating_add(1)
        };
        FragmentStart {
            start_ns,
            prev_end_ns: prev_end_ns.unwrap_or(start_ns),
            anchored,
            went_backward,
            jumped,
            clamped_by_ns,
            since_anchor: self.since_anchor,
        }
    }

    /// Running time of the next split point after a fragment that starts at
    /// `running_time_ns`: the next multiple of `segment_ns` on the TAI timeline.
    ///
    /// Splitting on a TAI grid gives every flow written from the same frames the
    /// same segment boundaries, and puts boundaries of flows on a shared clock at
    /// the same TAI. `None` until the timeline is anchored or with a zero length.
    pub fn next_split_running_time(&self, running_time_ns: u64, segment_ns: u64) -> Option<u64> {
        let offset = self.offset_ns?;
        if segment_ns == 0 {
            return None;
        }
        let tai = running_time_ns as i128 + offset;
        Some(clamp_ns(next_grid_point(tai, segment_ns) - offset))
    }
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

    /// After the shared mapping jumps, every block re-anchors on the new offset,
    /// whenever it notices, so the angles stay aligned. The fragment open across
    /// the sleep ends where its media ends, leaving a gap.
    #[test]
    fn blocks_reanchor_together_after_a_sleep() {
        const SEG: u64 = 1_000_000_000;
        const H16: i128 = 16 * 3600 * 1_000_000_000;
        let before = PipelineTaiMapping {
            offset_ns: 1_800_000_000_000_000_000,
            domain: ClockDomain::Other,
            epoch: 0,
        };
        let after = PipelineTaiMapping {
            offset_ns: before.offset_ns + H16,
            epoch: 1,
            ..before
        };
        let anchor = |m: PipelineTaiMapping, rt| move || (m.tai_ns(0, rt), AnchorKind::Pipeline);
        let mut cam1 = FragmentTimeline::default();
        let mut cam2 = FragmentTimeline::default();
        for tl in [&mut cam1, &mut cam2] {
            for rt in [10 * SEG, 11 * SEG] {
                tl.note_pipeline_epoch(before.epoch);
                tl.place(rt, SEG, None, 0, anchor(before, rt));
            }
        }
        // cam1 sees the new epoch at its next fragment.
        cam1.note_pipeline_epoch(after.epoch);
        let j1 = cam1.place(12 * SEG, SEG, None, 0, anchor(after, 12 * SEG));
        assert!(j1.jumped && !j1.went_backward);
        assert_eq!(j1.anchored, Some(AnchorKind::Pipeline));
        assert_eq!(j1.start_ns, after.tai_ns(0, 12 * SEG));
        assert_eq!(
            j1.prev_end_ns,
            before.tai_ns(0, 12 * SEG),
            "gap, no stretch"
        );
        // cam2 places one more fragment before it asks again: still the old
        // timeline. Then it sees the new epoch and lands where cam1 is.
        let o2 = cam2.place(12 * SEG, SEG, None, 0, || unreachable!());
        assert_eq!(o2.start_ns, before.tai_ns(0, 12 * SEG));
        cam2.note_pipeline_epoch(after.epoch);
        let j2 = cam2.place(13 * SEG, SEG, None, 0, anchor(after, 13 * SEG));
        assert!(j2.jumped);
        let n1 = cam1.place(13 * SEG, SEG, None, 0, || unreachable!());
        assert_eq!(n1.start_ns, j2.start_ns, "angles aligned after the jump");
        // Same epoch again: no further re-anchor.
        cam1.note_pipeline_epoch(after.epoch);
        let m1 = cam1.place(14 * SEG, SEG, None, 0, || unreachable!());
        assert!(!m1.jumped && m1.anchored.is_none());
    }

    /// A backward clock step is not followed past the previous fragment's end:
    /// the store never gets overlapping segments.
    #[test]
    fn backward_mapping_jump_never_overlaps() {
        const SEG: u64 = 1_000_000_000;
        let before = PipelineTaiMapping {
            offset_ns: 1_800_000_000_000_000_000,
            domain: ClockDomain::Other,
            epoch: 0,
        };
        let after = PipelineTaiMapping {
            offset_ns: before.offset_ns - 10 * SEG as i128,
            epoch: 1,
            ..before
        };
        let mut tl = FragmentTimeline::default();
        tl.note_pipeline_epoch(0);
        tl.place(10 * SEG, SEG, None, 0, || {
            (before.tai_ns(0, 10 * SEG), AnchorKind::Pipeline)
        });
        tl.note_pipeline_epoch(1);
        let j = tl.place(11 * SEG, SEG, None, 0, || {
            (after.tai_ns(0, 11 * SEG), AnchorKind::Pipeline)
        });
        assert!(j.jumped);
        assert_eq!(j.start_ns, before.tai_ns(0, 11 * SEG));
        assert_eq!(j.prev_end_ns, j.start_ns);
        assert_eq!(j.clamped_by_ns, Some(10 * SEG));
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

    /// Blocks anchored on the shared mapping put equal running times at equal TAI,
    /// whenever their first fragment opens.
    #[test]
    fn blocks_on_one_mapping_line_up() {
        let mapping = PipelineTaiMapping {
            offset_ns: 5_000,
            domain: ClockDomain::Other,
            epoch: 0,
        };
        let base = 1_000_000;
        let mut cam1 = FragmentTimeline::default();
        let mut cam2 = FragmentTimeline::default();
        let anchor = |rt| move || (mapping.tai_ns(base, rt), AnchorKind::Pipeline);
        // cam2's first fragment opens one segment later than cam1's.
        let s1 = cam1.place(0, 0, None, 0, anchor(0));
        let s1b = cam1.place(1_000_000_000, 0, None, 0, anchor(1_000_000_000));
        let s2b = cam2.place(1_000_000_000, 0, None, 0, anchor(1_000_000_000));
        assert_eq!(s1.anchored, Some(AnchorKind::Pipeline));
        assert_eq!(s1b.anchored, None);
        assert_eq!(s1b.start_ns, s2b.start_ns);
        assert_eq!(s1b.start_ns - s1.start_ns, 1_000_000_000);
    }

    #[test]
    fn splits_fall_on_the_tai_grid() {
        let mut tl = FragmentTimeline::default();
        assert_eq!(tl.next_split_running_time(0, 1_000), None);
        // Running time 100 is TAI 10_350.
        tl.place(100, 1_000, None, 0, || (10_350, AnchorKind::Pipeline));
        // Next TAI multiple of 1_000 is 11_000 = running time 750.
        assert_eq!(tl.next_split_running_time(100, 1_000), Some(750));
        // A fragment that starts on the grid ends one segment later.
        tl.place(750, 1_000, None, 0, || unreachable!());
        assert_eq!(tl.next_split_running_time(750, 1_000), Some(1_750));
        // A late keyframe (TAI 12_040) ends at the following grid point.
        tl.place(1_790, 1_000, None, 0, || unreachable!());
        assert_eq!(tl.next_split_running_time(1_790, 1_000), Some(2_750));
        assert_eq!(tl.next_split_running_time(1_790, 0), None);
    }

    #[test]
    fn fragments_are_contiguous() {
        let mut tl = FragmentTimeline::default();
        let a = tl.place(500, 1_000, Some(10_000), 0, || unreachable!());
        assert_eq!(a.start_ns, 10_000);
        assert_eq!(a.anchored, Some(AnchorKind::Reference));
        let b = tl.place(1_500, 1_000, Some(11_000), 0, || unreachable!());
        assert_eq!((b.start_ns, b.prev_end_ns), (11_000, 11_000));
        assert!(!b.went_backward && b.anchored.is_none());
    }

    /// Running time steps back inside one pipeline run. The shared
    /// mapping still holds, so the block re-anchors on it, not on host "now":
    /// two blocks that see the jump at different moments stay aligned, and the
    /// fragment before the jump keeps its planned timerange.
    #[test]
    fn backward_running_time_reanchors_on_the_shared_mapping() {
        const SEG: u64 = 1_000_000_000;
        let mapping = PipelineTaiMapping {
            offset_ns: 1_800_000_000_000_000_000,
            domain: ClockDomain::Other,
            epoch: 0,
        };
        let base = 0;
        let anchor = |rt| move || (mapping.tai_ns(base, rt), AnchorKind::Pipeline);
        let mut cam1 = FragmentTimeline::default();
        let mut cam2 = FragmentTimeline::default();
        for tl in [&mut cam1, &mut cam2] {
            tl.place(10 * SEG, SEG, None, 0, anchor(10 * SEG));
            tl.place(11 * SEG, SEG, None, 0, anchor(11 * SEG));
        }
        // The source restarts: running time is back at 2 s. The mapping places
        // it before the open fragment, so the start is clamped to the grid
        // point that fragment was planned to end on.
        let c1 = cam1.place(2 * SEG, SEG, None, 0, anchor(2 * SEG));
        let c2 = cam2.place(2 * SEG, SEG, None, 0, anchor(2 * SEG));
        assert!(c1.went_backward);
        assert_eq!(c1.anchored, Some(AnchorKind::Pipeline));
        assert_eq!(c1, c2, "both blocks re-anchor on the same TAI");
        let planned_end = mapping.tai_ns(base, 12 * SEG);
        assert_eq!(c1.start_ns, planned_end);
        assert_eq!(
            c1.prev_end_ns, planned_end,
            "previous fragment not cut short"
        );
        assert_eq!(c1.clamped_by_ns, Some(10 * SEG));
        // Later fragments follow the new offset on both blocks.
        let d1 = cam1.place(3 * SEG, SEG, None, 0, || unreachable!());
        let d2 = cam2.place(3 * SEG, SEG, None, 0, || unreachable!());
        assert_eq!(d1.start_ns, planned_end + SEG);
        assert_eq!(d1, d2);
    }

    #[test]
    fn grid_misses() {
        assert_eq!(grid_miss_ns(5_000, 1_000, 20), None);
        assert_eq!(grid_miss_ns(5_020, 1_000, 20), None);
        assert_eq!(grid_miss_ns(5_021, 1_000, 20), Some(21));
        assert_eq!(grid_miss_ns(5_480, 1_000, 20), Some(480));
        assert_eq!(grid_miss_ns(5_480, 0, 20), None);
    }

    /// The first on-grid fragment is the first whose keyframe was requested
    /// at least `KEYFRAME_LEAD_NS` ahead: fragment 2 with 1 s segments (as
    /// before sub-second segments), later with shorter ones.
    #[test]
    fn first_on_grid_fragment_follows_the_lead() {
        const MS: u64 = 1_000_000;
        assert_eq!(first_on_grid_fragment(2_000 * MS), 2);
        assert_eq!(first_on_grid_fragment(1_000 * MS), 2);
        assert_eq!(first_on_grid_fragment(600 * MS), 3);
        assert_eq!(first_on_grid_fragment(500 * MS), 3);
        assert_eq!(first_on_grid_fragment(200 * MS), 6);
    }

    #[test]
    fn fragments_count_from_the_anchor() {
        let mut tl = FragmentTimeline::default();
        assert_eq!(
            tl.place(0, 10, None, 0, || (0, AnchorKind::Pipeline))
                .since_anchor,
            0
        );
        assert_eq!(tl.place(5, 10, None, 0, || unreachable!()).since_anchor, 1);
        assert_eq!(tl.place(10, 10, None, 0, || unreachable!()).since_anchor, 2);
        // A backward jump re-anchors and starts the count again.
        assert_eq!(
            tl.place(1, 10, None, 0, || (0, AnchorKind::Pipeline))
                .since_anchor,
            0
        );
    }

    /// A backward jump whose anchor is already past the planned end (the source
    /// was away for a while) is not clamped: the store gets a gap, and the
    /// previous fragment ends at its planned grid point, not at the new start.
    #[test]
    fn backward_running_time_after_a_gap_leaves_a_gap() {
        let mut tl = FragmentTimeline::default();
        tl.place(5_000, 1_000, Some(100_000), 0, || unreachable!());
        let b = tl.place(100, 1_000, Some(250_000), 0, || unreachable!());
        assert!(b.went_backward);
        assert_eq!(b.start_ns, 250_000);
        assert_eq!(b.prev_end_ns, 101_000);
        assert_eq!(b.clamped_by_ns, None);
        // Without a grid, the floor is one ns after the previous start.
        let mut tl = FragmentTimeline::default();
        tl.place(5_000, 0, None, 0, || (100_000, AnchorKind::Arrival));
        let b = tl.place(100, 0, None, 0, || (5, AnchorKind::Arrival));
        assert_eq!((b.start_ns, b.prev_end_ns), (100_001, 100_001));
    }

    /// A source restarted behind the block, within less than one segment: the
    /// capture time lies before the grid point the open fragment was planned
    /// to end on. The new fragment still starts at its capture time (new
    /// phase); the previous one is cut there instead.
    #[test]
    fn reference_restart_is_never_clamped_to_the_old_grid() {
        const SEG: u64 = 1_000_000_000;
        let mut tl = FragmentTimeline::default();
        // First run: phase 7.9 ms.
        tl.place(
            10 * SEG,
            SEG,
            Some(100 * SEG + 7_900_000),
            0,
            || unreachable!(),
        );
        let b = tl.place(
            11 * SEG,
            SEG,
            Some(101 * SEG + 7_900_000),
            0,
            || unreachable!(),
        );
        assert_eq!(b.since_anchor, 1);
        // Restart: running time back at 0, captured at 101.5032 s (phase 3.2 ms).
        let restart = 101 * SEG + 503_200_000;
        let c = tl.place(0, SEG, Some(restart), 20_000_000, || unreachable!());
        assert!(c.went_backward);
        assert_eq!(c.anchored, Some(AnchorKind::Reference));
        assert_eq!(c.start_ns, restart, "follows the capture time");
        assert_eq!(c.clamped_by_ns, None);
        assert_eq!(c.prev_end_ns, restart, "previous fragment cut, no overlap");
        // The next split is on the TAI grid, frames keep the new phase.
        assert_eq!(tl.next_split_running_time(0, SEG), Some(SEG - 503_200_000));
        let d = tl.place(
            SEG - 500_000_000,
            SEG,
            Some(102 * SEG + 3_200_000),
            20_000_000,
            || unreachable!(),
        );
        assert_eq!(d.start_ns, 102 * SEG + 3_200_000);
        assert_eq!(d.prev_end_ns, d.start_ns);
    }

    /// Capture time jumps forward while running time goes on (the source
    /// restarted behind a pipeline that kept running): the fragment starts at
    /// the new capture time, and the previous one ends where its media ends.
    #[test]
    fn reference_jump_without_running_time_jump_reanchors() {
        const SEG: u64 = 1_000_000_000;
        const FRAME: u64 = 20_000_000;
        let mut tl = FragmentTimeline::default();
        tl.place(0, SEG, Some(50 * SEG + 7_900_000), FRAME, || unreachable!());
        let b = tl.place(
            SEG,
            SEG,
            Some(51 * SEG + 7_900_000),
            FRAME,
            || unreachable!(),
        );
        assert!(b.anchored.is_none());
        // 20 s later, new phase 8.8 ms, running time only 1 s on.
        let c = tl.place(
            2 * SEG,
            SEG,
            Some(72 * SEG + 8_800_000),
            FRAME,
            || unreachable!(),
        );
        assert_eq!(c.anchored, Some(AnchorKind::Reference));
        assert!(!c.went_backward);
        assert_eq!(c.start_ns, 72 * SEG + 8_800_000);
        assert_eq!(c.prev_end_ns, 52 * SEG + 7_900_000, "gap stays a gap");
        assert_eq!(c.since_anchor, 0);
        // Jitter within one frame keeps the fragments contiguous and follows
        // the capture time.
        let d = tl.place(
            3 * SEG,
            SEG,
            Some(73 * SEG + 8_800_100),
            FRAME,
            || unreachable!(),
        );
        assert!(d.anchored.is_none());
        assert_eq!(
            (d.start_ns, d.prev_end_ns),
            (73 * SEG + 8_800_100, 73 * SEG + 8_800_100)
        );
    }
}
