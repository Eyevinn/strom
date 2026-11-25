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
//! The `Other` offset is not re-sampled during a run (review L11). A monotonic
//! or NTP clock drifts against disciplined TAI by tens of ppm, so over hours the
//! store's TAI moves away from wall-clock capture time (about 1-2 s over 10 h at
//! 50 ppm). Angles of one pipeline share the offset and stay aligned with each
//! other; UTC addressing and joining stores of other pipelines suffer. Re-sampling
//! per block would break that alignment, since blocks would see different
//! samples at the same running time. Long recordings that need wall-clock
//! accuracy should run the pipeline on a TAI, PTP or Realtime clock, which map
//! by a fixed offset, or carry reference timestamps.
//!
//! A buffer that carries a capture time as a `GstReferenceTimestampMeta`
//! (`timestamp/x-tai`, `x-ptp`, `x-unix` or `x-ntp`) takes precedence: the block
//! anchors on it instead of the pipeline clock.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use strom_types::FlowId;
use tracing::info;

/// TAI - UTC (37 s, fixed since the 2026 CGPM decision). Used as the fallback
/// when the kernel has no TAI offset set (macOS, or Linux without chrony
/// `leapsectz`).
pub use strom_types::tai::TAI_MINUS_UTC_NS;

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_TO_UNIX_NS: u64 = 2_208_988_800 * 1_000_000_000;

/// Below this, CLOCK_TAI and CLOCK_REALTIME are taken to be the same clock (the
/// kernel's TAI offset is unset).
const UNSET_OFFSET_THRESHOLD_NS: u64 = 1_000_000_000;

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
                let before = clock.time().nseconds() as i128;
                let tai_now = tai.now_ns() as i128;
                let after = clock.time().nseconds() as i128;
                tai_now - (before + after) / 2
            }
        };
        Self { offset_ns, domain }
    }

    /// TAI of a buffer with this running time in a pipeline with this base time.
    pub fn tai_ns(&self, base_time_ns: u64, running_time_ns: u64) -> u64 {
        clamp_ns(base_time_ns as i128 + running_time_ns as i128 + self.offset_ns)
    }
}

struct RegistryEntry {
    base_time_ns: u64,
    clock: gst::glib::WeakRef<gst::Clock>,
    mapping: PipelineTaiMapping,
}

static REGISTRY: LazyLock<Mutex<HashMap<FlowId, RegistryEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The TAI mapping shared by all TAMS Outputs of one pipeline run.
///
/// The first block to ask samples it; later calls with the same flow, clock and
/// base time get the same mapping. A restarted pipeline has a new base time (or
/// clock), so it gets a fresh one.
pub fn shared_pipeline_mapping(
    flow_id: FlowId,
    clock: &gst::Clock,
    base_time_ns: u64,
    tai: &HostTai,
) -> PipelineTaiMapping {
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = reg.get(&flow_id) {
        let same_clock = entry.clock.upgrade().as_ref() == Some(clock);
        if entry.base_time_ns == base_time_ns && same_clock {
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
    reg.insert(
        flow_id,
        RegistryEntry {
            base_time_ns,
            clock: clock.downgrade(),
            mapping,
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
    /// After a backward jump: how far the start was moved forward so it does
    /// not overlap the previous fragment.
    pub clamped_by_ns: Option<u64>,
    /// Index of this fragment since the timeline was last (re)anchored (0 for
    /// the anchoring fragment).
    pub since_anchor: u32,
}

/// Fragments after the (re)anchoring one whose split could not be requested
/// in time: the first ends at a split scheduled only when it opened, and the
/// keyframe for the second was requested at the same moment. From this index
/// on, a fragment's start was requested one fragment ahead and should be on
/// the grid.
pub const FIRST_ON_GRID_FRAGMENT: u32 = 2;

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
    /// one begins (review M2). Blocks on one grid clamp to the same point.
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
            (None, Some(_)) => {}
        }
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

    /// Running time steps back inside one pipeline run (review M2). The shared
    /// mapping still holds, so the block re-anchors on it, not on host "now":
    /// two blocks that see the jump at different moments stay aligned, and the
    /// fragment before the jump keeps its planned timerange.
    #[test]
    fn backward_running_time_reanchors_on_the_shared_mapping() {
        const SEG: u64 = 1_000_000_000;
        let mapping = PipelineTaiMapping {
            offset_ns: 1_800_000_000_000_000_000,
            domain: ClockDomain::Other,
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
