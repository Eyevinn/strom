//! QoS messages from a flow's bus: who dropped late data, how much, and how
//! often to say so.
//!
//! An element posts a QoS message when it drops late data (or lowers its
//! quality to keep up). The message's `processed` and `dropped` counts are
//! cumulative per source. A pad can be the source: `GstAudioAggregator` posts
//! one per input pad, so the drop is reported against the element that owns
//! the pad, and the pad name says which input it was (#1265).

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Who posted a QoS message: an element, or one input pad of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct QosSource {
    /// Name of the element (the pad's parent when a pad posted it).
    pub element_name: String,
    /// Name of the pad that posted it, if a pad did.
    pub pad_name: Option<String>,
}

/// One QoS message, read off the bus.
#[derive(Debug, Clone)]
pub(super) struct QosSample {
    pub source: QosSource,
    /// The element linked to the posting pad: the input that was late.
    pub upstream_element: Option<String>,
    pub proportion: f64,
    pub jitter: i64,
    /// Cumulative processed count, when the element reports it.
    pub processed: Option<u64>,
    /// Cumulative dropped count, when the element reports it.
    pub dropped: Option<u64>,
    /// Unit of `processed` and `dropped`.
    pub stats_format: Option<String>,
}

/// Read a QoS message. None when it has no source.
pub(super) fn parse_qos(qos: &gst::message::Qos) -> Option<QosSample> {
    let src = qos.src()?;
    let (jitter, proportion, _quality) = qos.values();
    // gstreamer-rs returns (processed, dropped), both in the same format.
    let (processed, dropped) = qos.stats();

    let pad = src.downcast_ref::<gst::Pad>();
    let source = match pad {
        Some(pad) => QosSource {
            element_name: pad
                .parent_element()
                .map(|e| e.name().to_string())
                .unwrap_or_else(|| pad.name().to_string()),
            pad_name: Some(pad.name().to_string()),
        },
        None => QosSource {
            element_name: src.name().to_string(),
            pad_name: None,
        },
    };
    let upstream_element = pad
        .and_then(|p| p.peer())
        .and_then(|peer| peer.parent_element())
        .map(|e| e.name().to_string());

    let format = processed.format();
    let stats_format = format_name(format, pad);
    let (processed, dropped) = if stats_format.is_some() {
        (count(processed.value()), count(dropped.value()))
    } else {
        (None, None)
    };

    Some(QosSample {
        source,
        upstream_element,
        proportion,
        jitter,
        processed,
        dropped,
        stats_format,
    })
}

/// A count from a QoS message: -1 (unknown) and other negatives are None.
fn count(raw: i64) -> Option<u64> {
    u64::try_from(raw).ok()
}

/// The unit of a QoS message's counts. `GST_FORMAT_DEFAULT` means samples for
/// audio and frames for video, read from the pad's caps when a pad posted it.
fn format_name(format: gst::Format, pad: Option<&gst::Pad>) -> Option<String> {
    let name = match format {
        gst::Format::Undefined => return None,
        gst::Format::Buffers => "buffers",
        gst::Format::Bytes => "bytes",
        gst::Format::Time => "ns",
        gst::Format::Default => {
            let media = pad
                .and_then(|p| p.current_caps())
                .and_then(|caps| caps.structure(0).map(|s| s.name().to_string()));
            match media.as_deref() {
                Some(m) if m.starts_with("audio/") => "samples",
                Some(m) if m.starts_with("video/") => "frames",
                _ => "default",
            }
        }
        _ => "default",
    };
    Some(name.to_string())
}

/// QoS messages from one source within one aggregation period.
#[derive(Debug, Clone)]
pub(super) struct QosPeriod {
    pub event_count: u64,
    sum_proportion: f64,
    pub min_proportion: f64,
    pub max_proportion: f64,
    sum_jitter: i64,
    /// Latest cumulative processed count.
    pub processed: Option<u64>,
    /// Latest cumulative dropped count.
    pub dropped: Option<u64>,
    /// Dropped in this period: the growth of `dropped` since the last period,
    /// or the message count when the source reports no dropped count. Set by
    /// [`QoSAggregator::extract_and_reset`].
    pub dropped_in_period: u64,
    pub stats_format: Option<String>,
    pub upstream_element: Option<String>,
}

impl QosPeriod {
    fn new() -> Self {
        Self {
            event_count: 0,
            sum_proportion: 0.0,
            min_proportion: f64::MAX,
            max_proportion: f64::MIN,
            sum_jitter: 0,
            processed: None,
            dropped: None,
            dropped_in_period: 0,
            stats_format: None,
            upstream_element: None,
        }
    }

