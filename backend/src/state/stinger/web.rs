//! Page stinger takes: an HTML Input in stinger mode on the stinger input.
//!
//! The page's start is not known until it paints, so a take raises the
//! graphic pad, triggers the page, finds the page's first frame
//! ([`crate::stinger::web`]) and only then programs the take, from that
//! frame. Reporting and finishing are a clip take's.

use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::stinger::{
    StingerBeneath, StingerClip, StingerClipSettings, StingerSourceKind, StingerState,
    StingerTakeReport, StingerTakeResponse, StingerVariant,
};
use strom_types::{FlowId, PropertyValue, StromEvent};
use tracing::{debug, warn};

use super::{err, take_on_grid, AppState, PageContext, Started};
use crate::blocks::builtin::html_input;
use crate::gst::pipeline::PipelineError;
use crate::stinger::web::{
    check_page_url, take_fragment, web_first_output_frame, WebAnchor, WebStart, WebStingerSettings,
    PAGE_FIRST_FRAME_DELAY_FIXED_RATE, PAGE_FIRST_FRAME_DELAY_PAINTED, PAGE_FIRST_FRAME_GRACE,
    VARIABLE_RATE_PROPERTY,
};
use crate::stinger::FrameGrid;

/// How long past the grace period a take waits for the page's output to
/// move before it times the page from the take anyway. A paint-only page
/// that paints nothing leaves the output still.
const SILENT_PAGE_LIMIT: Duration = Duration::from_millis(400);

/// A 30 fps grid, to judge a page's settings while its flow is stopped.
const NOMINAL_GRID: FrameGrid = FrameGrid { num: 30, den: 1 };

/// How many page frames, stamped every `page_frame_ns` from `first_ns`, fall
/// in `[from, to)`.
fn page_frames_between(first_ns: u64, page_frame_ns: u64, from: u64, to: u64) -> u32 {
    let index_at = |t: u64| t.saturating_sub(first_ns).div_ceil(page_frame_ns.max(1));
    index_at(to).saturating_sub(index_at(from)) as u32
}

impl PageContext {
    fn settings(&self) -> WebStingerSettings {
        WebStingerSettings::from_properties(&self.properties)
    }

    fn configured_url(&self) -> String {
        html_input::url(&self.properties)
    }

    /// How long one page frame lasts, from the rate cefsrc negotiated, or
    /// the block's when that is variable or not known yet.
    fn frame_ns(&self, cefsrc_src: &gst::Pad) -> u64 {
        cefsrc_src
            .current_caps()
            .and_then(|c| c.structure(0)?.get::<gst::Fraction>("framerate").ok())
            .filter(|f| f.numer() > 0 && f.denom() > 0)
            .map(|f| 1_000_000_000 * f.denom() as u64 / f.numer() as u64)
            .unwrap_or_else(|| 1_000_000_000 / self.framerate())
    }

    fn framerate(&self) -> u64 {
        match self.properties.get("framerate") {
            Some(PropertyValue::UInt(f)) if *f > 0 => *f,
            Some(PropertyValue::Int(f)) if *f > 0 => *f as u64,
            _ => 30,
        }
    }

    /// A page is the one entry of its library: index 0, named by its URL.
    pub(super) fn check_take(
        &self,
        index: Option<usize>,
        expected_file: Option<&str>,
    ) -> Result<(), PipelineError> {
        if index.is_some_and(|i| i != 0) {
            return Err(err(format!(
                "HTML Input {} is a stinger page: its only entry is 0",
                self.source_block_id
            )));
        }
        match expected_file {
            Some(want) if want != self.configured_url() => Err(PipelineError::Conflict(format!(
                "the stinger page is now '{}', not '{}'",
                self.configured_url(),
                want
            ))),
            _ => Ok(()),
        }
    }

    /// The page as a library entry, for the operator panel.
    fn describe(&self, grid: FrameGrid) -> (StingerClip, Option<String>) {
        let settings = self.settings();
        let plan =
            check_page_url(&self.configured_url()).and_then(|()| settings.plan(grid.frame_ns()));
        let cut = plan.as_ref().ok().and_then(|p| p.cut_point_ms);
        let clip = StingerClip {
            index: 0,
            file: self.configured_url(),
            settings: StingerClipSettings {
                layout: strom_types::stinger::StingerLayout::Classic,
                cut_point_ms: cut,
                beneath: if settings.mix_ms > 0 {
                    StingerBeneath::Mix
                } else {
                    StingerBeneath::Cut
                },
                mix_ms: plan.as_ref().map(|p| p.mix_ms).unwrap_or(settings.mix_ms),
                premultiplied: true,
                invert_matte: false,
            },
            info: None,
            variant: Some(StingerVariant::Classic),
            downgraded_from: None,
            cut_point_ms: cut,
        };
        (clip, plan.err())
    }
}

