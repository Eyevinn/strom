//! Reusable thumbnail tap for GStreamer video chains.
//!
//! Provides a tee-based tap that sits passively in a video chain.
//! When thumbnails are requested, it lazily attaches a processing branch
//! on the running pipeline. The branch auto-detaches after an idle timeout.
//!
//! Processing branch (when active):
//! ```text
//! GL path:
//! [tee] ─┬─ (passthrough)
//!         └─ queue ─[pad probe: rate limit]─→ glcolorscale
//!            → capsfilter(GL,RGBA,WxH) → gldownload
//!            → capsfilter(RGBA,WxH) → appsink
//!
//! Non-GL path:
//! [tee] ─┬─ (passthrough)
//!         └─ queue ─[pad probe: rate limit]─→ videoconvertscale
//!            → capsfilter(RGBA,WxH) → appsink
//! ```
//!
//! Frame rate is limited via a pad probe on the queue src pad that drops
//! buffers arriving sooner than `update_interval`. This is invisible to
//! caps negotiation (unlike videorate). The appsink callback does
//! lightweight JPEG encoding on the already-scaled frame.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use image::RgbImage;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, error, warn};

use crate::gst::thumbnail::ThumbnailError;

/// Configuration for a thumbnail tap.
#[derive(Debug, Clone)]
pub struct ThumbnailTapConfig {
    /// Target thumbnail width in pixels.
    pub width: u32,
    /// Target thumbnail height in pixels.
    pub height: u32,
    /// JPEG quality (0-100).
    pub jpeg_quality: u8,
    /// Detach the branch after this duration of inactivity.
    pub idle_timeout: Duration,
    /// Minimum interval between frame captures in the appsink callback.
    pub update_interval: Duration,
    /// Cached JPEG time-to-live — ignored when stale thumbnails are
    /// preferred over timeout errors (current behaviour).
    #[allow(dead_code)]
    pub cache_ttl: Duration,
}

impl Default for ThumbnailTapConfig {
    fn default() -> Self {
        Self {
            width: 320,
            height: 180,
            jpeg_quality: 75,
            idle_timeout: Duration::from_secs(10),
            update_interval: Duration::from_secs(1),
            cache_ttl: Duration::from_millis(500),
        }
    }
}

/// Internal mutable state for a thumbnail tap.
struct TapState {
    /// Whether the processing branch is currently attached.
    active: bool,
    /// When a thumbnail was last requested (used for idle timeout).
    last_request: Instant,
    /// GStreamer elements in the processing branch (for cleanup).
    branch_elements: Vec<gst::Element>,
    /// The tee request pad connected to the branch.
    tee_src_pad: Option<gst::Pad>,
    /// Cached JPEG bytes and the time they were generated.
    cached_jpeg: Option<(Vec<u8>, Instant)>,
}

impl TapState {
    fn new() -> Self {
        Self {
            active: false,
            last_request: Instant::now(),
            branch_elements: Vec::new(),
            tee_src_pad: None,
            cached_jpeg: None,
        }
    }
}

/// A thumbnail tap that sits passively in a video chain via a tee element.
///
/// When thumbnails are requested, it lazily attaches a processing branch
/// that uses GStreamer-native elements for format conversion and scaling.
/// The branch auto-detaches after a configurable idle timeout.
pub struct ThumbnailTap {
    tee: gst::Element,
    pipeline: gst::Pipeline,
    config: ThumbnailTapConfig,
    state: Arc<Mutex<TapState>>,
    name_prefix: String,
}

impl ThumbnailTap {
    /// Create a new thumbnail tap with an existing tee element.
    ///
    /// The tee must already be added to the pipeline and linked into
    /// the video chain. It should have `allow-not-linked=true`.
    pub fn new_with_tee(
        pipeline: &gst::Pipeline,
        name_prefix: &str,
        tee: gst::Element,
        config: ThumbnailTapConfig,
    ) -> Self {
        Self {
            tee,
            pipeline: pipeline.clone(),
            config,
            state: Arc::new(Mutex::new(TapState::new())),
            name_prefix: name_prefix.to_string(),
        }
    }

