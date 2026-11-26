//! Frame rate of a video stream estimated from its timestamps.
//!
//! Some H.264 streams carry no timing in their SPS (VideoToolbox writes no VUI
//! `timing_info`), so after a transport hop the caps say `framerate=0/1`. The
//! PTS still advance one frame period per picture, so the rate can be read
//! back from them.

/// Standard rates, as `(numerator, denominator)`, an estimate is snapped to.
const STANDARD_RATES: [(u32, u32); 12] = [
    (24000, 1001),
    (24, 1),
    (25, 1),
    (30000, 1001),
    (30, 1),
    (48, 1),
    (50, 1),
    (60000, 1001),
    (60, 1),
    (100, 1),
    (120000, 1001),
    (120, 1),
];

/// Relative tolerance when snapping a measured period to a rate.
const SNAP_TOLERANCE: f64 = 0.003;

/// Frame periods outside this range (ns) are not a video frame rate.
const MIN_PERIOD_NS: u64 = 4_000_000; // 250 fps
const MAX_PERIOD_NS: u64 = 1_000_000_000; // 1 fps

/// Fewest distinct timestamps an estimate is made from.
pub const MIN_SAMPLES: usize = 8;

/// The frame rate the timestamps `pts` (ns, any order, one per picture)
/// advance at: the median distance between neighbouring sorted timestamps,
/// snapped to a standard rate, or to a whole rate. `None` when there are too
/// few timestamps, or the median distance matches no rate within 0.3 %.
///
/// Sorting handles reordered (B-frame) streams; the median handles a dropped
/// frame or two.
pub fn estimate_frame_rate(pts: impl IntoIterator<Item = u64>) -> Option<(u32, u32)> {
    let mut pts: Vec<u64> = pts.into_iter().collect();
    pts.sort_unstable();
    pts.dedup();
    if pts.len() < MIN_SAMPLES {
        return None;
    }
    let mut deltas: Vec<u64> = pts.windows(2).map(|w| w[1] - w[0]).collect();
    deltas.sort_unstable();
    let period = deltas[deltas.len() / 2];
    if !(MIN_PERIOD_NS..=MAX_PERIOD_NS).contains(&period) {
        return None;
    }
    let fps = 1e9 / period as f64;
    let close = |n: u32, d: u32| {
        let rate = n as f64 / d as f64;
        ((fps - rate) / rate).abs() <= SNAP_TOLERANCE
    };
    // TS timestamps are 90 kHz, so 29.97 and 30 differ by 0.1 %: pick the
    // closest standard rate, not the first within tolerance.
    let best = STANDARD_RATES
        .iter()
        .copied()
        .filter(|&(n, d)| close(n, d))
        .min_by(|a, b| {
            let err = |(n, d): (u32, u32)| (fps - n as f64 / d as f64).abs();
            err(*a).total_cmp(&err(*b))
        });
    best.or_else(|| {
        let whole = fps.round() as u32;
        (whole > 0 && close(whole, 1)).then_some((whole, 1))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(period_ns: f64, n: usize) -> Vec<u64> {
        (0..n)
            .map(|i| (1_000_000_000.0 + i as f64 * period_ns) as u64)
            .collect()
    }

    /// 90 kHz rounding, as after an MPEG-TS hop.
    fn ticks90k(pts: Vec<u64>) -> Vec<u64> {
        pts.into_iter()
            .map(|p| (p * 9 / 100_000) * 100_000 / 9)
            .collect()
    }

    #[test]
    fn standard_rates_from_timestamps() {
        assert_eq!(estimate_frame_rate(run(1e9 / 30.0, 30)), Some((30, 1)));
        assert_eq!(estimate_frame_rate(run(20e6, 30)), Some((50, 1)));
        assert_eq!(
            estimate_frame_rate(ticks90k(run(1001e9 / 30_000.0, 30))),
            Some((30000, 1001))
        );
        assert_eq!(
            estimate_frame_rate(ticks90k(run(1e9 / 30.0, 30))),
            Some((30, 1))
        );
        assert_eq!(
            estimate_frame_rate(ticks90k(run(1001e9 / 60_000.0, 30))),
            Some((60000, 1001))
        );
    }

    #[test]
    fn reordered_and_dropped_frames() {
        let mut pts = run(40e6, 30);
        pts.swap(3, 4);
        pts.swap(10, 12);
        pts.remove(20);
        assert_eq!(estimate_frame_rate(pts), Some((25, 1)));
    }

    #[test]
    fn whole_rate_outside_the_table() {
        assert_eq!(estimate_frame_rate(run(1e9 / 15.0, 20)), Some((15, 1)));
    }

    #[test]
    fn no_estimate() {
        // Too few timestamps.
        assert_eq!(estimate_frame_rate(run(20e6, 5)), None);
        // All the same.
        assert_eq!(estimate_frame_rate(vec![7; 30]), None);
        // Not a frame rate.
        assert_eq!(estimate_frame_rate(run(1e9 / 27.3, 30)), None);
        assert_eq!(estimate_frame_rate(run(2e9, 30)), None);
    }
}
