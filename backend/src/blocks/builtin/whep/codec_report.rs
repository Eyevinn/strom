//! Which of the video codecs a WHEP output asked for actually survived
//! `whepserversink`'s codec discovery.
//!
//! Before it will offer a codec, `whepserversink` builds a throwaway pipeline
//! per codec - encoder, parser, a `codec-parser-caps` capsfilter, payloader,
//! appsink - and keeps the codec only if payloaded caps come out the far end.
//! A codec whose pipeline errors is dropped from the offer and the element
//! carries on with whatever is left. Nothing observable comes out of that:
//! the discovery pipeline's bus sync handler returns `Drop` for every message,
//! no property lists the surviving codecs, and the sink still reaches
//! `PLAYING` and passes data on the codecs that did work.
//!
//! The one hook into that pipeline is `request-encoded-filter`, which asks for
//! an element to splice in immediately downstream of `codec-parser-caps` -
//! the capsfilter that rejects the encoder output when discovery fails. An
//! `identity` handed over there sees a CAPS event exactly when that codec got
//! past the encoder, and sees nothing when it did not.
//!
//! Only the first round of discovery counts. That is the one that runs when
//! the input caps arrive, with output caps of `ANY`, and its result becomes the
//! codec list the sink will answer a viewer with. Every later round belongs to
//! a viewer that has already connected and is asked a different question - its
//! output caps come from that viewer's SDP, so `force_profile` is false and the
//! `codec-parser-caps` filter stops demanding `constrained-baseline`. H.264
//! therefore passes on the viewer path even when it failed at startup, and
//! letting that clear the verdict would report the block healthy while its
//! answer still has no H.264 in it.
//!
//! Audio is left alone. There is one audio codec to discover, and a WHEP
//! output that loses it stalls a pad task in the flow pipeline, which the
//! health scan already finds.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::gst::BlockDiagnostic;

/// How long after the last discovery pipeline was built before its outcome is
/// treated as final.
///
/// Discovery pipelines for every codec are built back to back and then run
/// concurrently, so the clock starts at the last one. A codec that is simply
/// slow to negotiate - a software AV1 encoder on a loaded machine - must not
/// be reported as dropped, and the cost of waiting is only that an operator
/// sees the warning a few seconds later than it happened.
const SETTLE_AFTER: Duration = Duration::from_secs(15);

/// What became of one codec's discovery pipeline.
struct Attempt {
    /// Encoder factory `whepserversink` picked for this codec, once
    /// `encoder-setup` has named it.
    encoder: Option<String>,
    /// Set when a CAPS event reaches the spliced-in filter, which only happens
    /// if the encoder output was accepted.
    negotiated: Arc<AtomicBool>,
}

#[derive(Default)]
struct Inner {
    /// Video codec media types the block asked `whepserversink` to offer, in
    /// the order they appear in `video-caps`.
    requested: Vec<String>,
    /// Codec media type to the outcome of its discovery. Codecs discovery
    /// never attempted are absent.
    attempts: BTreeMap<String, Attempt>,
    /// When the most recent discovery pipeline was built.
    last_attempt_at: Option<Instant>,
    /// Verdict on the first round of discovery. Held from the moment it is
    /// reached; later rounds answer a different question and cannot revise it.
    verdict: Option<String>,
    /// Whether `verdict` is final.
    decided: bool,
}

/// Per-block record of which requested video codecs `whepserversink` kept.
pub struct CodecDiscoveryReport {
    block_id: String,
    settle_after: Duration,
    inner: Mutex<Inner>,
}

impl CodecDiscoveryReport {
    pub fn new(block_id: String) -> Arc<Self> {
        Self::with_settle(block_id, SETTLE_AFTER)
    }