    /// Get the tee element to insert into a video chain.
    pub fn tee_element(&self) -> &gst::Element {
        &self.tee
    }

    /// Request a thumbnail. Activates the branch if needed. Returns JPEG bytes.
    pub fn get_thumbnail(&self) -> Result<Vec<u8>, ThumbnailError> {
        let mut state = self.state.lock().unwrap();
        state.last_request = Instant::now();

        // Activate branch if not active
        if !state.active {
            drop(state); // Release lock before activation
            self.activate_branch()?;
            // After activation, there's no frame yet
            return Err(ThumbnailError::Timeout);
        }

        // Return cached JPEG — even if slightly stale, a 1-2s old thumbnail
        // is better than returning Timeout and causing a visual flicker.
        if let Some((ref jpeg, _)) = state.cached_jpeg {
            return Ok(jpeg.clone());
        }

        // Branch active but no frame captured yet
        Err(ThumbnailError::Timeout)
    }

    /// Check idle timeout and detach branch if no requests for `idle_timeout`.
    pub fn maybe_deactivate(&self) {
        let state = self.state.lock().unwrap();
        if state.active && state.last_request.elapsed() > self.config.idle_timeout {
            drop(state);
            if let Err(e) = self.deactivate_branch() {
                warn!(
                    "Failed to deactivate thumbnail branch for {}: {}",
                    self.name_prefix, e
                );
            }
        }
    }

