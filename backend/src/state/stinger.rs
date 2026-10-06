//! Stinger takes on a vision mixer: cueing clips, taking them, finishing and
//! reporting each take, and the state the operator panel shows.
//!
//! The clips come from a Media Player in stinger mode wired to the mixer's
//! stinger input; its playlist is the stinger library and its block carries
//! each clip's settings. A take is planned from the clip's settings and
//! analysis (`crate::stinger`), programmed into the mixer in full for the
//! frame its clip lands on (`effects::stinger`), and then the clip is let go
//! (`MediaPlayerState::play_at`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use strom_types::stinger::{
    StingerClip, StingerClipSettings, StingerExamplesResponse, StingerState, StingerTakeReport,
    StingerTakeResponse, STINGER_CLIPS_PROPERTY, STINGER_INPUT_PAD, STINGER_MODE_PROPERTY,
};
use strom_types::{FlowId, PropertyValue, StromEvent};
use tracing::{debug, error, info, warn};

use super::AppState;
use crate::blocks::builtin::mediaplayer::{
    normalize_uri, MediaPlayerKey, MediaPlayerState, MEDIA_PLAYER_REGISTRY,
};
use crate::gst::pipeline::effects::stinger::{StingerTake, StingerWatch};
use crate::gst::pipeline::PipelineError;
use crate::stinger::{analysis, examples, plan_clip};

/// How long after the clip's end a take waits for the mixer to get past it
/// before giving up on a stalled mixer.
const FINISH_GRACE: Duration = Duration::from_secs(5);

/// Per-mixer stinger bookkeeping, kept outside the flow definition.
#[derive(Default)]
struct MixerStinger {
    /// The token of the take on air.
    running: Option<u64>,
    last_take: Option<StingerTakeReport>,
    last_cue_ms: Option<u64>,
}

static MIXERS: LazyLock<Mutex<HashMap<(FlowId, String), MixerStinger>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Clips being analysed now, so a listing does not start a second run.
static ANALYSING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn with_mixer<T>(flow: FlowId, block: &str, f: impl FnOnce(&mut MixerStinger) -> T) -> T {
    let mut map = MIXERS.lock().unwrap_or_else(|p| p.into_inner());
    f(map.entry((flow, block.to_string())).or_default())
}

/// Whether a stinger is on air on this mixer. Other takes wait for it.
pub fn is_running(flow: &FlowId, block: &str) -> bool {
    MIXERS
        .lock()
        .ok()
        .and_then(|m| {
            m.get(&(*flow, block.to_string()))
                .map(|s| s.running.is_some())
        })
        .unwrap_or(false)
}

/// Drop a stopped flow's takes: a take still waiting for its clip's end sees
/// its token gone and leaves the next pipeline alone.
pub fn forget_flow(flow: &FlowId) {
    if let Ok(mut map) = MIXERS.lock() {
        for ((f, _), s) in map.iter_mut() {
            if f == flow {
                s.running = None;
            }
        }
    }
}

fn still_ours(flow: FlowId, block: &str, token: u64) -> bool {
    MIXERS
        .lock()
        .ok()
        .and_then(|m| m.get(&(flow, block.to_string())).and_then(|s| s.running))
        == Some(token)
}

fn release(flow: FlowId, block: &str, token: u64) {
    with_mixer(flow, block, |s| {
        if s.running == Some(token) {
            s.running = None;
        }
    });
}

fn err(reason: impl Into<String>) -> PipelineError {
    PipelineError::TransitionError(reason.into())
}

/// What a stinger operation needs to know about a mixer and its source.
struct Context {
    source_block_id: String,
    player: Arc<MediaPlayerState>,
    settings: HashMap<String, StingerClipSettings>,
    preroll_ms: u64,
    pgm_aspect: f64,
    matte_supported: bool,
}

impl Context {
    fn uri(&self, file: &str) -> String {
        normalize_uri(file, &self.player.media_path)
    }

    fn settings_for(&self, file: &str) -> StingerClipSettings {
        self.settings.get(file).cloned().unwrap_or_default()
    }
}