impl AppState {
    /// The stinger state of a mixer whose stinger input is a page.
    pub(super) async fn page_stinger_state(
        &self,
        flow_id: &FlowId,
        mixer: &str,
        page: &PageContext,
        last_take: Option<StingerTakeReport>,
        running: bool,
    ) -> StingerState {
        let (grid, playing) = {
            let pipelines = self.inner.pipelines.read().await;
            let manager = pipelines.get(flow_id);
            let grid = manager.and_then(|m| m.mixer_frame_grid(mixer));
            let playing = manager
                .and_then(|m| m.find_gst_element(&format!("{}:cefsrc", page.source_block_id)))
                .is_some_and(|c| c.current_state() == gst::State::Playing);
            (grid, playing)
        };
        let (clip, invalid) = page.describe(grid.unwrap_or(NOMINAL_GRID));
        let problem = invalid.or_else(|| {
            (!playing).then(|| format!("HTML Input {} is not running", page.source_block_id))
        });
        StingerState {
            source_block_id: Some(page.source_block_id.clone()),
            source_kind: Some(StingerSourceKind::Page),
            problem,
            matte_supported: page.matte_supported,
            preroll_ms: page.preroll_ms,
            clips: vec![clip],
            cued_index: Some(0),
            ready: playing,
            last_cue_ms: None,
            running,
            last_take,
        }
    }

    /// A page is always ready: cueing it does nothing.
    pub(super) fn page_stinger_cue(
        &self,
        flow_id: &FlowId,
        block: &str,
        index: usize,
    ) -> Result<u64, PipelineError> {
        if index != 0 {
            return Err(err("a stinger page's only entry is 0"));
        }
        self.inner.events.broadcast(StromEvent::StingerCued {
            flow_id: *flow_id,
            block_id: block.to_string(),
            index,
            ready: true,
            cue_ms: 0,
        });
        Ok(0)
    }