    /// As [`new`](Self::new) with the settling delay overridden, so tests do
    /// not have to wait out a real discovery round.
    pub fn with_settle(block_id: String, settle_after: Duration) -> Arc<Self> {
        Arc::new(Self {
            block_id,
            settle_after,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Record the codecs `video-caps` asks for. Call once the block has decided
    /// what to put on the sink, reading the property back rather than the
    /// intended value so that the element's own default is recorded when the
    /// block leaves it alone.
    pub fn set_requested(&self, video_caps: &gst::Caps) {
        let requested: Vec<String> = (0..video_caps.size())
            .filter_map(|i| video_caps.structure(i))
            .map(|s| s.name().to_string())
            .filter(|name| name.starts_with("video/") && name != "video/x-raw")
            .collect();
        self.inner.lock().unwrap().requested = requested;
    }

    /// Install the signal handlers this report reads.
    ///
    /// Both handlers capture only this `Arc`, never the sink, so the element
    /// owning them does not end up in a reference cycle.
    pub fn attach(self: &Arc<Self>, sink: &gst::Element) {
        let encoder_report = Arc::clone(self);
        sink.connect("encoder-setup", false, move |values| {
            // Discovery names its consumer "discovery"; a real session names
            // the peer. Only discovery decides what gets offered.
            let consumer = values[1].get::<Option<String>>().unwrap_or(None);
            if consumer.as_deref() != Some("discovery") {
                return Some(false.to_value());
            }
            if let Ok(encoder) = values[3].get::<gst::Element>() {
                encoder_report.record_encoder(&encoder);
            }
            // False leaves webrtcsink's own encoder configuration in place.
            Some(false.to_value())
        });

        let filter_report = Arc::clone(self);
        sink.connect("request-encoded-filter", false, move |values| {
            // Discovery passes no consumer id, a real session does. Splicing an
            // element into a session pipeline would change what viewers get.
            let consumer = values[1].get::<Option<String>>().unwrap_or(None);
            if consumer.is_some() {
                return Some(None::<gst::Element>.to_value());
            }
            let codec = values[3]
                .get::<gst::Caps>()
                .ok()
                .and_then(|caps| caps.structure(0).map(|s| s.name().to_string()));
            let Some(codec) = codec.filter(|c| c.starts_with("video/")) else {
                return Some(None::<gst::Element>.to_value());
            };
            Some(
                filter_report
                    .make_probe_filter(&codec)
                    .map(|e| e.to_value())
                    .unwrap_or_else(|| None::<gst::Element>.to_value()),
            )
        });
    }

    /// Note the encoder discovery picked, keyed by what that encoder produces
    /// rather than by call order, since discovery runs per codec concurrently.
    fn record_encoder(&self, encoder: &gst::Element) {
        let Some(factory) = encoder.factory() else {
            return;
        };
        let Some(codec) = encoder_output_media_type(&factory) else {
            return;
        };
        let mut inner = self.inner.lock().unwrap();
        if inner.decided {
            return;
        }
        inner
            .attempts
            .entry(codec)
            .or_insert_with(|| Attempt {
                encoder: None,
                negotiated: Arc::new(AtomicBool::new(false)),
            })
            .encoder = Some(factory.name().to_string());
    }

    /// Build the `identity` spliced in below `codec-parser-caps` for `codec`,
    /// and register the flag its pad probe sets.
    fn make_probe_filter(&self, codec: &str) -> Option<gst::Element> {
        let identity = gst::ElementFactory::make("identity")
            .name(format!(
                "{}:codec-probe:{}",
                self.block_id,
                codec.replace('/', "-")
            ))
            .build()
            .ok()?;

        let negotiated = {
            let mut inner = self.inner.lock().unwrap();
            if inner.decided {
                return None;
            }
            inner.last_attempt_at = Some(Instant::now());
            let attempt = inner.attempts.entry(codec.to_string()).or_insert(Attempt {
                encoder: None,
                negotiated: Arc::new(AtomicBool::new(false)),
            });
            Arc::clone(&attempt.negotiated)
        };

        let sink_pad = identity.static_pad("sink")?;
        sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                if event.type_() == gst::EventType::Caps {
                    negotiated.store(true, Ordering::SeqCst);
                }
            }
            gst::PadProbeReturn::Ok
        });

        Some(identity)
    }

    /// Requested codecs that discovery did not deliver, with the reason for
    /// each. Empty while discovery is still settling or nothing is missing.
    fn missing(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        inner
            .requested
            .iter()
            .filter_map(|codec| match inner.attempts.get(codec) {
                None => Some(format!(
                    "{} was not offered: no encoder for it is installed",
                    codec_label(codec)
                )),
                Some(attempt) if !attempt.negotiated.load(Ordering::SeqCst) => Some(format!(
                    "{} was dropped from the offer: {} failed codec discovery",
                    codec_label(codec),
                    attempt.encoder.as_deref().unwrap_or("its encoder"),
                )),
                Some(_) => None,
            })
            .collect()
    }
}

impl BlockDiagnostic for CodecDiscoveryReport {
    fn block_id(&self) -> &str {
        &self.block_id
    }

    fn degraded(&self) -> Option<String> {
        let settled = {
            let inner = self.inner.lock().unwrap();
            if inner.decided {
                return inner.verdict.clone();
            }
            // No discovery yet means nothing to conclude, not "all missing".
            inner.last_attempt_at?.elapsed() >= self.settle_after
        };
        if !settled {
            return None;
        }

        let missing = self.missing();
        let verdict = if missing.is_empty() {
            None
        } else {
            Some(missing.join("; "))
        };
        let mut inner = self.inner.lock().unwrap();
        inner.verdict = verdict.clone();
        inner.decided = true;
        verdict
    }
}

/// The media type an encoder produces, read from its factory's src pad
/// template. This is what ties an `encoder-setup` callback to the codec being
/// discovered without relying on the order the concurrent discoveries run in.
fn encoder_output_media_type(factory: &gst::ElementFactory) -> Option<String> {
    factory
        .static_pad_templates()
        .into_iter()
        .find(|t| t.direction() == gst::PadDirection::Src)
        .and_then(|t| t.caps().structure(0).map(|s| s.name().to_string()))
}

