//! TAMS Output block: records pre-encoded streams into a TAMS store.
//!
//! Two container modes (the `container` property):
//! - **MP4 (default):** each essence goes to its own `splitmuxsink` (mp4mux) and is
//!   registered as a separate single-essence TAMS flow (video flow + audio flow
//!   grouped under one Source). Canonical TAMS model, max flexibility.
//! - **MPEG-TS:** all essences are muxed by one `mpegtsmux`/`splitmuxsink` into TS
//!   segments and registered on a single NMOS `format:mux` flow. Broadcast-native,
//!   inherent A/V sync, fewer objects.
//!
//! Each split file is a complete, GOP-aligned, independently decodable container =
//! one TAMS segment. Files are written to a temp dir; the uploader uploads each as a
//! media object, registers it with its timerange, then deletes it (kept on disk if
//! upload ultimately fails — see the uploader module).
//!
//! Only pre-encoded material is accepted — raw video/audio is rejected (add an
//! encoder block upstream). See `docs/archive/TAMS_INTEGRATION_PLAN.md`.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::client_auth::AuthMethod;
use crate::osc::SatProvider;
use crate::tams::client::{FlowSpec, TamsClient};
use crate::tams::uploader::{channel, new_tail_slot, spawn_uploader, FragmentReady, TailFragment};
use gst::glib;
use gst::glib::prelude::ToValue;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use strom_types::tams::{
    CONTENT_TYPE_MP4, CONTENT_TYPE_MPEGTS, FORMAT_AUDIO, FORMAT_MUX, FORMAT_VIDEO,
};
use strom_types::{block::*, PropertyValue, *};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub struct TamsOutputBuilder;

const DEFAULT_SEGMENT_SECS: u64 = 2;
const DEFAULT_NUM_VIDEO_TRACKS: usize = 1;
const DEFAULT_NUM_AUDIO_TRACKS: usize = 1;
const DEFAULT_CONTAINER: &str = "mp4";

/// Segment container format.
#[derive(Clone, Copy, PartialEq)]
enum Container {
    /// Separate single-essence MP4 flows (one per essence). Canonical TAMS model.
    Mp4,
    /// One muxed MPEG-TS flow (NMOS `format:mux`) carrying all essences together.
    MpegTs,
}

impl Container {
    fn muxer_factory(&self) -> &'static str {
        match self {
            Container::Mp4 => "mp4mux",
            Container::MpegTs => "mpegtsmux",
        }
    }
    fn file_ext(&self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::MpegTs => "ts",
        }
    }
    fn content_type(&self) -> &'static str {
        match self {
            Container::Mp4 => CONTENT_TYPE_MP4,
            Container::MpegTs => CONTENT_TYPE_MPEGTS,
        }
    }
}

/// Fixed namespace for deriving deterministic TAMS source/flow UUIDs from a block
/// instance id, so restarting a flow appends to the same TAMS timeline.
const TAMS_NAMESPACE: Uuid = Uuid::from_u128(0x7a3f_2b10_5c84_4e6d_9f12_a8b7_c6d5_e4f3);