    fn add(&mut self, sample: QosSample) {
        self.event_count = self.event_count.saturating_add(1);
        self.sum_proportion += sample.proportion;
        self.min_proportion = self.min_proportion.min(sample.proportion);
        self.max_proportion = self.max_proportion.max(sample.proportion);
        // Saturating add: QoS jitter values can be huge when the pipeline clock is
        // NTP/TAI (time since 1900/1970 in ns), and the running sum may overflow i64.
        self.sum_jitter = self.sum_jitter.saturating_add(sample.jitter);
        // The counts are cumulative: keep the latest.
        self.processed = sample.processed.or(self.processed);
        self.dropped = sample.dropped.or(self.dropped);
        if sample.stats_format.is_some() {
            self.stats_format = sample.stats_format;
        }
        if sample.upstream_element.is_some() {
            self.upstream_element = sample.upstream_element;
        }
    }

    pub fn avg_proportion(&self) -> f64 {
        if self.event_count > 0 {
            self.sum_proportion / self.event_count as f64
        } else {
            0.0
        }
    }

    pub fn avg_jitter(&self) -> i64 {
        if self.event_count > 0 {
            self.sum_jitter / self.event_count as i64
        } else {
            0
        }
    }

    /// Whether the source dropped late data in the period. The proportion
    /// says nothing about drops: an `audiomixer` input reports 1.0 while it
    /// drops everything that reaches it.
    pub fn is_falling_behind(&self) -> bool {
        self.dropped_in_period > 0
    }
}

#[derive(Debug, Default)]
struct AggregatorState {
    periods: HashMap<QosSource, QosPeriod>,
    /// Cumulative dropped count per source at the end of the last period.
    last_dropped: HashMap<QosSource, u64>,
}

/// Collects a flow's QoS messages per source, for the periodic broadcast.
#[derive(Debug, Clone, Default)]
pub(super) struct QoSAggregator {
    state: Arc<Mutex<AggregatorState>>,
}

impl QoSAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, sample: QosSample) {
        let mut state = self.state.lock().unwrap();
        state
            .periods
            .entry(sample.source.clone())
            .or_insert_with(QosPeriod::new)
            .add(sample);
    }

    /// The period's stats per source, with `dropped_in_period` filled in. The
    /// next period starts empty; the cumulative dropped counts carry over.
    pub fn extract_and_reset(&self) -> Vec<(QosSource, QosPeriod)> {
        let mut state = self.state.lock().unwrap();
        let periods = std::mem::take(&mut state.periods);
        periods
            .into_iter()
            .map(|(source, mut period)| {
                period.dropped_in_period = match period.dropped {
                    Some(total) => {
                        let previous = state.last_dropped.insert(source.clone(), total);
                        dropped_since(previous, total)
                    }
                    None => period.event_count,
                };
                (source, period)
            })
            .collect()
    }
}

/// Dropped since the previous cumulative count. A count below the previous
/// one means the counter restarted (a flush, a new pad), so all of it is new.
fn dropped_since(previous: Option<u64>, total: u64) -> u64 {
    match previous {
        Some(previous) if total >= previous => total - previous,
        _ => total,
    }
}

/// How long drops from one source are summed before they are logged again.
pub(super) const DROP_LOG_INTERVAL: Duration = Duration::from_secs(10);

struct DropLogEntry {
    last_logged: Instant,
    pending_dropped: u64,
    pending_events: u64,
    description: String,
}

/// A drop report ready to log.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct DropLogLine {
    pub description: String,
    pub dropped: u64,
    pub events: u64,
    /// The first report after a quiet interval.
    pub first: bool,
}

/// Logs the first drop from a source at once, then the sum at most once per
/// [`DROP_LOG_INTERVAL`]. A dropping mixer input posts a QoS message for every
/// buffer, dozens per second.
#[derive(Default)]
pub(super) struct DropLogGate {
    entries: HashMap<QosSource, DropLogEntry>,
}