/// Codec media type as an operator would name it.
fn codec_label(media_type: &str) -> &str {
    match media_type {
        "video/x-h264" => "H.264",
        "video/x-h265" => "H.265",
        "video/x-vp8" => "VP8",
        "video/x-vp9" => "VP9",
        "video/x-av1" => "AV1",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand in for a discovery round: `codec` was tried with `encoder`, and
    /// `negotiated` says whether its caps got past the parser capsfilter.
    fn record(report: &CodecDiscoveryReport, codec: &str, encoder: &str, negotiated: bool) {
        let mut inner = report.inner.lock().unwrap();
        inner.last_attempt_at = Some(Instant::now());
        inner.attempts.insert(
            codec.to_string(),
            Attempt {
                encoder: Some(encoder.to_string()),
                negotiated: Arc::new(AtomicBool::new(negotiated)),
            },
        );
    }

    fn report_for(requested: &[&str]) -> Arc<CodecDiscoveryReport> {
        let report = CodecDiscoveryReport::with_settle("whep".to_string(), Duration::ZERO);
        report.inner.lock().unwrap().requested = requested.iter().map(|c| c.to_string()).collect();
        report
    }

    #[test]
    fn every_requested_codec_negotiating_is_not_degraded() {
        let report = report_for(&["video/x-h264", "video/x-vp9"]);
        record(&report, "video/x-h264", "x264enc", true);
        record(&report, "video/x-vp9", "vp9enc", true);
        assert_eq!(report.degraded(), None);
    }

    #[test]
    fn a_codec_whose_discovery_failed_names_its_encoder() {
        let report = report_for(&["video/x-h264", "video/x-vp9"]);
        record(&report, "video/x-h264", "vtenc_h264", false);
        record(&report, "video/x-vp9", "vp9enc", true);
        assert_eq!(
            report.degraded().as_deref(),
            Some("H.264 was dropped from the offer: vtenc_h264 failed codec discovery")
        );
    }

    #[test]
    fn a_codec_discovery_never_tried_reads_as_missing_encoder() {
        let report = report_for(&["video/x-h264", "video/x-av1"]);
        record(&report, "video/x-h264", "x264enc", true);
        assert_eq!(
            report.degraded().as_deref(),
            Some("AV1 was not offered: no encoder for it is installed")
        );
    }

    /// Nothing has been discovered yet, so there is no verdict to give - not
    /// even "everything is missing".
    #[test]
    fn no_discovery_yet_is_not_degraded() {
        let report = report_for(&["video/x-h264"]);
        assert_eq!(report.degraded(), None);
    }

    /// A viewer connecting makes whepserversink run discovery again, with the
    /// viewer's own SDP caps instead of ANY, so `codec-parser-caps` stops
    /// demanding constrained-baseline and H.264 passes where it failed at
    /// startup. That says nothing about what the sink will answer with, so it
    /// must not revise the verdict.
    #[test]
    fn a_later_round_cannot_revise_the_verdict() {
        let report = report_for(&["video/x-h264"]);
        record(&report, "video/x-h264", "vtenc_h264", false);
        let first = report.degraded();
        assert_eq!(
            first.as_deref(),
            Some("H.264 was dropped from the offer: vtenc_h264 failed codec discovery")
        );

        // The viewer's round negotiates the same codec.
        record(&report, "video/x-h264", "vtenc_h264", true);
        assert_eq!(report.degraded(), first);
    }

    /// A codec that has not finished negotiating is not yet a dropped codec.
    #[test]
    fn an_unsettled_round_reports_nothing() {
        let report = CodecDiscoveryReport::with_settle("whep".to_string(), Duration::from_secs(60));
        report.inner.lock().unwrap().requested = vec!["video/x-h264".to_string()];
        record(&report, "video/x-h264", "vtenc_h264", false);
        assert_eq!(report.degraded(), None);
    }

    /// Once the first round has been judged healthy it stays healthy, and the
    /// probe stops instrumenting later rounds.
    #[test]
    fn a_healthy_first_round_is_also_final() {
        let report = report_for(&["video/x-h264"]);
        record(&report, "video/x-h264", "vtenc_h264", true);
        assert_eq!(report.degraded(), None);
        assert!(report.inner.lock().unwrap().decided);
        assert!(report.make_probe_filter("video/x-h264").is_none());
    }

    #[test]
    fn requested_codecs_come_from_the_caps_the_sink_was_given() {
        gst::init().unwrap();
        let report = CodecDiscoveryReport::new("whep".to_string());
        let mut caps = gst::Caps::new_empty();
        {
            let m = caps.get_mut().unwrap();
            m.append(gst::Caps::builder("video/x-h264").build());
            m.append(gst::Caps::builder("video/x-raw").build());
            m.append(gst::Caps::builder("video/x-vp9").build());
        }
        report.set_requested(&caps);
        assert_eq!(
            report.inner.lock().unwrap().requested,
            vec!["video/x-h264".to_string(), "video/x-vp9".to_string()]
        );
    }
}
