//! Running a stinger take.
//!
//! Resolution and validation live in `gst::stinger`; this is the half that
//! touches the pipeline: claiming the mixer, starting the clip, landing the
//! transition beneath on the right frame, and tearing down afterwards.

use super::AppState;
use crate::blocks::builtin::mediaplayer::{
    MediaPlayerKey, MediaPlayerState, MEDIA_PLAYER_REGISTRY,
};
use crate::gst::pipeline::PipelineError;
use std::sync::Arc;
use strom_types::{FlowId, StromEvent};
use tracing::{debug, error, warn};

/// Holds a mixer's output thread while a stinger's take is applied. Dropping it
/// releases the mixer, so it must outlive the take.
struct CutGuard {
    _release: std::sync::mpsc::SyncSender<()>,
}

/// What the spawned half of a take needs once validation has passed.
struct Take {
    flow: FlowId,
    mixer: String,
    source: String,
    dsk_index: usize,
    token: u64,
    cut_point_ms: u64,
    clip_ms: u64,
    from_input: usize,
    to_input: usize,
    under: String,
    under_ms: u64,
    played_at: std::time::Instant,
    player: Arc<MediaPlayerState>,
}

impl AppState {
    /// How long past the cut point to keep waiting for the clip to reach the
    /// mixer before giving up on anchoring and cutting on wall clock.
    const STINGER_ANCHOR_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
    const STINGER_ANCHOR_POLL: std::time::Duration = std::time::Duration::from_millis(2);
    /// Longest the mixer's output is held while a take is applied. Reaching it
    /// means the cut lands late, not that the mixer stays blocked.
    const STINGER_MAX_HOLD: std::time::Duration = std::time::Duration::from_millis(250);