impl DropLogGate {
    /// Record a period's drops from `source`. Returns a line to log now, if any.
    pub fn record(
        &mut self,
        source: &QosSource,
        description: String,
        dropped: u64,
        events: u64,
        now: Instant,
    ) -> Option<DropLogLine> {
        if dropped == 0 {
            return None;
        }
        match self.entries.get_mut(source) {
            None => {
                self.entries.insert(
                    source.clone(),
                    DropLogEntry {
                        last_logged: now,
                        pending_dropped: 0,
                        pending_events: 0,
                        description: description.clone(),
                    },
                );
                Some(DropLogLine {
                    description,
                    dropped,
                    events,
                    first: true,
                })
            }
            Some(entry) => {
                entry.pending_dropped = entry.pending_dropped.saturating_add(dropped);
                entry.pending_events = entry.pending_events.saturating_add(events);
                entry.description = description;
                if now.duration_since(entry.last_logged) >= DROP_LOG_INTERVAL {
                    Some(Self::take(entry, now))
                } else {
                    None
                }
            }
        }
    }

    /// Lines for drops held back for a full interval, and forget sources that
    /// have been quiet that long, so their next drop is logged at once.
    pub fn flush(&mut self, now: Instant) -> Vec<DropLogLine> {
        let mut lines = Vec::new();
        self.entries.retain(|_, entry| {
            if now.duration_since(entry.last_logged) < DROP_LOG_INTERVAL {
                return true;
            }
            if entry.pending_dropped == 0 {
                return false;
            }
            lines.push(Self::take(entry, now));
            true
        });
        lines
    }

