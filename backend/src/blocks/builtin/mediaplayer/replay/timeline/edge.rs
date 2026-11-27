//! The live edge between its steps.

/// Steps of the live edge kept for the estimate.
const EDGE_STEPS: usize = 16;
/// Edge steps closer together than this (host time) count as one.
const EDGE_COALESCE_NS: i64 = 250_000_000;

/// The live edge between its steps. The index grows one segment at a time,
/// so the known edge jumps (once a second for 1 s segments) while capture
/// goes on all the time. Each step tells when (host monotonic time) media up
/// to which instant was known; the media grows at 1x, so the edge now is at
/// least `m_now + max(e_k - m_k)` over recent steps (the step seen with the
/// least delay sets the line). Kept within one step of the known edge, and
/// only while the edge grows. The estimate never steps back (until the edge
/// itself goes back: a new run or another source).
#[derive(Debug, Default)]
pub struct EdgeTracker {
    steps: std::collections::VecDeque<(i64, i64)>,
    /// The last estimate given: the floor of the next one.
    last: Option<i64>,
}

impl EdgeTracker {
    /// The known edge `edge` at host time `mono` (ns, monotonic).
    pub fn observe(&mut self, mono: i64, edge: i64) {
        match self.steps.back() {
            Some(&(_, last)) if edge == last => return,
            // Went back: a new run or another source. Start over.
            Some(&(_, last)) if edge < last => {
                self.steps.clear();
                self.last = None;
            }
            _ => {}
        }
        // Angles step one after the other: the common edge moves a few ms
        // when the first does, the rest when the last does. Steps that close
        // in time are one step, seen when it completed.
        if let Some(last) = self.steps.back_mut() {
            if mono - last.0 < EDGE_COALESCE_NS {
                *last = (mono, edge);
                return;
            }
        }
        self.steps.push_back((mono, edge));
        while self.steps.len() > EDGE_STEPS {
            self.steps.pop_front();
        }
    }

    /// Host time (monotonic ns) the edge last stepped.
    pub fn last_seen(&self) -> Option<i64> {
        self.steps.back().map(|&(m, _)| m)
    }

    /// Median step of the edge, once it has stepped twice.
    pub fn step(&self) -> Option<i64> {
        let mut d: Vec<i64> = self
            .steps
            .iter()
            .zip(self.steps.iter().skip(1))
            .map(|(a, b)| b.1 - a.1)
            .collect();
        if d.len() < 2 {
            return None;
        }
        d.sort_unstable();
        Some(d[d.len() / 2])
    }

    /// The estimated edge at `mono`, and whether the edge is growing. Not
    /// growing (fewer than three steps, or none for three step lengths): the
    /// known edge, or the last estimate if that was later.
    pub fn estimate(&mut self, mono: i64, edge: i64) -> (i64, bool) {
        let (raw, growing) = self.raw_estimate(mono, edge);
        let est = self.last.map_or(raw, |l| l.max(raw));
        self.last = Some(est);
        (est, growing)
    }

    fn raw_estimate(&self, mono: i64, edge: i64) -> (i64, bool) {
        let (Some(step), Some(&(last_m, _))) = (self.step(), self.steps.back()) else {
            return (edge, false);
        };
        if mono - last_m > 3 * step {
            return (edge, false);
        }
        let offset = self.steps.iter().map(|&(m, e)| e - m).max().unwrap_or(0);
        ((mono + offset).clamp(edge, edge + step), true)
    }
}