    /// Trigger a stinger: play a keyed clip over the program while another
    /// transition runs beneath it.
    ///
    /// Everything that can be rejected is rejected before anything moves on
    /// air, so a bad request leaves the program untouched. Once the clip is
    /// rolling, a single task drives the rest: the underlying transition at the
    /// cut point, then teardown and re-arm when the clip ends.
    #[allow(clippy::too_many_arguments)]
    pub async fn trigger_stinger(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        from_input: usize,
        to_input: usize,
        source_block_id: Option<&str>,
    ) -> Result<String, PipelineError> {
        use crate::blocks::builtin::mediaplayer::{MediaPlayerKey, MEDIA_PLAYER_REGISTRY};
        use crate::gst::stinger::{self, StingerError};
        use crate::gst::transitions::TransitionType;

        // --- Validation. Nothing below this block touches the pipeline. ---
        let binding = {
            let flows = self.inner.flows.read().await;
            let flow = flows
                .get(flow_id)
                .ok_or_else(|| PipelineError::InvalidFlow(format!("No flow {flow_id}")))?;
            stinger::resolve_binding(
                &flow.blocks,
                &flow.links,
                block_instance_id,
                source_block_id,
            )?
        };

        // Cut point and the transition beneath are properties of the clip, not
        // of the take: the person who cut the clip knows where it covers.
        let under_name = binding.under_transition.clone();
        let under_duration_ms = binding.under_duration_ms;
        let under_type: TransitionType = under_name
            .parse()
            .map_err(|_| StingerError::UnknownUnderTransition(under_name.clone()))?;
        if matches!(under_type, TransitionType::Stinger) {
            return Err(StingerError::StingerBeneathStinger.into());
        }

        let key = MediaPlayerKey {
            flow_id: *flow_id,
            block_id: binding.source_block_id.clone(),
        };
        let player = MEDIA_PLAYER_REGISTRY
            .get(&key)
            .ok_or_else(|| StingerError::UnknownSource(binding.source_block_id.clone()))?;

        // Clip length drives the stinger; duration_ms is the transition beneath.
        //
        // No readable duration means the clip is missing or undecodable. Degrade
        // rather than reject: a broken clip must not leave the program
        // mid-transition. The mixer is not claimed yet, so nothing to release.
        let Some(clip_ms) = player
            .duration()
            .map(|ns| ns / 1_000_000)
            .filter(|ms| *ms > 0)
        else {
            return self
                .cut_without_the_clip(
                    flow_id,
                    block_instance_id,
                    &binding.source_block_id,
                    from_input,
                    to_input,
                    &under_name,
                    under_duration_ms,
                    "clip has no readable duration (missing or undecodable)",
                )
                .await;
        };

        // A cut point is optional; without one, cut where a covering clip is
        // most likely to be opaque.
        let cut_point = binding.cut_point_ms.unwrap_or(clip_ms / 2);
        let (under_ms, clamped_from) =
            stinger::fit_under_transition(cut_point, under_duration_ms, clip_ms)?;
        if let Some(requested) = clamped_from {
            warn!(
                "Stinger on {}: transition beneath shortened from {} ms to {} ms so it \
                 completes before the {} ms clip ends",
                block_instance_id, requested, under_ms, clip_ms
            );
        }

        let was_armed = player.is_stinger_armed();
        if !was_armed {
            warn!(
                "Stinger source {} was not armed; its first frame will be late",
                binding.source_block_id
            );
        }

        // --- Claim the mixer. A stinger owns the program bus until it ends. ---
        let claim = (*flow_id, block_instance_id.to_string());
        let token = self
            .inner
            .next_stinger_token
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        {
            let mut in_flight = self.inner.stingers_in_flight.lock();
            if in_flight.contains_key(&claim) {
                return Err(StingerError::AlreadyRunning(block_instance_id.to_string()).into());
            }
            in_flight.insert(claim.clone(), token);
        }

        // From here on, every exit path must release the claim.
        let release = {
            let inner = self.inner.clone();
            let claim = claim.clone();
            move || {
                let mut in_flight = inner.stingers_in_flight.lock();
                if in_flight.get(&claim) == Some(&token) {
                    in_flight.remove(&claim);
                }
            }
        };

        if let Err(e) = self
            .set_dsk_enabled(flow_id, block_instance_id, binding.dsk_index, true)
            .await
        {
            release();
            return Err(e);
        }

        // A clip that will not play costs the branding, not the cut: run the
        // transition beneath on its own so the program still changes.
        if let Err(e) = player.play() {
            let _ = self
                .set_dsk_enabled(flow_id, block_instance_id, binding.dsk_index, false)
                .await;
            release();
            return self
                .cut_without_the_clip(
                    flow_id,
                    block_instance_id,
                    &binding.source_block_id,
                    from_input,
                    to_input,
                    &under_name,
                    under_ms,
                    &e.to_string(),
                )
                .await;
        }

        self.inner.events.broadcast(StromEvent::StingerStarted {
            flow_id: *flow_id,
            block_instance_id: block_instance_id.to_string(),
            source_block_id: binding.source_block_id.clone(),
            clip_ms,
            cut_point_ms: cut_point,
            under_transition: under_name.clone(),
            under_duration_ms: under_ms,
            under_duration_clamped_from: clamped_from,
            armed: was_armed,
        });

        let state = self.clone();
        let take = Take {
            flow: *flow_id,
            mixer: block_instance_id.to_string(),
            source: binding.source_block_id.clone(),
            dsk_index: binding.dsk_index,
            token,
            cut_point_ms: cut_point,
            clip_ms,
            from_input,
            to_input,
            under: under_name,
            under_ms,
            played_at: std::time::Instant::now(),
            player: player.clone(),
        };
        tokio::spawn(async move { state.run_take(take).await });

        Ok("stinger".to_string())
    }

    /// Run the transition beneath on its own, because the clip will not play.
    ///
    /// A broken file costs the branding, not the cut: the program still changes
    /// rather than being left mid-transition.
    #[allow(clippy::too_many_arguments)]
    async fn cut_without_the_clip(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        source_block_id: &str,
        from_input: usize,
        to_input: usize,
        under: &str,
        under_ms: u64,
        reason: &str,
    ) -> Result<String, PipelineError> {
        error!(
            "Stinger clip on {}: {} — running the transition beneath on its own",
            source_block_id, reason
        );
        self.inner.events.broadcast(StromEvent::StingerFailed {
            flow_id: *flow_id,
            block_instance_id: block_instance_id.to_string(),
            source_block_id: source_block_id.to_string(),
            reason: reason.to_string(),
            still_running: false,
        });
        self.trigger_transition(
            flow_id,
            block_instance_id,
            from_input,
            to_input,
            under,
            under_ms,
        )
        .await
    }