    /// The take, once the mixer is claimed. On failure says whether the
    /// program should cut instead (the page will not play).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn page_stinger_take(
        &self,
        flow_id: &FlowId,
        block: &str,
        page: &PageContext,
        token: u64,
        from: usize,
        to: usize,
        requested: Instant,
    ) -> Result<StingerTakeResponse, (String, bool)> {
        let source = &page.source_block_id;
        let (cefsrc, grid) = {
            let pipelines = self.inner.pipelines.read().await;
            let manager = pipelines
                .get(flow_id)
                .ok_or_else(|| ("the flow is not running".to_string(), false))?;
            let grid = manager.mixer_frame_grid(block).ok_or_else(|| {
                (
                    "the mixer has not negotiated a frame rate".to_string(),
                    false,
                )
            })?;
            let cefsrc = manager
                .find_gst_element(&format!("{}:cefsrc", source))
                .cloned()
                .ok_or_else(|| (format!("HTML Input {} is not running", source), true))?;
            (cefsrc, grid)
        };
        check_page_url(&page.configured_url()).map_err(|e| (e, false))?;
        let settings = page.settings();
        let plan = settings.plan(grid.frame_ns()).map_err(|e| (e, false))?;
        let duration_ns = plan.duration_ms * 1_000_000;

        let fixed_rate = cefsrc.find_property(VARIABLE_RATE_PROPERTY).is_none();
        if !fixed_rate {
            warn!(
                "Stinger page {}: this gstcefsrc emits frames only as the page paints, and the \
                 mixer's stinger input lets a held frame go after two frames: a page that holds \
                 a still frame across the cut can drop off air",
                source
            );
        }
        if html_input::remote_control_enabled(&page.properties) {
            debug!(
                "Stinger page {}: under remote control the page loads its trigger \
                 asynchronously, so a page that paints late is timed from before it started",
                source
            );
        }
        let pad = cefsrc
            .static_pad("src")
            .ok_or_else(|| ("the stinger page has no output".to_string(), true))?;

        // Stage the take and raise the graphic: the page is transparent at
        // rest, so nothing shows until it starts. Its start is not known
        // yet; the times are filled in once it is.
        let staged = take_on_grid(grid, 0, duration_ns, &plan, from, to, None, true);
        let ftb_cancelled = {
            let pipelines = self.inner.pipelines.read().await;
            let manager = pipelines
                .get(flow_id)
                .ok_or_else(|| ("the flow is not running".to_string(), false))?;
            manager.raise_stinger_fill(block, &staged).map_err(|e| {
                manager.abort_stinger(block, &staged);
                (e.to_string(), true)
            })?
        };
        let staged = &staged;
        let abort = |reason: String| async move {
            if let Some(manager) = self.inner.pipelines.read().await.get(flow_id) {
                manager.abort_stinger(block, staged);
            }
            (reason, true)
        };

        let delay = if fixed_rate {
            PAGE_FIRST_FRAME_DELAY_FIXED_RATE
        } else {
            PAGE_FIRST_FRAME_DELAY_PAINTED
        };
        let Some(anchor) = WebAnchor::watch(&pad, delay) else {
            return Err(abort("could not watch the stinger page's output".to_string()).await);
        };
        // A paint-only gstcefsrc sends nothing while the page rests, so
        // there is nothing to wait for.
        if fixed_rate {
            anchor.ready(Duration::from_millis(100)).await;
        }
        if anchor.paints_at_rest() {
            warn!(
                "Stinger page {}: the page paints while at rest, so its first frame after the take \
                 may be a paint of its rest state and the cut may land a frame or two early. A \
                 stinger page should stop drawing until the take",
                source
            );
        }

        // A new fragment is a same-document navigation: the loaded page gets
        // `hashchange` and starts its animation, rather than reloading.
        let shown: Option<String> = cefsrc.property(html_input::URL_PROPERTY);
        let shown = crate::cef_pages::shown_url(
            cefsrc.upcast_ref(),
            shown.unwrap_or_else(|| page.configured_url()),
        );
        let base = shown.split('#').next().unwrap_or_default().to_string();
        crate::cef_pages::load_url(&cefsrc, &format!("{}#{}", base, take_fragment(token)));
        let taken_at = Instant::now();
        let Some(taken_rt) = cefsrc.current_running_time().map(|t| t.nseconds()) else {
            return Err(abort("the flow has no running time".to_string()).await);
        };
        anchor.taken(taken_rt);

        let give_up = taken_at + PAGE_FIRST_FRAME_GRACE + SILENT_PAGE_LIMIT;
        // The page's start: its first frame, or the take plus the delivery
        // delay when it painted nothing promptly.
        let (anchor_ns, prompt) = loop {
            match anchor.start() {
                WebStart::FirstFrame(ns) => break (ns, true),
                WebStart::FromTake(ns) => break (ns, false),
                WebStart::Waiting if Instant::now() < give_up => {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                WebStart::Waiting => break (taken_rt + delay.as_nanos() as u64, false),
            }
        };
        let first = anchor.first_frame_ns();
        drop(anchor);
        let after_take = |ns: u64| ns.saturating_sub(taken_rt) / 1_000_000;
        if prompt {
            debug!(
                "Stinger page {}: first frame {} ms after the take",
                source,
                after_take(anchor_ns)
            );
        } else {
            warn!(
                "Stinger page {}: no frame within {} ms of the take{}, so the cut is timed \
                 from the take and may be a frame out. A stinger page should change \
                 something visible on its first frame",
                source,
                PAGE_FIRST_FRAME_GRACE.as_millis(),
                first
                    .map(|f| format!(" (first came after {} ms)", after_take(f)))
                    .unwrap_or_default()
            );
        }

        // Program the take from the output frame the page goes on air in.
        let page_frame_ns = page.frame_ns(&pad);
        let start = web_first_output_frame(anchor_ns, page_frame_ns, grid);
        let take = take_on_grid(grid, start, duration_ns, &plan, from, to, None, true);
        debug!(
            "Stinger page {}: page started at {} ns, on air from output frame {} ns, cut at {:?} ns",
            source, anchor_ns, take.start, take.cut_at
        );
        // The frame count starts when the take is programmed, after the
        // page's first frames have reached the mixer, so only the frames from
        // then on are expected.
        let counted_from = cefsrc
            .current_running_time()
            .map_or(take.start, |t| t.nseconds().max(take.start));
        let programmed = {
            let pipelines = self.inner.pipelines.read().await;
            match pipelines.get(flow_id) {
                Some(manager) => manager.program_stinger(block, &take),
                None => Err(PipelineError::InvalidFlow("the flow is not running".into())),
            }
        };
        let watch = match programmed {
            Ok((watch, _)) => watch,
            Err(e) => return Err(abort(e.to_string()).await),
        };

        let take_to_air_ms =
            (taken_at - requested).as_secs_f64() * 1000.0 + (start as f64 - taken_rt as f64) / 1e6;
        let frames_expected = page_frames_between(anchor_ns, page_frame_ns, counted_from, take.end);
        Ok(self
            .announce_stinger_take(
                flow_id,
                block,
                Started {
                    token,
                    index: 0,
                    file: base,
                    plan,
                    take,
                    watch,
                    ftb_cancelled,
                    take_to_air_ms,
                    cue_ms: None,
                    frames_expected,
                    player: None,
                },
            )
            .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_page_frames_count_from_when_counting_starts() {
        let f = 33_333_333;
        // Page frames every frame from 1_008 ms; on air from 1_000 ms for a
        // second: 30 frames.
        let first = 1_008_000_000;
        assert_eq!(
            page_frames_between(first, f, 1_000_000_000, 2_000_000_000),
            30
        );
        // Counting started 12 ms after the first frame: it missed that one.
        assert_eq!(
            page_frames_between(first, f, 1_020_000_000, 2_000_000_000),
            29
        );
        // A 60 fps page.
        assert_eq!(
            page_frames_between(first, 16_666_667, 1_033_333_333, 2_033_333_333),
            60
        );
    }
}