    /// Attach the processing branch to the tee on the running pipeline.
    fn activate_branch(&self) -> Result<(), ThumbnailError> {
        let mut state = self.state.lock().unwrap();
        if state.active {
            return Ok(());
        }

        // Don't activate until the pipeline is fully in PLAYING state.
        let pipeline_state = self.pipeline.current_state();
        let tee_caps = self.tee.static_pad("sink").and_then(|p| p.current_caps());
        let has_caps = tee_caps.is_some();

        debug!(
            "activate_branch check for {}: pipeline_state={:?}, tee_has_caps={}, tee_name={}",
            self.name_prefix,
            pipeline_state,
            has_caps,
            self.tee.name()
        );

        let (_, _, pending_state) = self.pipeline.state(gst::ClockTime::ZERO);
        let ok = pipeline_state == gst::State::Playing
            || (pipeline_state == gst::State::Paused && pending_state == gst::State::Playing);
        if !ok {
            debug!(
                "Skipping activation for {}: pipeline not Playing (state={:?}, pending={:?})",
                self.name_prefix, pipeline_state, pending_state
            );
            return Err(ThumbnailError::PipelineNotRunning);
        }

        if !has_caps {
            debug!(
                "Skipping activation for {}: no caps on tee sink",
                self.name_prefix
            );
            return Err(ThumbnailError::PipelineNotRunning);
        }

        debug!(
            "Activating thumbnail branch for {} ({}x{})",
            self.name_prefix, self.config.width, self.config.height
        );

        let prefix = &self.name_prefix;

        // Create elements.
        // Queue overrides: leaky + single-buffer so the thumbnail branch never
        // backpressures the main video chain and always processes the latest frame.
        let queue = gst::ElementFactory::make("queue")
            .name(format!("{}_thumb_queue", prefix))
            .property_from_str("leaky", "downstream")
            .property("max-size-buffers", 1u32)
            .property("max-size-time", 0u64)
            .property("max-size-bytes", 0u32)
            .build()
            .map_err(|e| ThumbnailError::FrameMapping(format!("queue: {}", e)))?;

        // Check if the tee outputs GL memory — if so, scale on GPU before downloading.
        let is_gl = self
            .tee
            .static_pad("sink")
            .and_then(|p| p.current_caps())
            .map(|caps| caps.to_string().contains("memory:GLMemory"))
            .unwrap_or(false);

        let mut branch_elements: Vec<gst::Element> = Vec::new();

        if is_gl {
            // GL path: scale on GPU, then download the small frame.
            // glcolorscale requires RGBA — the compositor tee already carries RGBA.
            let scale = gst::ElementFactory::make("glcolorscale")
                .name(format!("{}_thumb_glscale", prefix))
                .build()
                .map_err(|e| ThumbnailError::FrameMapping(format!("glcolorscale: {}", e)))?;
            branch_elements.push(scale);

            let gl_caps_str = format!(
                "video/x-raw(memory:GLMemory),format=RGBA,width={},height={}",
                self.config.width, self.config.height
            );
            let gl_caps = gst::Caps::from_str(&gl_caps_str)
                .map_err(|e| ThumbnailError::FrameMapping(format!("gl caps: {}", e)))?;
            let gl_capsfilter = gst::ElementFactory::make("capsfilter")
                .name(format!("{}_thumb_glcaps", prefix))
                .property("caps", &gl_caps)
                .build()
                .map_err(|e| ThumbnailError::FrameMapping(format!("gl capsfilter: {}", e)))?;
            branch_elements.push(gl_capsfilter);

            let download = gst::ElementFactory::make("gldownload")
                .name(format!("{}_thumb_gldownload", prefix))
                .build()
                .map_err(|e| ThumbnailError::FrameMapping(format!("gldownload: {}", e)))?;
            branch_elements.push(download);
        } else {
            // Non-GL path: CPU-based format conversion and scaling.
            let convert = gst::ElementFactory::make("videoconvertscale")
                .name(format!("{}_thumb_convert", prefix))
                .build()
                .map_err(|e| ThumbnailError::FrameMapping(format!("videoconvertscale: {}", e)))?;
            branch_elements.push(convert);
        }

        let caps_str = format!(
            "video/x-raw,format=RGBA,width={},height={}",
            self.config.width, self.config.height
        );
        let caps = gst::Caps::from_str(&caps_str)
            .map_err(|e| ThumbnailError::FrameMapping(format!("caps: {}", e)))?;
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(format!("{}_thumb_caps", prefix))
            .property("caps", &caps)
            .build()
            .map_err(|e| ThumbnailError::FrameMapping(format!("capsfilter: {}", e)))?;

        // `async` off: the branch joins a pipeline that is already running, and
        // a sink still wanting a preroll answers that state change with ASYNC,
        // taking the pipeline back to PAUSED until a frame arrives. Activation
        // is allowed during that wait, and a branch attached inside one never
        // completes its own state change: it takes a single buffer, then blocks
        // its streaming thread.
        let appsink = gst_app::AppSink::builder()
            .name(format!("{}_thumb_sink", prefix))
            .max_buffers(1)
            .drop(true)
            .sync(false)
            .async_(false)
            .build();

        // Set up appsink callback — pad probe on queue src limits fps,
        // so every frame that arrives here should be encoded.
        let callback_state = Arc::clone(&self.state);
        let jpeg_quality = self.config.jpeg_quality;
        let thumb_width = self.config.width;
        let thumb_height = self.config.height;
        let callback_prefix = self.name_prefix.clone();
        let frame_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let first_frame_time = Arc::new(Mutex::new(None::<Instant>));

        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let count = frame_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    let mut first = first_frame_time.lock().unwrap();
                    let start = *first.get_or_insert_with(Instant::now);
                    let elapsed = start.elapsed().as_secs_f64();
                    if count.is_multiple_of(10) {
                        let fps = if elapsed > 0.0 {
                            count as f64 / elapsed
                        } else {
                            0.0
                        };
                        debug!(
                            "Thumbnail tap {}: {} frames in {:.1}s ({:.2} fps)",
                            callback_prefix, count, elapsed, fps
                        );
                    }

                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;

                    let video_info =
                        gst_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;

                    let frame =
                        gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &video_info)
                            .map_err(|_| gst::FlowError::Error)?;

