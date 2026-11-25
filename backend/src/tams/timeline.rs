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
pub struct TaiClock {
    tai: gst::Clock,
    realtime: gst::Clock,
}

impl Default for TaiClock {
    fn default() -> Self {
        Self::new()
    }
}

impl TaiClock {
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
    pub fn for_clock(clock: &gst::Clock, tai: &TaiClock) -> Self {
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
    tai: &TaiClock,
) -> PipelineTaiMapping {
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = reg.get(&flow_id) {
        let same_clock = entry.clock.upgrade().as_ref() == Some(clock);
        if entry.base_time_ns == base_time_ns && same_clock {
            return entry.mapping;
        }
    }
    let mapping = PipelineTaiMapping::for_clock(clock, tai);
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
    /// Host TAI "now" (no clock, or recovery from a backward running time).
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
}

/// Outcome of placing one fragment on the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentStart {
    pub start_ns: u64,
    /// Set when this fragment (re)anchored the timeline.
    pub anchored: Option<AnchorKind>,
    /// Set when running time went backward and the timeline was re-anchored.
    pub went_backward: bool,
}

impl FragmentTimeline {
    /// Place a fragment whose first buffer has `running_time_ns`.
    ///
    /// `anchor` is asked for `(TAI of this buffer, kind)` only when the timeline is
    /// not anchored yet. `now_tai_ns` is asked only when running time went
    /// backward: the timeline then re-anchors at host TAI now (but never before
    /// the previous fragment's start), so segments never overlap.
    pub fn place(
        &mut self,
        running_time_ns: u64,
        anchor: impl FnOnce() -> (u64, AnchorKind),
        now_tai_ns: impl FnOnce() -> u64,
    ) -> FragmentStart {
        let mut anchored = None;
        let mut went_backward = false;
        match self.offset_ns {
            None => {
                let (tai, kind) = anchor();
                self.offset_ns = Some(tai as i128 - running_time_ns as i128);
                anchored = Some(kind);
            }
            Some(_) if running_time_ns < self.last_running_time_ns => {
                let tai = now_tai_ns().max(self.last_start_ns + 1);
                self.offset_ns = Some(tai as i128 - running_time_ns as i128);
                anchored = Some(AnchorKind::Arrival);
                went_backward = true;
            }
            Some(_) => {}
        }
        let offset = self.offset_ns.unwrap_or(0);
        let start_ns = clamp_ns(running_time_ns as i128 + offset);
        self.last_running_time_ns = running_time_ns;
        self.last_start_ns = start_ns;
        FragmentStart {
            start_ns,
            anchored,
            went_backward,
        }
    }
}

impl FragmentTimeline {
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
        let seg = segment_ns as i128;
        let tai = running_time_ns as i128 + offset;
        let next_tai = (tai.div_euclid(seg) + 1) * seg;
        Some(clamp_ns(next_tai - offset))
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
        let tai = TaiClock::new();
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
        let tai = TaiClock::new();
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
        let tai = TaiClock::new();
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
        let s1 = cam1.place(0, anchor(0), || unreachable!());
        let s1b = cam1.place(1_000_000_000, anchor(1_000_000_000), || unreachable!());
        let s2b = cam2.place(1_000_000_000, anchor(1_000_000_000), || unreachable!());
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
        tl.place(100, || (10_350, AnchorKind::Pipeline), || unreachable!());
        // Next TAI multiple of 1_000 is 11_000 = running time 750.
        assert_eq!(tl.next_split_running_time(100, 1_000), Some(750));
        // A fragment that starts on the grid ends one segment later.
        tl.place(750, || unreachable!(), || unreachable!());
        assert_eq!(tl.next_split_running_time(750, 1_000), Some(1_750));
        // A late keyframe (TAI 12_040) ends at the following grid point.
        tl.place(1_790, || unreachable!(), || unreachable!());
        assert_eq!(tl.next_split_running_time(1_790, 1_000), Some(2_750));
        assert_eq!(tl.next_split_running_time(1_790, 0), None);
    }

    #[test]
    fn fragments_are_contiguous_and_reanchor_on_backward_running_time() {
        let mut tl = FragmentTimeline::default();
        let a = tl.place(500, || (10_000, AnchorKind::Reference), || unreachable!());
        assert_eq!(a.start_ns, 10_000);
        assert_eq!(a.anchored, Some(AnchorKind::Reference));
        let b = tl.place(1_500, || unreachable!(), || unreachable!());
        assert_eq!(b.start_ns, 11_000);
        // Running time steps back: re-anchor at "now", never before the last start.
        let c = tl.place(100, || unreachable!(), || 5);
        assert!(c.went_backward);
        assert_eq!(c.anchored, Some(AnchorKind::Arrival));
        assert_eq!(c.start_ns, 11_001);
        let d = tl.place(1_100, || unreachable!(), || unreachable!());
        assert_eq!(d.start_ns, 12_001);
        let e = tl.place(200, || unreachable!(), || 50_000);
        assert_eq!(e.start_ns, 50_000);
    }
}