impl AppState {
    /// Find the mixer's stinger source and settings. Errors say what is
    /// missing in terms an operator can fix.
    async fn stinger_context(&self, flow_id: &FlowId, block: &str) -> Result<Context, String> {
        let (source_block_id, settings, preroll_ms, pgm_aspect) = {
            let flows = self.inner.flows.read().await;
            let flow = flows.get(flow_id).ok_or("no such flow")?;
            let mixer = flow
                .blocks
                .iter()
                .find(|b| b.id == block)
                .ok_or("no such vision mixer")?;
            let props = &mixer.properties;
            if !matches!(
                props.get(strom_types::stinger::ENABLE_STINGER_PROPERTY),
                Some(PropertyValue::Bool(true))
            ) {
                return Err("the vision mixer has no stinger input (turn on Stinger Input)".into());
            }
            let preroll_ms = crate::blocks::builtin::vision_mixer::properties::parse_u64(
                props,
                strom_types::stinger::STINGER_PREROLL_PROPERTY,
                strom_types::stinger::DEFAULT_STINGER_PREROLL_MS,
            )
            .clamp(
                strom_types::stinger::MIN_STINGER_PREROLL_MS,
                strom_types::stinger::MAX_STINGER_PREROLL_MS,
            );
            let (w, h) = crate::blocks::builtin::vision_mixer::properties::parse_resolution(
                props,
                "pgm_resolution",
                strom_types::vision_mixer::DEFAULT_PGM_RESOLUTION,
            );
            let pgm_aspect = if h > 0 {
                w as f64 / h as f64
            } else {
                16.0 / 9.0
            };
            let to = format!("{}:{}", block, STINGER_INPUT_PAD);
            let source = flow
                .links
                .iter()
                .find(|l| l.to == to)
                .and_then(|l| l.from.strip_suffix(":video_out"))
                .ok_or("nothing is wired to the stinger input")?;
            let source_block = flow
                .blocks
                .iter()
                .find(|b| b.id == source)
                .ok_or("the stinger input is not fed by a Media Player")?;
            if source_block.block_definition_id != "builtin.media_player" {
                return Err("the stinger input is not fed by a Media Player".into());
            }
            if !matches!(
                source_block.properties.get(STINGER_MODE_PROPERTY),
                Some(PropertyValue::Bool(true))
            ) {
                return Err(format!(
                    "Media Player {} is not a stinger clip source (turn on Stinger Clip Source)",
                    source
                ));
            }
            let settings = match source_block.properties.get(STINGER_CLIPS_PROPERTY) {
                Some(PropertyValue::String(json)) => {
                    strom_types::stinger::parse_clip_settings(json)
                }
                _ => HashMap::new(),
            };
            (source.to_string(), settings, preroll_ms, pgm_aspect)
        };
        let player = MEDIA_PLAYER_REGISTRY
            .get(&MediaPlayerKey {
                flow_id: *flow_id,
                block_id: source_block_id.clone(),
            })
            .ok_or("the flow is not running")?;
        let matte_supported = self
            .inner
            .pipelines
            .read()
            .await
            .get(flow_id)
            .and_then(|m| m.stinger_pads(block))
            .is_some_and(|p| p.matte.is_some());
        Ok(Context {
            source_block_id,
            player,
            settings,
            preroll_ms,
            pgm_aspect,
            matte_supported,
        })
    }