                    // Frame is already RGBA at target size thanks to the pipeline
                    match encode_rgba_frame_as_jpeg(&frame, thumb_width, thumb_height, jpeg_quality)
                    {
                        Ok(jpeg) => {
                            let mut state = callback_state.lock().unwrap();
                            state.cached_jpeg = Some((jpeg, Instant::now()));
                        }
                        Err(e) => {
                            error!(
                                "JPEG encoding failed in thumbnail tap {}: {}",
                                callback_prefix, e
                            );
                        }
                    }

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // Build the full element chain:
        //   GL:     queue [pad probe] → glcolorscale → glcaps → gldownload → capsfilter → appsink
        //   non-GL: queue [pad probe] → videoconvertscale → capsfilter → appsink
        let mut elements: Vec<gst::Element> = vec![queue.clone()];
        elements.extend(branch_elements.iter().cloned());
        elements.push(capsfilter.clone());
        elements.push(appsink.upcast_ref::<gst::Element>().clone());

        // Add all elements to the pipeline, with rollback on failure
        let mut added: Vec<gst::Element> = Vec::new();
        let add_result = (|| -> Result<(), ThumbnailError> {
            for elem in &elements {
                self.pipeline.add(elem).map_err(|e| {
                    ThumbnailError::FrameMapping(format!(
                        "Failed to add {} to pipeline: {}",
                        elem.name(),
                        e
                    ))
                })?;
                added.push(elem.clone());
            }

            // Link the chain sequentially
            for pair in elements.windows(2) {
                pair[0].link(&pair[1]).map_err(|e| {
                    ThumbnailError::FrameMapping(format!(
                        "Failed to link {}→{}: {}",
                        pair[0].name(),
                        pair[1].name(),
                        e
                    ))
                })?;
            }

            // Request a high-numbered pad (src_999) to avoid colliding with
            // pipeline-assigned pads (src_0, src_1, ...) regardless of activation order.
            // This is intentional — auto-assigned pads would work but a fixed name
            // makes the branch easily identifiable in debug graphs and logs.
            let tee_pad = self
                .tee
                .request_pad_simple("src_999")
                .ok_or_else(|| ThumbnailError::PadNotFound("tee src_999".to_string()))?;
            let queue_sink = queue
                .static_pad("sink")
                .ok_or_else(|| ThumbnailError::PadNotFound("queue sink".to_string()))?;
            tee_pad.link(&queue_sink).map_err(|e| {
                ThumbnailError::FrameMapping(format!("Failed to link tee→queue: {}", e))
            })?;

            // Rate-limit via pad probe on queue src: drop buffers that arrive
            // sooner than update_interval since the last passed buffer.
            // This is invisible to caps negotiation (unlike videorate).
            let probe_interval_ms = self.config.update_interval.as_millis() as u64;
            let probe_epoch = Instant::now();
            let probe_last_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let queue_src = queue
                .static_pad("src")
                .ok_or_else(|| ThumbnailError::PadNotFound("queue src".to_string()))?;
            queue_src.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                let now_ms = probe_epoch.elapsed().as_millis() as u64;
                let last = probe_last_ms.load(std::sync::atomic::Ordering::Relaxed);
                if now_ms.saturating_sub(last) < probe_interval_ms {
                    return gst::PadProbeReturn::Drop;
                }
                probe_last_ms.store(now_ms, std::sync::atomic::Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });

            // Sync all elements to parent state
            for elem in &elements {
                elem.sync_state_with_parent().map_err(|e| {
                    ThumbnailError::FrameMapping(format!(
                        "Failed to sync {} state: {}",
                        elem.name(),
                        e
                    ))
                })?;
            }

            Ok(())
        })();

        if let Err(e) = add_result {
            // Rollback: remove any elements we added
            for elem in &added {
                let _ = elem.set_state(gst::State::Null);
                let _ = self.pipeline.remove(elem);
            }
            return Err(e);
        }

        // Retrieve the tee src pad for later cleanup
        let tee_src_pad = self
            .tee
            .static_pad("src_999")
            .ok_or_else(|| ThumbnailError::PadNotFound("tee src_999 after link".to_string()))?;

        state.branch_elements = elements;
        state.tee_src_pad = Some(tee_src_pad);
        state.active = true;

        debug!("Thumbnail branch activated for {}", self.name_prefix);
        Ok(())
    }

    /// Detach the processing branch from the tee.
    fn deactivate_branch(&self) -> Result<(), ThumbnailError> {
        let mut state = self.state.lock().unwrap();
        if !state.active {
            return Ok(());
        }

        debug!("Deactivating thumbnail branch for {}", self.name_prefix);

        // Unlink tee src pad from queue sink
        if let Some(ref tee_src_pad) = state.tee_src_pad {
            if let Some(peer) = tee_src_pad.peer() {
                let _ = tee_src_pad.unlink(&peer);
            }
            // Release the request pad
            self.tee.release_request_pad(tee_src_pad);
        }

        // Set all branch elements to Null and remove from pipeline
        for elem in &state.branch_elements {
            let _ = elem.set_state(gst::State::Null);
        }
        for elem in &state.branch_elements {
            let _ = self.pipeline.remove(elem);
        }

        state.branch_elements.clear();
        state.tee_src_pad = None;
        state.active = false;

        debug!("Thumbnail branch deactivated for {}", self.name_prefix);
        Ok(())
    }
}