fn prop_str(props: &HashMap<String, PropertyValue>, key: &str) -> Option<String> {
    match props.get(key) {
        Some(PropertyValue::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn prop_usize(props: &HashMap<String, PropertyValue>, key: &str, default: usize) -> usize {
    match props.get(key) {
        Some(PropertyValue::UInt(u)) => *u as usize,
        Some(PropertyValue::Int(i)) if *i >= 0 => *i as usize,
        _ => default,
    }
}

fn prop_u64(props: &HashMap<String, PropertyValue>, key: &str, default: u64) -> u64 {
    match props.get(key) {
        Some(PropertyValue::UInt(u)) => *u,
        Some(PropertyValue::Int(i)) if *i >= 0 => *i as u64,
        _ => default,
    }
}

/// Parse a `key=value, key=value` string into TAMS flow tags. Blank/keyless
/// entries are skipped.
fn parse_tags(s: &Option<String>) -> Vec<(String, String)> {
    s.as_deref()
        .map(|raw| {
            raw.split(',')
                .filter_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    let k = k.trim();
                    if k.is_empty() {
                        None
                    } else {
                        Some((k.to_string(), v.trim().to_string()))
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Derive a deterministic UUID (string) for a TAMS entity belonging to a block.
fn derive_id(instance_id: &str, suffix: &str) -> String {
    Uuid::new_v5(
        &TAMS_NAMESPACE,
        format!("{}:{}", instance_id, suffix).as_bytes(),
    )
    .to_string()
}

impl BlockBuilder for TamsOutputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let num_video = prop_usize(properties, "num_video_tracks", DEFAULT_NUM_VIDEO_TRACKS).min(1);
        let num_audio = prop_usize(properties, "num_audio_tracks", DEFAULT_NUM_AUDIO_TRACKS).min(1);
        let mut inputs = Vec::new();
        if num_video >= 1 {
            inputs.push(ExternalPad {
                label: Some("V0".to_string()),
                name: "video_in_0".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "video_input_0".to_string(),
                internal_pad_name: "sink".to_string(),
            });
        }
        if num_audio >= 1 {
            inputs.push(ExternalPad {
                label: Some("A0".to_string()),
                name: "audio_in_0".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audio_input_0".to_string(),
                internal_pad_name: "sink".to_string(),
            });
        }
        Some(ExternalPads {
            inputs,
            outputs: vec![],
        })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building TAMS Output block instance: {}", instance_id);

        let gateway_url = prop_str(properties, "gateway_url")
            .or_else(|| std::env::var("STROM_TAMS_GATEWAY_URL").ok())
            .ok_or_else(|| {
                BlockBuildError::InvalidProperty(
                    "TAMS Output: gateway_url is required (or set STROM_TAMS_GATEWAY_URL)"
                        .to_string(),
                )
            })?;
        // Auth mode selects how the gateway is authenticated. "static" (default) uses
        // a long-lived bearer token (for TAMS gateways run outside OSC); "osc" mints
        // short-lived Service Access Tokens from the OSC PAT configured on this Strom
        // instance (STROM_OSC_PAT / OSC_ACCESS_TOKEN).
        let auth_mode = prop_str(properties, "auth_mode").unwrap_or_else(|| "static".to_string());
        let auth_plan = match auth_mode.as_str() {
            "osc" => {
                let provider = crate::osc::sat_provider();
                let service_id = prop_str(properties, "osc_service_id")
                    .or_else(|| crate::osc::derive_service_id(&gateway_url))
                    .ok_or_else(|| {
                        BlockBuildError::InvalidProperty(
                            "TAMS Output: OSC mode could not derive the service id from \
                             gateway_url — set the OSC Service ID property"
                                .to_string(),
                        )
                    })?;
                info!(
                    "TAMS Output {}: OSC PAT/SAT auth for service {}",
                    instance_id, service_id
                );
                AuthPlan::Osc {
                    provider,
                    service_id,
                }
            }
            _ => {
                let auth = match prop_str(properties, "api_token")
                    .or_else(|| std::env::var("STROM_TAMS_API_TOKEN").ok())
                {
                    Some(t) => AuthMethod::Bearer(t),
                    None => AuthMethod::None,
                };
                AuthPlan::Fixed(auth)
            }
        };

        let segment_secs =
            prop_u64(properties, "segment_duration_secs", DEFAULT_SEGMENT_SECS).max(1);
        let num_video = prop_usize(properties, "num_video_tracks", DEFAULT_NUM_VIDEO_TRACKS).min(1);
        let num_audio = prop_usize(properties, "num_audio_tracks", DEFAULT_NUM_AUDIO_TRACKS).min(1);
        if num_video == 0 && num_audio == 0 {
            return Err(BlockBuildError::InvalidProperty(
                "TAMS Output: at least one of video/audio tracks is required".to_string(),
            ));
        }

        let source_id = prop_str(properties, "tams_source_id")
            .unwrap_or_else(|| derive_id(instance_id, "source"));
        let label = prop_str(properties, "label");
        let description = prop_str(properties, "description");
        let tags = parse_tags(&prop_str(properties, "tags"));

        let container = match prop_str(properties, "container")
            .unwrap_or_else(|| DEFAULT_CONTAINER.to_string())
            .as_str()
        {
            "mpegts" => Container::MpegTs,
            _ => Container::Mp4,
        };

        let mut elements: Vec<(String, gst::Element)> = Vec::new();

        match container {
            // Separate single-essence MP4 flows: one per essence.
            Container::Mp4 => {
                if num_video >= 1 {
                    let spec = FlowSpec {
                        flow_id: derive_id(instance_id, "video"),
                        source_id: source_id.clone(),
                        format: FORMAT_VIDEO.to_string(),
                        codec: "video/h264".to_string(), // fallback; real codec from caps
                        container: CONTENT_TYPE_MP4.to_string(),
                        // Distinguish the two flows of one recording in flow listings.
                        label: label.as_ref().map(|l| format!("{} (video)", l)),
                        description: description.clone(),
                        tags: tags.clone(),
                    };
                    build_flow_chain(
                        instance_id,
                        "video",
                        Container::Mp4,
                        segment_secs,
                        &[Essence::Video],
                        true,
                        gateway_url.clone(),
                        auth_plan.clone(),
                        spec,
                        ctx,
                        &mut elements,
                    )?;
                }
                if num_audio >= 1 {
                    let spec = FlowSpec {
                        flow_id: derive_id(instance_id, "audio"),
                        source_id: source_id.clone(),
                        format: FORMAT_AUDIO.to_string(),
                        codec: "audio/aac".to_string(), // fallback; real codec from caps
                        container: CONTENT_TYPE_MP4.to_string(),
                        label: label.as_ref().map(|l| format!("{} (audio)", l)),
                        description: description.clone(),
                        tags: tags.clone(),
                    };
                    build_flow_chain(
                        instance_id,
                        "audio",
                        Container::Mp4,
                        segment_secs,
                        &[Essence::Audio],
                        true,
                        gateway_url.clone(),
                        auth_plan.clone(),
                        spec,
                        ctx,
                        &mut elements,
                    )?;
                }
            }
            // One muxed MPEG-TS flow carrying all essences together.
            Container::MpegTs => {
                let mut inputs = Vec::new();
                if num_video >= 1 {
                    inputs.push(Essence::Video);
                }
                if num_audio >= 1 {
                    inputs.push(Essence::Audio);
                }
                let spec = FlowSpec {
                    flow_id: derive_id(instance_id, "mux"),
                    source_id: source_id.clone(),
                    format: FORMAT_MUX.to_string(),
                    codec: CONTENT_TYPE_MPEGTS.to_string(),
                    container: CONTENT_TYPE_MPEGTS.to_string(),
                    label: label.clone(),
                    description: description.clone(),
                    tags: tags.clone(),
                };
                build_flow_chain(
                    instance_id,
                    "mux",
                    Container::MpegTs,
                    segment_secs,
                    &inputs,
                    false, // codec is the container; no per-essence detection
                    gateway_url.clone(),
                    auth_plan,
                    spec,
                    ctx,
                    &mut elements,
                )?;
            }
        }

        info!(
            "TAMS Output {}: built ({} video, {} audio, {}), gateway {}, segment {}s",
            instance_id,
            num_video,
            num_audio,
            container.content_type(),
            gateway_url,
            segment_secs
        );

        Ok(BlockBuildResult {
            elements,
            internal_links: vec![],
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Auth configuration resolved per flow at pipeline start.
///
/// OSC mode keys its PAT by the flow id, which is only known in the element-setup
/// closure — not at block-build time. So we carry the recipe and finalize the
/// concrete [`AuthMethod`] once the flow id is available.
#[derive(Clone)]
enum AuthPlan {
    /// Flow-id-independent auth (static bearer token, or none).
    Fixed(AuthMethod),
    /// OSC PAT/SAT; the credential key is the flow id, filled in at setup.
    Osc {
        provider: Arc<SatProvider>,
        service_id: String,
    },
}

impl AuthPlan {
    fn resolve(&self, credential_key: &str) -> AuthMethod {
        match self {
            AuthPlan::Fixed(auth) => auth.clone(),
            AuthPlan::Osc {
                provider,
                service_id,
            } => AuthMethod::Osc {
                provider: provider.clone(),
                service_id: service_id.clone(),
                credential_key: credential_key.to_string(),
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Essence {
    Video,
    Audio,
}

impl Essence {
    fn input_id(&self) -> &'static str {
        match self {
            Essence::Video => "video_input_0",
            Essence::Audio => "audio_input_0",
        }
    }
    fn tag(&self) -> &'static str {
        match self {
            Essence::Video => "video",
            Essence::Audio => "audio",
        }
    }
}

/// Build one TAMS flow chain: N identity inputs -> (dynamic parsers) -> shared
/// muxer/splitmuxsink -> segment files. For MP4 each chain has a single essence
/// (one flow per essence); for MPEG-TS a single chain muxes all essences into one
/// `format:mux` flow. Wires the per-fragment timerange capture + uploader at start.
///
/// `suffix` distinguishes the temp dir / element names (`video`/`audio`/`mux`).
/// `detect_codec` controls whether the caps probe overrides the flow's codec label
/// (true for single-essence MP4; false for muxed TS, where the codec is the container).
#[allow(clippy::too_many_arguments)]
fn build_flow_chain(
    instance_id: &str,
    suffix: &str,
    container: Container,
    segment_secs: u64,
    inputs: &[Essence],
    detect_codec: bool,
    gateway_url: String,
    auth_plan: AuthPlan,
    spec: FlowSpec,
    ctx: &BlockBuildContext,
    elements: &mut Vec<(String, gst::Element)>,
) -> Result<(), BlockBuildError> {
    if inputs.is_empty() {
        return Ok(());
    }

    // Temp directory for this flow's segment files.
    let safe_id: String = instance_id
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let temp_dir = std::env::temp_dir().join(format!("strom-tams-{}-{}", safe_id, suffix));
    if let Err(e) = std::fs::create_dir_all(&temp_dir) {
        return Err(BlockBuildError::InvalidConfiguration(format!(
            "TAMS Output: cannot create temp dir {}: {}",
            temp_dir.display(),
            e
        )));
    }

    // One muxer per fragment => each split file is a complete, decodable container.
    let mux_factory = container.muxer_factory();
    let mux_id = format!("{}:{}_mux", instance_id, suffix);
    let mux = gst::ElementFactory::make(mux_factory)
        .name(&mux_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", mux_factory, e)))?;
    if container == Container::MpegTs && mux.has_property("alignment") {
        mux.set_property("alignment", 7i32);
    }

    let sink_id = format!("{}:{}_splitmuxsink", instance_id, suffix);
    let splitmuxsink = gst::ElementFactory::make("splitmuxsink")
        .name(&sink_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("splitmuxsink: {}", e)))?;
    splitmuxsink.set_property("muxer", &mux);
    splitmuxsink.set_property("max-size-time", segment_secs * 1_000_000_000);
    // Fallback location template; the format-location-full signal overrides it.
    let fallback = temp_dir.join(format!("seg_%05d.{}", container.file_ext()));
    splitmuxsink.set_property("location", fallback.to_string_lossy().as_ref());
    // Ask upstream for keyframes at split points so segments are GOP-aligned.
    if splitmuxsink.has_property("send-keyframe-requests") {
        splitmuxsink.set_property("send-keyframe-requests", true);
    }

    // Codec label for the TAMS flow, derived from caps at runtime by the probes and
    // read by the uploader when it creates the flow.
    let detected_codec: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // One identity input + caps probe per essence, each requesting its own sink pad.
    for &essence in inputs {
        let sink_pad_name = request_sink_pad(&splitmuxsink, essence)?;

        let input_id = format!("{}:{}", instance_id, essence.input_id());
        let input = gst::ElementFactory::make("identity")
            .name(&input_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("identity: {}", e)))?;
        let src_pad = input
            .static_pad("src")
            .ok_or_else(|| BlockBuildError::ElementCreation("identity has no src pad".into()))?;

        let parser_inserted = Arc::new(AtomicBool::new(false));
        let splitmuxsink_weak = splitmuxsink.downgrade();
        let instance_id_owned = instance_id.to_string();
        let detected_codec_probe = detected_codec.clone();
        src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
            let event = match info.data.as_ref() {
                Some(gst::PadProbeData::Event(e)) => e,
                _ => return gst::PadProbeReturn::Ok,
            };
            if event.type_() != gst::EventType::Caps {
                return gst::PadProbeReturn::Ok;
            }
            if parser_inserted.swap(true, Ordering::SeqCst) {
                return gst::PadProbeReturn::Ok;
            }
            let caps = match event.view() {
                gst::EventView::Caps(c) => c.caps().to_owned(),
                _ => return gst::PadProbeReturn::Ok,
            };
            let structure = match caps.structure(0) {
                Some(s) => s,
                None => return gst::PadProbeReturn::Ok,
            };
            let caps_name = structure.name().to_string();

            // (GStreamer parser factory, TAMS codec label) derived from caps.
            let (parser_factory, codec_label) = match essence {
                Essence::Video => match caps_name.as_str() {
                    "video/x-h264" => ("h264parse", "video/h264"),
                    "video/x-h265" => ("h265parse", "video/h265"),
                    "video/x-raw" => {
                        warn!(
                            "TAMS Output {}: raw video rejected — add an encoder upstream",
                            instance_id_owned
                        );
                        return gst::PadProbeReturn::Ok;
                    }
                    other => {
                        warn!(
                            "TAMS Output {}: unsupported video codec {} (H.264/H.265 only)",
                            instance_id_owned, other
                        );
                        return gst::PadProbeReturn::Ok;
                    }
                },
                Essence::Audio => {
                    let mpegversion = structure.get::<i32>("mpegversion").unwrap_or(0);
                    match caps_name.as_str() {
                        "audio/mpeg" if mpegversion == 1 => ("mpegaudioparse", "audio/mpeg"),
                        "audio/mpeg" => ("aacparse", "audio/aac"),
                        "audio/x-opus" => ("opusparse", "audio/opus"),
                        "audio/x-ac3" => ("ac3parse", "audio/ac3"),
                        "audio/x-raw" => {
                            warn!(
                                "TAMS Output {}: raw audio rejected — add an encoder upstream",
                                instance_id_owned
                            );
                            return gst::PadProbeReturn::Ok;
                        }
                        other => {
                            warn!(
                                "TAMS Output {}: unsupported audio codec {}",
                                instance_id_owned, other
                            );
                            return gst::PadProbeReturn::Ok;
                        }
                    }
                }
            };

            // For single-essence flows, record the detected codec as the flow's codec.
            if detect_codec {
                if let Ok(mut c) = detected_codec_probe.lock() {
                    *c = Some(codec_label.to_string());
                }
            }

            let splitmuxsink = match splitmuxsink_weak.upgrade() {
                Some(e) => e,
                None => return gst::PadProbeReturn::Ok,
            };
            let bin = match splitmuxsink
                .parent()
                .and_then(|p| p.downcast::<gst::Bin>().ok())
            {
                Some(b) => b,
                None => return gst::PadProbeReturn::Ok,
            };
            let parser_name = format!("{}:{}_parser", instance_id_owned, essence.tag());
            let parser = match gst::ElementFactory::make(parser_factory)
                .name(&parser_name)
                .build()
            {
                Ok(p) => p,
                Err(e) => {
                    error!(
                        "TAMS Output {}: failed to create {}: {}",
                        instance_id_owned, parser_factory, e
                    );
                    return gst::PadProbeReturn::Ok;
                }
            };
            if parser.has_property("config-interval") {
                parser.set_property("config-interval", -1i32);
            }
            if bin.add(&parser).is_err() || parser.sync_state_with_parent().is_err() {
                error!(
                    "TAMS Output {}: failed to add/sync parser",
                    instance_id_owned
                );
                return gst::PadProbeReturn::Ok;
            }
            let (psink, psrc) = match (parser.static_pad("sink"), parser.static_pad("src")) {
                (Some(a), Some(b)) => (a, b),
                _ => return gst::PadProbeReturn::Ok,
            };
            let sink_pad = match splitmuxsink.static_pad(&sink_pad_name) {
                Some(p) => p,
                None => return gst::PadProbeReturn::Ok,
            };
            if pad.link(&psink).is_err() || psrc.link(&sink_pad).is_err() {
                error!(
                    "TAMS Output {}: failed to link parser chain",
                    instance_id_owned
                );
                return gst::PadProbeReturn::Ok;
            }
            info!(
                "TAMS Output {}: {} chain linked via {}",
                instance_id_owned,
                essence.tag(),
                parser_factory
            );
            gst::PadProbeReturn::Ok
        });

        elements.push((input_id, input));
    }

    elements.push((sink_id, splitmuxsink.clone()));

    // Wire timerange capture + uploader at pipeline start (flow_id + events available).
    let block_id = instance_id.to_string();
    let temp_dir_for_setup = temp_dir.clone();
    let chain_tag = suffix.to_string();
    let content_type = container.content_type().to_string();
    let file_ext = container.file_ext().to_string();
    let tail_segment_ns = segment_secs.saturating_mul(1_000_000_000);
    ctx.register_element_setup(Box::new(move |flow_id, events| {
        // OSC auth keys its PAT by the flow id (tenant isolation on a shared
        // instance), which is only known here — so finalize the gateway client now.
        let credential_key = flow_id.to_string();
        if let AuthPlan::Osc { provider, .. } = &auth_plan {
            if !provider.has_pat_for(&credential_key) {
                warn!(
                    "TAMS {}: OSC mode but no PAT for flow {} yet — uploads will fail \
                     until one is configured (STROM_OSC_PAT / OSC_ACCESS_TOKEN, or push \
                     via PUT /api/osc/pat/{})",
                    block_id, credential_key, credential_key
                );
            }
        }
        let client = match TamsClient::new(&gateway_url, auth_plan.resolve(&credential_key)) {
            Ok(c) => c,
            Err(e) => {
                error!("TAMS {}: failed to build gateway client: {:#}", block_id, e);
                return;
            }
        };

        let (tx, rx) = channel();
        // Holds the currently-open (not-yet-rotated) fragment; flushed on shutdown.
        let tail = new_tail_slot();
        spawn_uploader(
            client,
            spec,
            detected_codec,
            content_type,
            flow_id,
            block_id.clone(),
            events,
            rx,
            tail.clone(),
            tail_segment_ns,
        );

        // Per-fragment timeline state: maps GStreamer PTS to an absolute TAI timeline.
        let state = Arc::new(Mutex::new(FragState::default()));
        let temp_dir = temp_dir_for_setup;
        let tag = chain_tag;
        let block_id_sig = block_id;

        // A fresh GStreamer TAI clock (CLOCK_TAI on Linux). The kernel maintains the
        // current TAI-UTC offset when ntp/chrony disciplines it, so we never hardcode
        // leap seconds. This is independent of the pipeline's own clock (which is
        // often Monotonic): we only sample TAI once to anchor the timeline, then
        // advance it by buffer-PTS deltas, so segments stay contiguous.
        let tai_clock: gst::Clock = glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Tai)
            .build()
            .upcast();
        let realtime_clock: gst::Clock = glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Realtime)
            .build()
            .upcast();

        let _ = splitmuxsink.connect("format-location-full", false, move |args| {
            let fragment_id = args.get(1).and_then(|v| v.get::<u32>().ok()).unwrap_or(0);
            let pts_ns = args
                .get(2)
                .and_then(|v| v.get::<gst::Sample>().ok())
                .and_then(|s| s.buffer().and_then(|b| b.pts()))
                .map(|t| t.nseconds())
                .unwrap_or(0);

            let location = temp_dir.join(format!("seg_{:05}.{}", fragment_id, file_ext));
            let location_str = location.to_string_lossy().to_string();

            // Anchor the timeline once, then advance by PTS deltas.
            let abs_start = {
                let mut st = state.lock().unwrap();
                if !st.initialized {
                    st.initialized = true;
                    st.pts0_ns = pts_ns;
                    // If CLOCK_TAI has no offset set (ntp/chrony not disciplining it),
                    // it equals CLOCK_REALTIME — timestamps would be UTC, ~tens of
                    // seconds off true TAI. Warn rather than silently mislabel.
                    let tai_now = tai_clock.time().nseconds();
                    let rt_now = realtime_clock.time().nseconds();
                    if tai_now.abs_diff(rt_now) < 1_000_000_000 {
                        warn!(
                            "TAMS {} ({}): CLOCK_TAI offset appears unset (TAI == UTC); \
                             segment timestamps will not be true TAI. Ensure ntp/chrony \
                             sets the system TAI offset.",
                            block_id_sig, tag
                        );
                    }
                    st.t0_tai_ns = tai_now;
                }
                st.t0_tai_ns + pts_ns.saturating_sub(st.pts0_ns)
            };

            // The previously open fragment is now complete: [its start, this start).
            // The just-opened one becomes the new tail (flushed on shutdown).
            if let Ok(mut tail) = tail.lock() {
                if let Some(prev) = tail.take() {
                    let ready = FragmentReady {
                        path: prev.path,
                        start_ns: prev.start_ns,
                        end_ns: abs_start,
                    };
                    if let Err(e) = tx.try_send(ready) {
                        warn!(
                            "TAMS {} ({}): dropping segment, uploader channel full: {}",
                            block_id_sig, tag, e
                        );
                    }
                }
                *tail = Some(TailFragment {
                    path: location.clone(),
                    start_ns: abs_start,
                });
            }

            debug!(
                "TAMS {} ({}): opening segment {}",
                block_id_sig, tag, location_str
            );
            Some(location_str.to_value())
        });
    }));

    Ok(())
}

/// Request the splitmuxsink sink pad for a given essence (`video` or `audio_%u`).
fn request_sink_pad(
    splitmuxsink: &gst::Element,
    essence: Essence,
) -> Result<String, BlockBuildError> {
    let pad = match essence {
        Essence::Video => splitmuxsink.request_pad_simple("video").ok_or_else(|| {
            BlockBuildError::ElementCreation("splitmuxsink: failed to request video pad".into())
        })?,
        Essence::Audio => {
            let tmpl = splitmuxsink.pad_template("audio_%u").ok_or_else(|| {
                BlockBuildError::ElementCreation("splitmuxsink: no audio_%u template".into())
            })?;
            splitmuxsink.request_pad(&tmpl, None, None).ok_or_else(|| {
                BlockBuildError::ElementCreation("splitmuxsink: failed to request audio pad".into())
            })?
        }
    };
    Ok(pad.name().to_string())
}

#[derive(Default)]
struct FragState {
    initialized: bool,
    /// PTS of the very first fragment, used as the timeline origin.
    pts0_ns: u64,
    /// Absolute TAI time (ns) corresponding to `pts0_ns`.
    t0_tai_ns: u64,
}

/// Get TAMS Output block definitions.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![tams_output_definition()]
}

fn tams_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.tams_output".to_string(),
        name: "TAMS Output".to_string(),
        description: "Records pre-encoded video/audio into a TAMS store (Time-Addressable Media Store) via an Eyevinn TAMS Gateway. Either separate single-essence MP4 flows or one muxed MPEG-TS flow.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            simple_prop(
                "gateway_url",
                "Gateway URL",
                "Base URL of the TAMS gateway, e.g. http://localhost:8000",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
            ExposedProperty {
                name: "container".to_string(),
                label: "Container".to_string(),
                description: "Segment container. MP4 = separate single-essence flows (one per essence). MPEG-TS = one muxed flow (video+audio together, NMOS format:mux).".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "mp4".to_string(),
                            label: Some("MP4 (separate flows)".to_string()),
                        },
                        EnumValue {
                            value: "mpegts".to_string(),
                            label: Some("MPEG-TS (muxed)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String(DEFAULT_CONTAINER.to_string())),
                mapping: block_mapping("container"),
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "auth_mode".to_string(),
                label: "Authentication".to_string(),
                description: "How to authenticate to the gateway. Static API Token = a bearer token. OSC PAT/SAT = mint short-lived Service Access Tokens from the OSC Personal Access Token configured on this Strom instance (STROM_OSC_PAT / OSC_ACCESS_TOKEN).".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "static".to_string(),
                            label: Some("Static API Token".to_string()),
                        },
                        EnumValue {
                            value: "osc".to_string(),
                            label: Some("OSC PAT/SAT".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("static".to_string())),
                mapping: block_mapping("auth_mode"),
                live: false,
                persist: None,
            },
            simple_prop(
                "api_token",
                "API Token",
                "Static bearer token for the gateway (Static API Token mode only). Leave empty when behind an access gate. Ignored in OSC PAT/SAT mode.",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
            simple_prop(
                "osc_service_id",
                "OSC Service ID",
                "OSC PAT/SAT mode only. The OSC service type the SAT is scoped to. Leave empty to derive it from the gateway URL (the service-type label of an *.osaas.io host).",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
            simple_prop(
                "label",
                "Title",
                "Human-readable title stored on the TAMS flow(s), e.g. \"Studio A - Camera 4\". In MP4 mode the video/audio flows get \" (video)\"/\" (audio)\" appended.",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
            simple_prop(
                "description",
                "Description",
                "Optional longer description stored on the TAMS flow(s).",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
            simple_prop(
                "tags",
                "Tags",
                "Optional flow tags for grouping/search, as \"key=value, key=value\" (e.g. \"production=Studio A, camera=4\").",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
            simple_prop(
                "num_video_tracks",
                "Video Tracks",
                "Number of video tracks (0 or 1).",
                PropertyType::Int,
                PropertyValue::Int(1),
            ),
            simple_prop(
                "num_audio_tracks",
                "Audio Tracks",
                "Number of audio tracks (0 or 1).",
                PropertyType::Int,
                PropertyValue::Int(1),
            ),
            simple_prop(
                "segment_duration_secs",
                "Segment Duration (s)",
                "Target duration of each TAMS segment in seconds.",
                PropertyType::Int,
                PropertyValue::Int(DEFAULT_SEGMENT_SECS as i64),
            ),
            simple_prop(
                "tams_source_id",
                "TAMS Source ID",
                "Optional source UUID. Leave empty to derive a stable one from this block.",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
        ],
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("V0".to_string()),
                    name: "video_in_0".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "video_input_0".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_in_0".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audio_input_0".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: None,
            width: Some(3.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}

fn block_mapping(name: &str) -> PropertyMapping {
    PropertyMapping {
        element_id: "_block".to_string(),
        property_name: name.to_string(),
        transform: None,
    }
}

fn simple_prop(
    name: &str,
    label: &str,
    description: &str,
    property_type: PropertyType,
    default: PropertyValue,
) -> ExposedProperty {
    ExposedProperty {
        name: name.to_string(),
        label: label.to_string(),
        description: description.to_string(),
        property_type,
        default_value: Some(default),
        mapping: block_mapping(name),
        live: false,
        persist: None,
    }
}