    fn take(entry: &mut DropLogEntry, now: Instant) -> DropLogLine {
        let line = DropLogLine {
            description: entry.description.clone(),
            dropped: entry.pending_dropped,
            events: entry.pending_events,
            first: false,
        };
        entry.last_logged = now;
        entry.pending_dropped = 0;
        entry.pending_events = 0;
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(pad: &str) -> QosSource {
        QosSource {
            element_name: "mixer:audiomixer".into(),
            pad_name: Some(pad.into()),
        }
    }

    fn sample(pad: &str, proportion: f64, processed: u64, dropped: Option<u64>) -> QosSample {
        QosSample {
            source: source(pad),
            upstream_element: None,
            proportion,
            jitter: 0,
            processed: Some(processed),
            dropped,
            stats_format: Some("samples".into()),
        }
    }

    /// A QoS message from an aggregator pad: processed and dropped come out in
    /// that order, and the drop is put on the pad's element, with the pad.
    #[test]
    fn a_pad_qos_message_is_read_as_its_element_and_pad() {
        gst::init().unwrap();
        let mixer = gst::ElementFactory::make("audiomixer")
            .name("mixer:audiomixer")
            .build()
            .unwrap();
        let pad = mixer.request_pad_simple("sink_%u").unwrap();
        let msg = gst::message::Qos::builder(true)
            .running_time(gst::ClockTime::from_seconds(1))
            .duration(gst::ClockTime::from_mseconds(20))
            .values(0, 1.0, 1_000_000)
            .stats(
                gst::GenericFormattedValue::new(gst::Format::Default, 3200),
                gst::GenericFormattedValue::new(gst::Format::Default, 1920),
            )
            .src(&pad)
            .build();
        let gst::MessageView::Qos(qos) = msg.view() else {
            unreachable!()
        };
        let sample = parse_qos(qos).expect("a QoS message with a source");
        assert_eq!(sample.source.element_name, "mixer:audiomixer");
        assert_eq!(sample.source.pad_name.as_deref(), Some(pad.name().as_str()));
        assert_eq!(sample.processed, Some(3200));
        assert_eq!(sample.dropped, Some(1920));
        assert_eq!(sample.stats_format.as_deref(), Some("default"));
    }

    #[test]
    fn an_element_qos_message_without_stats_has_no_counts() {
        gst::init().unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .name("out")
            .build()
            .unwrap();
        let msg = gst::message::Qos::builder(true)
            .running_time(gst::ClockTime::ZERO)
            .src(&sink)
            .build();
        let gst::MessageView::Qos(qos) = msg.view() else {
            unreachable!()
        };
        let sample = parse_qos(qos).unwrap();
        assert_eq!(sample.source.element_name, "out");
        assert_eq!(sample.source.pad_name, None);
        assert_eq!(sample.processed, None);
        assert_eq!(sample.dropped, None);
    }

    #[test]
    fn drops_per_period_are_the_growth_of_the_cumulative_count() {
        let agg = QoSAggregator::new();
        agg.add(sample("sink_1", 1.0, 100, Some(500)));
        agg.add(sample("sink_1", 1.0, 200, Some(900)));
        let first = agg.extract_and_reset();
        assert_eq!(first.len(), 1);
        let p = &first[0].1;
        assert_eq!(p.event_count, 2);
        assert_eq!(p.processed, Some(200));
        assert_eq!(p.dropped, Some(900));
        assert_eq!(p.dropped_in_period, 900);
        assert!(p.is_falling_behind());

        agg.add(sample("sink_1", 1.0, 300, Some(1200)));
        let second = agg.extract_and_reset();
        assert_eq!(second[0].1.dropped_in_period, 300);

        // The counter restarted (flush, new pad): all of it is new.
        agg.add(sample("sink_1", 1.0, 10, Some(40)));
        let third = agg.extract_and_reset();
        assert_eq!(third[0].1.dropped_in_period, 40);
    }

    #[test]
    fn a_message_without_new_drops_is_not_falling_behind() {
        let agg = QoSAggregator::new();
        agg.add(sample("sink_1", 1.0, 100, Some(500)));
        agg.extract_and_reset();
        // Same cumulative count: a quality change, no drop.
        agg.add(sample("sink_1", 0.5, 150, Some(500)));
        let period = agg.extract_and_reset();
        assert_eq!(period[0].1.dropped_in_period, 0);
        assert!(!period[0].1.is_falling_behind());
    }

    /// The old health rule was `avg_proportion < 1.0`. A mixer input that
    /// drops everything reports 1.0, and a sink that keeps up reports < 1.0.
    #[test]
    fn health_follows_drops_not_proportion() {
        let agg = QoSAggregator::new();
        agg.add(sample("sink_1", 1.0, 3200, Some(1920)));
        let period = agg.extract_and_reset();
        assert!(period[0].1.is_falling_behind());
    }

    #[test]
    fn without_a_dropped_count_each_message_counts_as_a_drop() {
        let agg = QoSAggregator::new();
        agg.add(sample("sink_1", 1.2, 0, None));
        agg.add(sample("sink_1", 1.2, 0, None));
        let period = agg.extract_and_reset();
        assert_eq!(period[0].1.dropped_in_period, 2);
    }

    #[test]
    fn inputs_of_one_element_are_kept_apart() {
        let agg = QoSAggregator::new();
        agg.add(sample("sink_0", 1.0, 100, Some(10)));
        agg.add(sample("sink_1", 1.0, 100, Some(20)));
        let mut periods = agg.extract_and_reset();
        periods.sort_by(|a, b| a.0.pad_name.cmp(&b.0.pad_name));
        assert_eq!(periods.len(), 2);
        assert_eq!(periods[0].1.dropped_in_period, 10);
        assert_eq!(periods[1].1.dropped_in_period, 20);
    }

    #[test]
    fn the_first_drop_is_logged_then_at_most_once_per_interval() {
        let mut gate = DropLogGate::default();
        let src = source("sink_1");
        let t0 = Instant::now();
        let first = gate.record(&src, "d".into(), 5, 1, t0).unwrap();
        assert!(first.first);
        assert_eq!(first.dropped, 5);
        for s in 1..10 {
            assert_eq!(
                gate.record(&src, "d".into(), 7, 2, t0 + Duration::from_secs(s)),
                None
            );
        }
        let later = gate
            .record(&src, "d".into(), 7, 2, t0 + DROP_LOG_INTERVAL)
            .unwrap();
        assert!(!later.first);
        assert_eq!(later.dropped, 70);
        assert_eq!(later.events, 20);
    }

    #[test]
    fn held_back_drops_are_flushed_and_a_quiet_source_is_forgotten() {
        let mut gate = DropLogGate::default();
        let src = source("sink_1");
        let t0 = Instant::now();
        gate.record(&src, "d".into(), 5, 1, t0);
        gate.record(&src, "d".into(), 3, 1, t0 + Duration::from_secs(2));
        assert!(gate.flush(t0 + Duration::from_secs(5)).is_empty());
        let lines = gate.flush(t0 + DROP_LOG_INTERVAL);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].dropped, 3);
        // Quiet for another interval: forgotten, the next drop logs at once.
        assert!(gate.flush(t0 + 2 * DROP_LOG_INTERVAL).is_empty());
        let next = gate
            .record(&src, "d".into(), 1, 1, t0 + 2 * DROP_LOG_INTERVAL)
            .unwrap();
        assert!(next.first);
    }

    #[test]
    fn no_drops_log_nothing() {
        let mut gate = DropLogGate::default();
        assert_eq!(
            gate.record(&source("sink_1"), "d".into(), 0, 3, Instant::now()),
            None
        );
    }
}