impl std::fmt::Debug for ThumbnailTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThumbnailTap")
            .field("name_prefix", &self.name_prefix)
            .field("config", &self.config)
            .finish()
    }
}

/// Encode an RGBA video frame as JPEG.
///
/// The frame is expected to be RGBA format at the target dimensions
/// (already scaled by the GStreamer pipeline). Alpha is stripped during encoding.
fn encode_rgba_frame_as_jpeg(
    frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, ThumbnailError> {
    let data = frame
        .plane_data(0)
        .map_err(|e| ThumbnailError::FrameMapping(format!("Failed to get plane data: {}", e)))?;
    let stride = frame.plane_stride()[0] as usize;

    // Strip alpha: RGBA → RGB
    let mut rgb_data = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height as usize {
        let row_start = y * stride;
        for x in 0..width as usize {
            let px = row_start + x * 4;
            rgb_data.push(data[px]);
            rgb_data.push(data[px + 1]);
            rgb_data.push(data[px + 2]);
        }
    }

    let img = RgbImage::from_raw(width, height, rgb_data)
        .ok_or_else(|| ThumbnailError::FrameMapping("Failed to create RGB image".to_string()))?;

    let mut buffer = std::io::Cursor::new(Vec::new());
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, quality);
    encoder
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| ThumbnailError::JpegEncoding(e.to_string()))?;

    Ok(buffer.into_inner())
}

/// Shared storage for thumbnail taps, indexed by block_id, then by input index.
pub type ThumbnailTapStore = Arc<Mutex<HashMap<String, Vec<ThumbnailTap>>>>;

use std::collections::HashMap;

/// Create a new empty thumbnail tap store.
pub fn new_tap_store() -> ThumbnailTapStore {
    Arc::new(Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thumbnail_tap_config_default() {
        let config = ThumbnailTapConfig::default();
        assert_eq!(config.width, 320);
        assert_eq!(config.height, 180);
        assert_eq!(config.jpeg_quality, 75);
        assert_eq!(config.idle_timeout, Duration::from_secs(10));
        assert_eq!(config.update_interval, Duration::from_secs(1));
        assert_eq!(config.cache_ttl, Duration::from_millis(500));
    }
}