    /// Land the transition beneath at the cut point, then tear the stinger down
    /// when the clip ends.
    async fn run_take(&self, take: Take) {
        let Take {
            flow,
            mixer,
            source,
            dsk_index,
            token,
            cut_point_ms: cut_point,
            clip_ms,
            from_input,
            to_input,
            under: under_name,
            under_ms,
            played_at,
            player: clip_player,
        } = take;
        let state = self;
        let claim = (flow, mixer.clone());
        let still_ours = || state.inner.stingers_in_flight.lock().get(&claim) == Some(&token);

        // Hold the mixer on the frame before the one carrying the cut
        // point, so the take is applied before that frame is composited.
        // Dropping the guard releases it.
        let guard = state
            .hold_mixer_before_cut(&flow, &mixer, &clip_player, cut_point, played_at)
            .await;
        if guard.is_none() {
            // Nothing to anchor to: fall back to wall clock from the take.
            let elapsed = played_at.elapsed();
            let remaining = std::time::Duration::from_millis(cut_point).saturating_sub(elapsed);
            tokio::time::sleep(remaining).await;
        }

        // The flow may have been stopped and restarted while this waited.
        // Acting now would drive a pipeline this take knows nothing about.
        if !still_ours() {
            debug!(
                "Stinger on {} was superseded; leaving the mixer alone",
                mixer
            );
            return;
        }

        let beneath = state
            .trigger_transition(&flow, &mixer, from_input, to_input, &under_name, under_ms)
            .await;
        // Release the mixer only once the take has been applied.
        drop(guard);
        if let Err(e) = beneath {
            // The clip is already on air, so this is the worst place to
            // fail quietly: the graphic plays but the program never
            // changes. Report it rather than leaving it in the log.
            error!("Stinger on {}: transition beneath failed: {}", mixer, e);
            state.inner.events.broadcast(StromEvent::StingerFailed {
                flow_id: flow,
                block_instance_id: mixer.clone(),
                source_block_id: source.clone(),
                reason: format!("the transition beneath did not run: {e}"),
                still_running: true,
            });
        }

        // Hold the keyed pad up for the rest of the clip.
        let remaining = clip_ms.saturating_sub(cut_point);
        tokio::time::sleep(std::time::Duration::from_millis(remaining)).await;

        if !still_ours() {
            debug!(
                "Stinger on {} was superseded; leaving the mixer alone",
                mixer
            );
            return;
        }

        if let Err(e) = state.set_dsk_enabled(&flow, &mixer, dsk_index, false).await {
            error!(
                "Stinger on {}: could not hide the keyed input: {}",
                mixer, e
            );
        }

        // Re-arm so the next fire is fast again, then release the mixer.
        if let Some(player) = MEDIA_PLAYER_REGISTRY.get(&MediaPlayerKey {
            flow_id: flow,
            block_id: source.clone(),
        }) {
            if let Err(e) = player.arm_stinger() {
                warn!("Stinger source {} could not be re-armed: {}", source, e);
            }
        }
        {
            let mut in_flight = state.inner.stingers_in_flight.lock();
            if in_flight.get(&claim) == Some(&token) {
                in_flight.remove(&claim);
            }
        }
        state.inner.events.broadcast(StromEvent::StingerCompleted {
            flow_id: flow,
            block_instance_id: mixer.clone(),
            source_block_id: source.clone(),
        });
        debug!("Stinger on {} complete", mixer);
    }

