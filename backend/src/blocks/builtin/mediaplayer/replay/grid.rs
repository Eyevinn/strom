//! The frame grid of a replay player's source, anchored on TAI.
//!
//! Frame `k` is the instant `phase + k * fps_d / fps_n` seconds TAI. The phase
//! is taken from the first segment the player sees, so material whose segments
//! do not start on a whole multiple of the frame period still lands exactly on
//! the grid. Frame numbers are absolute (counted from TAI 0 + phase). Each
//! player has its own grid; the group shares time, not frame numbers (see
//! `timeline`).

const NS: i128 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameGrid {
    pub fps_n: u32,
    pub fps_d: u32,
    /// TAI ns of a grid point, reduced to `[0, frame period)`.
    pub phase_ns: i64,
}

impl FrameGrid {
    /// A grid for `fps_n / fps_d` frames per second through the instant
    /// `anchor_tai_ns`. `None` for a zero rate.
    pub fn new(fps_n: u32, fps_d: u32, anchor_tai_ns: i64) -> Option<Self> {
        if fps_n == 0 || fps_d == 0 {
            return None;
        }
        let mut g = Self {
            fps_n,
            fps_d,
            phase_ns: 0,
        };
        // Phase = anchor minus the grid point at or below it.
        let k = g.floor_frame(anchor_tai_ns);
        g.phase_ns = (anchor_tai_ns as i128 - g.tai_of_frame_i128(k)) as i64;
        Some(g)
    }

    /// Frame period in ns (fractional for NTSC rates).
    pub fn frame_ns(&self) -> f64 {
        1e9 * self.fps_d as f64 / self.fps_n as f64
    }

    fn tai_of_frame_i128(&self, k: i64) -> i128 {
        // Round up to whole ns, so the result always lies inside frame k.
        let num = k as i128 * self.fps_d as i128 * NS;
        let den = self.fps_n as i128;
        self.phase_ns as i128 - (-num).div_euclid(den)
    }

    /// TAI ns where frame `k` starts (rounded up to a whole ns).
    pub fn tai_of_frame(&self, k: i64) -> i64 {
        self.tai_of_frame_i128(k) as i64
    }

    /// The frame whose period contains `tai_ns`.
    pub fn floor_frame(&self, tai_ns: i64) -> i64 {
        let num = (tai_ns as i128 - self.phase_ns as i128) * self.fps_n as i128;
        let den = self.fps_d as i128 * NS;
        num.div_euclid(den) as i64
    }

    /// The grid point nearest to `tai_ns`. Used for decoded frames, whose TAI
    /// can sit a little off the grid (timestamp rounding, writer jitter): up
    /// to half a frame either way still maps to the same frame on every angle.
    pub fn nearest_frame(&self, tai_ns: i64) -> i64 {
        let num = (tai_ns as i128 - self.phase_ns as i128) * self.fps_n as i128;
        let den = self.fps_d as i128 * NS;
        div_round(num, den) as i64
    }

    /// First frame at or after `tai_ns` (start of a half-open range).
    pub fn ceil_frame(&self, tai_ns: i64) -> i64 {
        let k = self.floor_frame(tai_ns);
        if self.tai_of_frame(k) < tai_ns {
            k + 1
        } else {
            k
        }
    }
}

/// Round-half-up division for a positive denominator.
fn div_round(num: i128, den: i128) -> i128 {
    (2 * num + den).div_euclid(2 * den)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: i64 = 1_800_000_000_000_000_000;

    #[test]
    fn integer_rate_on_grid() {
        let g = FrameGrid::new(50, 1, BASE).unwrap();
        assert_eq!(g.phase_ns, 0);
        assert_eq!(g.floor_frame(BASE), 90_000_000_000);
        assert_eq!(g.tai_of_frame(90_000_000_000), BASE);
        assert_eq!(g.floor_frame(BASE + 19_999_999), 90_000_000_000);
        assert_eq!(g.floor_frame(BASE + 20_000_000), 90_000_000_001);
        assert_eq!(g.ceil_frame(BASE + 1), 90_000_000_001);
        assert_eq!(g.ceil_frame(BASE), 90_000_000_000);
    }

    #[test]
    fn off_grid_anchor_sets_phase() {
        let anchor = BASE + 13_457_000;
        let g = FrameGrid::new(50, 1, anchor).unwrap();
        assert_eq!(g.phase_ns, 13_457_000);
        let k = g.floor_frame(anchor);
        assert_eq!(g.tai_of_frame(k), anchor);
        // Half a frame of jitter either side still lands on the same frame.
        assert_eq!(g.nearest_frame(anchor + 9_000_000), k);
        assert_eq!(g.nearest_frame(anchor - 9_000_000), k);
        assert_eq!(g.nearest_frame(anchor + 11_000_000), k + 1);
    }

    #[test]
    fn ntsc_rate_round_trips() {
        let g = FrameGrid::new(60000, 1001, BASE).unwrap();
        for k in [0i64, 1, 59, 60, 1_000_003] {
            let f = g.floor_frame(BASE) + k;
            let t = g.tai_of_frame(f);
            assert_eq!(g.floor_frame(t), f, "frame {}", k);
            assert_eq!(g.nearest_frame(t + 3), f);
        }
        let f0 = g.floor_frame(BASE);
        // 60000 frames of 1001/60000 s = 1001 s exactly.
        assert_eq!(
            g.tai_of_frame(f0 + 60000) - g.tai_of_frame(f0),
            1_001 * 1_000_000_000
        );
    }

    #[test]
    fn negative_and_zero_rate() {
        assert!(FrameGrid::new(0, 1, 0).is_none());
        let g = FrameGrid::new(25, 1, 0).unwrap();
        assert_eq!(g.floor_frame(-1), -1);
        assert_eq!(g.tai_of_frame(-1), -40_000_000);
    }
}