    /// Analyse a clip off the request path, once at a time per clip.
    fn analyse_in_background(uri: String, pgm_aspect: f64) {
        if analysis::cached(&uri, pgm_aspect).is_some() {
            return;
        }
        {
            let mut busy = ANALYSING.lock().unwrap_or_else(|p| p.into_inner());
            if !busy.insert(uri.clone()) {
                return;
            }
        }
        tokio::task::spawn_blocking(move || {
            if let Err(e) = analysis::analyze_cached(&uri, pgm_aspect) {
                warn!("Stinger clip {} could not be analysed: {}", uri, e);
            }
            ANALYSING
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&uri);
        });
    }

    fn describe_clip(ctx: &Context, index: usize, file: &str) -> StingerClip {
        let settings = ctx.settings_for(file);
        let uri = ctx.uri(file);
        let info = analysis::cached(&uri, ctx.pgm_aspect);
        if info.is_none() {
            Self::analyse_in_background(uri, ctx.pgm_aspect);
        }
        let plan = plan_clip(&settings, info.as_ref(), ctx.matte_supported, None).ok();
        StingerClip {
            index,
            file: file.to_string(),
            settings,
            info,
            variant: plan.as_ref().map(|p| p.variant),
            downgraded_from: plan.as_ref().and_then(|p| p.downgraded_from),
            cut_point_ms: plan.and_then(|p| p.cut_point_ms),
        }
    }

    /// The stinger state of a vision mixer, for the operator panel. Starts
    /// analysing clips it has not seen yet.
    pub async fn stinger_state(
        &self,
        flow_id: &FlowId,
        block: &str,
    ) -> Result<StingerState, PipelineError> {
        let (last_take, last_cue_ms, running) = with_mixer(*flow_id, block, |s| {
            (s.last_take.clone(), s.last_cue_ms, s.running)
        });
        let ctx = match self.stinger_context(flow_id, block).await {
            Ok(ctx) => ctx,
            Err(problem) => {
                return Ok(StingerState {
                    source_block_id: None,
                    problem: Some(problem),
                    matte_supported: false,
                    preroll_ms: strom_types::stinger::DEFAULT_STINGER_PREROLL_MS,
                    clips: vec![],
                    cued_index: None,
                    ready: false,
                    last_cue_ms,
                    running: running.is_some(),
                    last_take,
                })
            }
        };
        let files = ctx.player.playlist_files();
        let clips = files
            .iter()
            .enumerate()
            .map(|(i, f)| Self::describe_clip(&ctx, i, f))
            .collect();
        let cued = (!files.is_empty()).then(|| ctx.player.current_index());
        Ok(StingerState {
            source_block_id: Some(ctx.source_block_id.clone()),
            problem: files
                .is_empty()
                .then(|| "the stinger source's playlist is empty".to_string()),
            matte_supported: ctx.matte_supported,
            preroll_ms: ctx.preroll_ms,
            clips,
            cued_index: cued,
            ready: cued.is_some_and(|i| ctx.player.is_parked_on(i)),
            last_cue_ms,
            running: running.is_some(),
            last_take,
        })
    }

    /// Load a clip and park it on its first frame. Returns how long it took.
    pub async fn stinger_cue(
        &self,
        flow_id: &FlowId,
        block: &str,
        index: usize,
    ) -> Result<u64, PipelineError> {
        if is_running(flow_id, block) {
            return Err(err("a stinger is on air; cue when it has finished"));
        }
        let ctx = self.stinger_context(flow_id, block).await.map_err(err)?;
        if let Some(file) = ctx.player.playlist_files().get(index) {
            Self::analyse_in_background(ctx.uri(file), ctx.pgm_aspect);
        }
        let player = Arc::clone(&ctx.player);
        let result = tokio::task::spawn_blocking(move || player.cue(index))
            .await
            .map_err(|e| err(e.to_string()))?;
        let (ready, cue_ms) = match &result {
            Ok(took) => (true, took.as_millis() as u64),
            Err(_) => (false, 0),
        };
        if ready {
            with_mixer(*flow_id, block, |s| s.last_cue_ms = Some(cue_ms));
        }
        self.inner.events.broadcast(StromEvent::StingerCued {
            flow_id: *flow_id,
            block_id: block.to_string(),
            index,
            ready,
            cue_ms,
        });
        result.map(|_| cue_ms).map_err(err)
    }

    /// Store a clip's settings on the stinger source block.
    pub async fn stinger_set_clip_settings(
        &self,
        flow_id: &FlowId,
        block: &str,
        index: usize,
        settings: StingerClipSettings,
    ) -> Result<StingerClip, PipelineError> {
        let ctx = self.stinger_context(flow_id, block).await.map_err(err)?;
        let file = ctx
            .player
            .playlist_files()
            .get(index)
            .cloned()
            .ok_or_else(|| err(format!("no clip {} in the stinger playlist", index)))?;
        {
            let mut flows = self.inner.flows.write().await;
            let source = flows
                .get_mut(flow_id)
                .and_then(|f| f.blocks.iter_mut().find(|b| b.id == ctx.source_block_id))
                .ok_or_else(|| err("the stinger source block is gone"))?;
            let mut map = match source.properties.get(STINGER_CLIPS_PROPERTY) {
                Some(PropertyValue::String(json)) => {
                    strom_types::stinger::parse_clip_settings(json)
                }
                _ => HashMap::new(),
            };
            if settings == StingerClipSettings::default() {
                map.remove(&file);
            } else {
                map.insert(file.clone(), settings);
            }
            let json = serde_json::to_string(&map).map_err(|e| err(e.to_string()))?;
            source.properties.insert(
                STINGER_CLIPS_PROPERTY.to_string(),
                PropertyValue::String(json),
            );
        }
        self.mark_flow_dirty(*flow_id).await;
        let ctx = self.stinger_context(flow_id, block).await.map_err(err)?;
        Ok(Self::describe_clip(&ctx, index, &file))
    }

    /// Render the example clips into the media directory, and add them to the
    /// stinger source's playlist.
    pub async fn stinger_write_examples(
        &self,
        flow_id: &FlowId,
        block: &str,
    ) -> Result<StingerExamplesResponse, PipelineError> {
        let ctx = self.stinger_context(flow_id, block).await.map_err(err)?;
        let media_dir = ctx.player.media_path.clone();
        let files = tokio::task::spawn_blocking(move || examples::write_examples(&media_dir))
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(err)?;
        let mut playlist = ctx.player.playlist_files();
        for f in &files {
            if !playlist.contains(f) {
                playlist.push(f.clone());
            }
        }
        {
            let mut flows = self.inner.flows.write().await;
            if let Some(source) = flows
                .get_mut(flow_id)
                .and_then(|f| f.blocks.iter_mut().find(|b| b.id == ctx.source_block_id))
            {
                source.properties.insert(
                    "playlist".to_string(),
                    PropertyValue::String(
                        serde_json::to_string(&playlist).unwrap_or_else(|_| "[]".into()),
                    ),
                );
            }
        }
        self.mark_flow_dirty(*flow_id).await;
        let was_empty = ctx.player.playlist_len() == 0;
        ctx.player.set_playlist(playlist);
        for f in &files {
            Self::analyse_in_background(ctx.uri(f), ctx.pgm_aspect);
        }
        if was_empty {
            let _ = self.stinger_cue(flow_id, block, 0).await;
        }
        Ok(StingerExamplesResponse {
            files,
            added_to_playlist: true,
        })
    }

    /// Take a stinger from PGM to PVW, playing clip `index` or the cued one.
    pub async fn stinger_take(
        &self,
        flow_id: &FlowId,
        block: &str,
        index: Option<usize>,
    ) -> Result<StingerTakeResponse, PipelineError> {
        let requested = Instant::now();
        let ctx = self.stinger_context(flow_id, block).await.map_err(err)?;
        let state = crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(block)
            .ok_or_else(|| err("the vision mixer is not running"))?;
        let (Some(from), Some(to)) = (state.pgm_input(), state.pvw_input()) else {
            return Err(err(
                "a stinger takes one input to another; PGM or PVW is a PiP",
            ));
        };
        if from == to {
            return Err(err("PGM and PVW are the same input"));
        }
        let files = ctx.player.playlist_files();
        let index = index.unwrap_or_else(|| ctx.player.current_index());
        let file = files
            .get(index)
            .cloned()
            .ok_or_else(|| err(format!("no clip {} in the stinger playlist", index)))?;

        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let claimed = with_mixer(*flow_id, block, |s| {
            if s.running.is_some() {
                false
            } else {
                s.running = Some(token);
                true
            }
        });
        if !claimed {
            return Err(err("a stinger is already on air on this mixer"));
        }

        match self
            .stinger_take_claimed(
                flow_id, block, &ctx, token, index, &file, from, to, requested,
            )
            .await
        {
            Ok(response) => Ok(response),
            Err((reason, cut_instead)) => {
                release(*flow_id, block, token);
                // A clip that will not play costs the graphic, not the
                // change: the program still cuts, once the claim is gone.
                let (reason, program_changed) = if cut_instead {
                    let cut = self
                        .trigger_transition(flow_id, block, from, to, "cut", 0)
                        .await;
                    (format!("{reason}, cut instead"), cut.is_ok())
                } else {
                    (reason, false)
                };
                error!("Stinger on {}: {}", block, reason);
                self.inner.events.broadcast(StromEvent::StingerFailed {
                    flow_id: *flow_id,
                    block_id: block.to_string(),
                    reason: reason.clone(),
                    program_changed,
                });
                Err(err(reason))
            }
        }
    }

    /// The take, once the mixer is claimed. On failure says whether the
    /// program should cut instead (the clip would not play).
    #[allow(clippy::too_many_arguments)]
    async fn stinger_take_claimed(
        &self,
        flow_id: &FlowId,
        block: &str,
        ctx: &Context,
        token: u64,
        index: usize,
        file: &str,
        from: usize,
        to: usize,
        requested: Instant,
    ) -> Result<StingerTakeResponse, (String, bool)> {
        // A clip that is not parked is cued first; that is what the panel's
        // cue does ahead of time.
        let cue_ms = if ctx.player.is_parked_on(index) {
            None
        } else {
            let player = Arc::clone(&ctx.player);
            let cued = tokio::task::spawn_blocking(move || player.cue(index))
                .await
                .map_err(|e| (e.to_string(), false))?;
            match cued {
                Ok(took) => Some(took.as_millis() as u64),
                Err(e) => {
                    return Err((format!("clip {} could not be cued ({})", index, e), true));
                }
            }
        };

        // The plan needs the clip's layout and length: a track-matte clip
        // planned blind would show its matte. A cue starts the analysis, so
        // this waits only for a clip taken straight after it was added.
        let uri = ctx.uri(file);
        let info = match analysis::cached(&uri, ctx.pgm_aspect) {
            Some(info) => Some(info),
            None => {
                let aspect = ctx.pgm_aspect;
                tokio::task::spawn_blocking(move || analysis::analyze_cached(&uri, aspect))
                    .await
                    .ok()
                    .and_then(|r| {
                        r.map_err(|e| warn!("Stinger clip could not be analysed: {}", e))
                            .ok()
                    })
            }
        };
        let settings = ctx.settings_for(file);
        let plan = plan_clip(
            &settings,
            info.as_ref(),
            ctx.matte_supported,
            ctx.player.duration().map(|ns| ns / 1_000_000),
        )
        .map_err(|e| (e, false))?;

        // Program the mixer for the frame the clip lands on.
        let (take, watch, now, now_at, ftb_cancelled) = {
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
            let frame_ns = grid.frame_ns();
            let now = manager
                .running_time_ns()
                .ok_or_else(|| ("the flow has no running time".to_string(), false))?;
            let now_at = Instant::now();
            let start = grid.at_or_after(now + ctx.preroll_ms * 1_000_000);
            let clip_ns = info
                .as_ref()
                .filter(|i| i.framerate_num > 0)
                .map(|i| {
                    i.frames as u64 * 1_000_000_000 * i.framerate_den as u64
                        / i.framerate_num as u64
                })
                .unwrap_or(plan.duration_ms * 1_000_000);
            let end = grid.at_or_after(start + clip_ns);
            let cut_at = plan.cut_point_ms.map(|c| {
                grid.at_or_after(start + c * 1_000_000)
                    .min(end.saturating_sub(frame_ns))
            });
            let take = StingerTake {
                frame_ns,
                from_input: from,
                to_input: to,
                start,
                end,
                cut_at,
                mix_ns: plan.mix_ms * 1_000_000,
                plan: plan.clone(),
                clip_size: info.as_ref().map(|i| (i.width, i.height)),
            };
            let (watch, ftb_cancelled) = manager
                .program_stinger(block, &take)
                .map_err(|e| (e.to_string(), false))?;
            (take, watch, now, now_at, ftb_cancelled)
        };

        // Let the clip go. Its frames are stamped half an output frame into
        // the frame they belong to: the mixer shows the newest buffer that
        // starts before an output frame ends, and a container that rounds
        // timestamps to the millisecond (Matroska) stamps clip frame k+1 a
        // hair before output frame k ends. Half a frame in, rounding cannot
        // move a clip frame into its neighbour's output frame.
        let player = Arc::clone(&ctx.player);
        let start_at = (take.start + take.frame_ns / 2) as i64;
        let played = tokio::task::spawn_blocking(move || player.play_at(start_at))
            .await
            .map_err(|e| (e.to_string(), false))?;
        if let Err(e) = played {
            drop(watch);
            if let Some(manager) = self.inner.pipelines.read().await.get(flow_id) {
                manager.abort_stinger(block, &take);
            }
            return Err((format!("clip {} could not play ({})", index, e), true));
        }

        let take_to_air_ms =
            (now_at - requested).as_secs_f64() * 1000.0 + (take.start - now) as f64 / 1e6;
        self.after_vision_mixer_take(
            flow_id,
            block,
            from,
            to,
            "stinger",
            plan.duration_ms,
            ftb_cancelled,
            Some(from),
            Some(to),
        )
        .await;
        self.inner.events.broadcast(StromEvent::StingerStarted {
            flow_id: *flow_id,
            block_id: block.to_string(),
            index,
            file: file.to_string(),
            variant: plan.variant,
            downgraded_from: plan.downgraded_from,
            from_input: from,
            to_input: to,
            take_to_air_ms,
            duration_ms: plan.duration_ms,
        });

        let frames_expected = info
            .as_ref()
            .map(|i| i.frames)
            .unwrap_or_else(|| (plan.duration_ms * 30 / 1000) as u32);
        let report = StingerTakeReport {
            index,
            file: file.to_string(),
            variant: plan.variant,
            downgraded_from: plan.downgraded_from,
            from_input: from,
            to_input: to,
            take_to_air_ms,
            cue_ms,
            duration_ms: plan.duration_ms,
            cut_point_ms: plan.cut_point_ms,
            frames_expected,
            frames_arrived: 0,
            frames_late: 0,
            worst_margin_ms: None,
        };
        let state = self.clone();
        let flow = *flow_id;
        let mixer = block.to_string();
        let player = Arc::clone(&ctx.player);
        tokio::spawn(async move {
            state
                .finish_stinger_take(flow, mixer, token, take, watch, report, player)
                .await;
        });

        Ok(StingerTakeResponse {
            index,
            variant: plan.variant,
            downgraded_from: plan.downgraded_from,
            take_to_air_ms,
            duration_ms: plan.duration_ms,
        })
    }

    /// Wait for the mixer to get past the clip, settle the pads, report what
    /// was measured and park the clip again.
    #[allow(clippy::too_many_arguments)]
    async fn finish_stinger_take(
        &self,
        flow: FlowId,
        mixer: String,
        token: u64,
        take: StingerTake,
        watch: StingerWatch,
        mut report: StingerTakeReport,
        player: Arc<MediaPlayerState>,
    ) {
        // Two output frames past the end, so the last keyframes have run.
        let target = take.end + 2 * take.frame_ns;
        let give_up = Instant::now()
            + Duration::from_nanos(take.end.saturating_sub(take.start))
            + Duration::from_millis(1_000)
            + FINISH_GRACE;
        loop {
            if !still_ours(flow, &mixer, token) {
                debug!("Stinger on {}: flow stopped during the take", mixer);
                return;
            }
            let position = self
                .inner
                .pipelines
                .read()
                .await
                .get(&flow)
                .and_then(|m| m.mixer_position_ns(&mixer));
            if position.is_some_and(|p| p >= target) {
                break;
            }
            if Instant::now() > give_up {
                warn!(
                    "Stinger on {}: the mixer did not get past the clip, settling anyway",
                    mixer
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        if let Some(manager) = self.inner.pipelines.read().await.get(&flow) {
            manager.finish_stinger(&mixer, &take);
        }
        report.frames_arrived = watch.stats.arrived.load(Ordering::Acquire);
        report.frames_late = watch.stats.late.load(Ordering::Acquire);
        report.worst_margin_ms = watch.worst_margin_ms();
        drop(watch);
        info!(
            "Stinger on {}: {:?} '{}' done: on air {:.0} ms after the take, {}/{} clip frames, {} late, worst margin {:?} ms",
            mixer,
            report.variant,
            report.file,
            report.take_to_air_ms,
            report.frames_arrived,
            report.frames_expected,
            report.frames_late,
            report.worst_margin_ms.map(|m| m.round())
        );

        // Park the clip again, so the next take is instant.
        let index = report.index;
        let cue_player = Arc::clone(&player);
        let cued = tokio::task::spawn_blocking(move || cue_player.cue(index)).await;
        let (ready, cue_ms) = match cued {
            Ok(Ok(took)) => (true, took.as_millis() as u64),
            Ok(Err(e)) => {
                warn!(
                    "Stinger on {}: could not park clip {} again: {}",
                    mixer, index, e
                );
                (false, 0)
            }
            Err(_) => (false, 0),
        };

        with_mixer(flow, &mixer, |s| {
            if s.running == Some(token) {
                s.running = None;
            }
            s.last_take = Some(report.clone());
            if ready {
                s.last_cue_ms = Some(cue_ms);
            }
        });
        self.inner.events.broadcast(StromEvent::StingerCued {
            flow_id: flow,
            block_id: mixer.clone(),
            index,
            ready,
            cue_ms,
        });
        self.inner.events.broadcast(StromEvent::StingerCompleted {
            flow_id: flow,
            block_id: mixer,
            report,
        });
    }
}