    /// Hold the mixer's output thread just before the frame that carries the
    /// cut point, returning a guard whose drop releases it.
    ///
    /// A cut point is a position in the clip, but a take is applied by setting
    /// pad properties from another thread, which races the aggregator: whether
    /// the change reaches the frame it was meant for depends on how the mixer
    /// happened to be scheduled. Waiting on wall clock or on the mixer's
    /// reported position both land a frame out, and by different amounts on
    /// different layouts, because a mixer whose inputs all have data aggregates
    /// as soon as it can while one waiting on a live source runs to its
    /// deadline.
    ///
    /// Anchoring removes the race. The clip's `stream_offset_ns` says where
    /// clip time sits in the mixer's timeline, which gives the output frame
    /// whose interval contains the cut point. Blocking the mixer's src pad on
    /// the frame before it means the take is always applied first.
    ///
    /// Returns `None` when there is nothing to anchor to, leaving the caller to
    /// fall back to wall clock.
    async fn hold_mixer_before_cut(
        &self,
        flow_id: &FlowId,
        block_instance_id: &str,
        player: &Arc<crate::blocks::builtin::mediaplayer::MediaPlayerState>,
        cut_point_ms: u64,
        played_at: std::time::Instant,
    ) -> Option<CutGuard> {
        let give_up =
            played_at + std::time::Duration::from_millis(cut_point_ms) + Self::STINGER_ANCHOR_GRACE;

        // The offset is only known once the bridge has delivered a buffer.
        let offset = loop {
            if let Some(o) = player.stream_offset_ns() {
                break o;
            }
            if std::time::Instant::now() > give_up {
                warn!(
                    "Stinger on {}: clip never reported a stream offset, cutting on \
                     wall clock",
                    block_instance_id
                );
                return None;
            }
            tokio::time::sleep(Self::STINGER_ANCHOR_POLL).await;
        };

        let (frame_ns, pad) = {
            let pipelines = self.inner.pipelines.read().await;
            let manager = pipelines.get(flow_id)?;
            (
                manager.mixer_frame_duration_ns(block_instance_id)?,
                manager.mixer_src_pad(block_instance_id)?,
            )
        };

        // Floor, not round: the frame that carries the cut point is the one
        // whose interval contains it, and rounding up is a frame late.
        let cut_at = offset.saturating_add((cut_point_ms as i64).saturating_mul(1_000_000));
        if cut_at < 0 {
            return None;
        }
        let cut_frame_pts = (cut_at as u64 / frame_ns) * frame_ns;
        // Half a frame back from the preceding frame, so a pts landing slightly
        // off the grid still matches it rather than the cut frame itself.
        let hold_from_pts = cut_frame_pts
            .saturating_sub(frame_ns)
            .saturating_sub(frame_ns / 2);

        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel::<u64>();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(0);
        let reached_tx = std::sync::Mutex::new(Some(reached_tx));
        let release_rx = std::sync::Mutex::new(release_rx);
        let fired = std::sync::atomic::AtomicBool::new(false);
        let held_for = Self::STINGER_MAX_HOLD;
        let probe_mixer = block_instance_id.to_string();

        // A per-buffer probe, which this codebase treats as performance
        // critical. Every buffer before the target costs one timestamp compare;
        // the lock and the channel run once, on the buffer it holds, and the
        // probe removes itself there. It is installed for a single take.
        use gstreamer::prelude::PadExtManual;
        let probe = pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
            let Some(pts) = info.buffer().and_then(|b| b.pts()) else {
                return gstreamer::PadProbeReturn::Ok;
            };
            if pts.nseconds() < hold_from_pts
                || fired.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                return gstreamer::PadProbeReturn::Ok;
            }
            if let Some(tx) = reached_tx.lock().ok().and_then(|mut g| g.take()) {
                let _ = tx.send(pts.nseconds());
            }
            let waited = std::time::Instant::now();
            let outcome = release_rx.lock().ok().map(|rx| rx.recv_timeout(held_for));
            if matches!(
                outcome,
                Some(Err(std::sync::mpsc::RecvTimeoutError::Timeout))
            ) {
                warn!(
                    "Stinger on {}: took longer than {} ms to apply, so the cut may \
                     land a frame late",
                    probe_mixer,
                    held_for.as_millis()
                );
            }
            debug!(
                "Stinger on {}: held the mixer for {:?}",
                probe_mixer,
                waited.elapsed()
            );
            gstreamer::PadProbeReturn::Remove
        })?;

        match tokio::time::timeout(
            give_up.saturating_duration_since(std::time::Instant::now()),
            reached_rx,
        )
        .await
        {
            Ok(Ok(_)) => Some(CutGuard {
                _release: release_tx,
            }),
            _ => {
                // Never reached: drop the sender so the probe does not hold the
                // mixer when it eventually fires, and cut on wall clock.
                pad.remove_probe(probe);
                warn!(
                    "Stinger on {}: mixer never reached the cut frame, cutting on \
                     wall clock",
                    block_instance_id
                );
                None
            }
        }
    }
}
