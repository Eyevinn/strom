//! TAMS Output block: records pre-encoded streams into a TAMS store.
//!
//! Two container modes (the `container` property):
//! - **MPEG-TS (default):** all essences are muxed by `mpegtsmux`
//!   into TS segments and registered on a single NMOS `format:multi` flow.
//!   Broadcast-native, inherent A/V sync, fewer objects.
//! - **MP4:** each essence goes to its own `mp4mux` segments and is
//!   registered as a separate single-essence TAMS flow (video flow + audio flow
//!   grouped under one Source). Canonical TAMS model, max flexibility.
//!
//! The segmenter ([`crate::tams::segmenter`]) cuts the streams on the TAI grid,
//! one muxer per fragment. Each split file is a complete, GOP-aligned, independently decodable container =
//! one TAMS segment. Files are written to a temp dir; the uploader uploads each as a
//! media object, registers it with its timerange, then deletes it (kept on disk if
//! upload ultimately fails — see the uploader module).
//!
//! Only pre-encoded material is accepted — raw video/audio is rejected (add an
//! encoder block upstream). See `docs/archive/TAMS_INTEGRATION_PLAN.md`.

use super::refusal::{accept_input, audio_refusal, refuse_input, video_refusal};
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::tams::auth::TamsAuth;
use crate::tams::client::{FlowSpec, TamsClient};
use crate::tams::segmenter::{OpenDecision, OpenRequest, Segmenter};
use crate::tams::tai_mapping::{
    grid_miss_ns, reference_tai_ns, shared_pipeline_mapping, AnchorKind, FragmentTimeline, HostTai,
    FIRST_ON_GRID_FRAGMENT,
};
use crate::tams::uploader::{
    channel, clear_orphans, closed_end_ns, media_end_running_time_ns, new_run, new_tail_slot,
    park_on_disk, segment_file_name, spawn_uploader, FragmentReady, MediaEnd, PadMediaEnd,
    ParkTarget, SharedDetected, TailFragment,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use strom_types::tams::{
    CONTENT_TYPE_MP4, CONTENT_TYPE_MPEGTS, FORMAT_AUDIO, FORMAT_MULTI, FORMAT_VIDEO,
};
use strom_types::{block::*, PropertyValue, *};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub struct TamsOutputBuilder;

const BLOCK_NAME: &str = "TAMS Output";
/// Block definition id of TAMS Output.
pub const TAMS_OUTPUT_BLOCK_ID: &str = "builtin.tams_output";
/// Segment length property, in milliseconds.
const SEGMENT_DURATION_MS: &str = "segment_duration_ms";
/// Whole-second segment length, replaced by [`SEGMENT_DURATION_MS`]. Flows
/// that still carry it are migrated (see [`migrate_legacy_properties`]).
const LEGACY_SEGMENT_DURATION_SECS: &str = "segment_duration_secs";
const DEFAULT_SEGMENT_MS: u64 = 2000;
/// Shortest segment `segment_duration_ms` accepts. Every segment is one HTTP
/// upload and one TAMS segment registration, so the rate is bounded.
const MIN_SEGMENT_MS: u64 = 200;
const DEFAULT_NUM_VIDEO_TRACKS: usize = 1;
const DEFAULT_NUM_AUDIO_TRACKS: usize = 1;
/// Sanity bound on audio tracks: a flow can carry several audio essences (muxed
/// in MPEG-TS, grouped in MP4), but this guards against a fat-fingered property
/// exploding into thousands of pads/uploaders. Programs rarely exceed a handful.
const MAX_AUDIO_TRACKS: usize = 32;
const DEFAULT_CONTAINER: &str = "mpegts";

/// Segment container format.
#[derive(Clone, Copy, PartialEq)]
enum Container {
    /// Separate single-essence MP4 flows (one per essence). Canonical TAMS model.
    Mp4,
    /// One muxed MPEG-TS flow (NMOS `format:multi`) carrying all essences together.
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

/// Rewrite the legacy `segment_duration_secs` to `segment_duration_ms`.
///
/// - Seconds without milliseconds (or with `segment_duration_ms` 0, which
///   used to mean "use the seconds"): `secs × 1000` ms, at least 1 s as
///   before.
/// - Both, with milliseconds non-zero: milliseconds won, so they stay.
/// - `segment_duration_ms` 0 without seconds: the default 2000 ms.
///
/// The seconds key is removed. Returns a description of the change, or
/// `None` when there was nothing to migrate.
pub fn migrate_legacy_properties(
    properties: &mut HashMap<String, PropertyValue>,
) -> Option<String> {
    let secs = properties.remove(LEGACY_SEGMENT_DURATION_SECS);
    let ms_set = properties.contains_key(SEGMENT_DURATION_MS);
    let ms = prop_u64(properties, SEGMENT_DURATION_MS, 0);
    if ms > 0 {
        return secs.map(|_| {
            format!(
                "dropped {} ({} = {} wins)",
                LEGACY_SEGMENT_DURATION_SECS, SEGMENT_DURATION_MS, ms
            )
        });
    }
    let new_ms = match &secs {
        Some(PropertyValue::UInt(u)) => (*u).max(1).saturating_mul(1000),
        Some(PropertyValue::Int(i)) => (*i).max(1).unsigned_abs().saturating_mul(1000),
        _ if ms_set || secs.is_some() => DEFAULT_SEGMENT_MS,
        _ => return None,
    };
    properties.insert(SEGMENT_DURATION_MS.to_string(), PropertyValue::UInt(new_ms));
    Some(match secs {
        Some(v) => format!(
            "{} = {:?} -> {} = {}",
            LEGACY_SEGMENT_DURATION_SECS, v, SEGMENT_DURATION_MS, new_ms
        ),
        None => format!("{} = 0 -> {}", SEGMENT_DURATION_MS, new_ms),
    })
}

/// Migrate the legacy properties of every TAMS Output in `flow` (see
/// [`migrate_legacy_properties`]), logging each block that changed. Returns
/// whether anything changed.
pub fn migrate_flow(flow: &mut Flow) -> bool {
    let mut changed = false;
    for block in &mut flow.blocks {
        if block.block_definition_id != TAMS_OUTPUT_BLOCK_ID {
            continue;
        }
        if let Some(change) = migrate_legacy_properties(&mut block.properties) {
            info!(
                "Flow '{}', TAMS Output {}: migrated legacy properties: {}",
                flow.name, block.id, change
            );
            changed = true;
        }
    }
    changed
}

/// Segment length in nanoseconds, from `segment_duration_ms` (default
/// [`DEFAULT_SEGMENT_MS`]) clamped to [`MIN_SEGMENT_MS`]. Flows are migrated
/// when they are loaded or saved; a legacy `segment_duration_secs` written by
/// other means is still read through [`migrate_legacy_properties`].
fn segment_duration_ns(props: &HashMap<String, PropertyValue>) -> u64 {
    let ms = if props.contains_key(LEGACY_SEGMENT_DURATION_SECS) {
        let mut migrated = props.clone();
        migrate_legacy_properties(&mut migrated);
        prop_u64(&migrated, SEGMENT_DURATION_MS, DEFAULT_SEGMENT_MS)
    } else {
        prop_u64(props, SEGMENT_DURATION_MS, DEFAULT_SEGMENT_MS)
    };
    ms.max(MIN_SEGMENT_MS).saturating_mul(1_000_000)
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
        // Video is a single track (it decides where segments split); audio can be
        // several, e.g. multiple language/commentary tracks.
        let num_video = prop_usize(properties, "num_video_tracks", DEFAULT_NUM_VIDEO_TRACKS).min(1);
        let num_audio = prop_usize(properties, "num_audio_tracks", DEFAULT_NUM_AUDIO_TRACKS)
            .min(MAX_AUDIO_TRACKS);
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
        for i in 0..num_audio {
            inputs.push(ExternalPad {
                label: Some(format!("A{}", i)),
                name: format!("audio_in_{}", i),
                media_type: MediaType::Audio,
                internal_element_id: format!("audio_input_{}", i),
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
            .or_else(|| strom_types::env::var_opt("STROM_TAMS_GATEWAY_URL"))
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
        let auth = crate::tams::auth::TamsAuth::from_properties(properties, &gateway_url).map_err(
            |e| {
                BlockBuildError::InvalidProperty(format!(
                    "TAMS Output: {} (set the OSC Service ID property if it cannot be \
                     derived from gateway_url)",
                    e
                ))
            },
        )?;
        // OSC mode keys its PAT by the flow id, which is only known in the
        // element-setup closure, so the concrete AuthMethod is made there.
        if let TamsAuth::Osc { service_id } = &auth {
            info!(
                "TAMS Output {}: OSC PAT/SAT auth for service {}",
                instance_id, service_id
            );
        }
        let auth_plan = auth;

        let segment_ns = segment_duration_ns(properties);
        let num_video = prop_usize(properties, "num_video_tracks", DEFAULT_NUM_VIDEO_TRACKS).min(1);
        let num_audio = prop_usize(properties, "num_audio_tracks", DEFAULT_NUM_AUDIO_TRACKS)
            .min(MAX_AUDIO_TRACKS);
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
            // Separate single-essence MP4 flows: one per essence, grouped under a
            // Multi-Flow. Canonical TAMS model (per-essence flows collected via
            // flow_collection), giving downstream consumers elemental access.
            Container::Mp4 => {
                // Members collected by the Multi-Flow: (flow_id, role).
                let mut members: Vec<(String, String)> = Vec::new();
                if num_video >= 1 {
                    let flow_id = derive_id(instance_id, "video");
                    members.push((flow_id.clone(), "video".to_string()));
                    let spec = FlowSpec {
                        flow_id,
                        // Own mono-essence Source. The gateway derives the Multi-Flow's
                        // source_collection by resolving each member flow to its
                        // source_id, so members must NOT share the program source.
                        source_id: derive_id(instance_id, "video-source"),
                        format: FORMAT_VIDEO.to_string(),
                        codec: Some("video/h264".to_string()), // fallback; real codec from caps
                        container: Some(CONTENT_TYPE_MP4.to_string()),
                        // Distinguish the two flows of one recording in flow listings.
                        label: label.as_ref().map(|l| format!("{} (video)", l)),
                        description: description.clone(),
                        tags: tags.clone(),
                        flow_collection: vec![],
                        essence_parameters: Default::default(),
                    };
                    build_flow_chain(
                        instance_id,
                        "video",
                        Container::Mp4,
                        segment_ns,
                        &[Essence::Video],
                        true,
                        gateway_url.clone(),
                        auth_plan.clone(),
                        spec,
                        ctx,
                        &mut elements,
                    )?;
                }
                for i in 0..num_audio {
                    let essence = Essence::Audio(i);
                    let tag = essence.tag(); // "audio-0", "audio-1", ...
                    let flow_id = derive_id(instance_id, &tag);
                    members.push((flow_id.clone(), tag.clone()));
                    let spec = FlowSpec {
                        flow_id,
                        // Own mono-essence Source (see video above).
                        source_id: derive_id(instance_id, &format!("{}-source", tag)),
                        format: FORMAT_AUDIO.to_string(),
                        codec: Some("audio/aac".to_string()), // fallback; real codec from caps
                        container: Some(CONTENT_TYPE_MP4.to_string()),
                        label: label.as_ref().map(|l| format!("{} ({})", l, tag)),
                        description: description.clone(),
                        tags: tags.clone(),
                        flow_collection: vec![],
                        essence_parameters: Default::default(),
                    };
                    build_flow_chain(
                        instance_id,
                        &tag,
                        Container::Mp4,
                        segment_ns,
                        &[essence],
                        true,
                        gateway_url.clone(),
                        auth_plan.clone(),
                        spec,
                        ctx,
                        &mut elements,
                    )?;
                }
                // Group the per-essence flows under one NMOS format:multi Multi-Flow.
                // It carries no essence of its own — segments live on the member
                // flows — so it is registered once at startup, not via an uploader.
                if members.len() >= 2 {
                    let multi = FlowSpec {
                        flow_id: derive_id(instance_id, "mux"),
                        source_id: source_id.clone(),
                        format: FORMAT_MULTI.to_string(),
                        // The Multi-Flow carries no segments of its own — its members
                        // do — but the gateway requires codec/container on every flow.
                        // Use the canonical mux media type (video/mp2t per BCP-006-04):
                        // it describes the multiplex these essences reconstitute.
                        codec: Some(CONTENT_TYPE_MPEGTS.to_string()),
                        container: Some(CONTENT_TYPE_MPEGTS.to_string()),
                        label: label.clone(),
                        description: description.clone(),
                        tags: tags.clone(),
                        flow_collection: members,
                        essence_parameters: Default::default(),
                    };
                    register_multiflow(instance_id, multi, gateway_url.clone(), auth_plan, ctx);
                }
            }
            // One muxed MPEG-TS flow carrying all essences together.
            Container::MpegTs => {
                let mut inputs = Vec::new();
                if num_video >= 1 {
                    inputs.push(Essence::Video);
                }
                for i in 0..num_audio {
                    inputs.push(Essence::Audio(i));
                }
                let spec = FlowSpec {
                    flow_id: derive_id(instance_id, "mux"),
                    source_id: source_id.clone(),
                    format: FORMAT_MULTI.to_string(),
                    codec: Some(CONTENT_TYPE_MPEGTS.to_string()),
                    container: Some(CONTENT_TYPE_MPEGTS.to_string()),
                    label: label.clone(),
                    description: description.clone(),
                    tags: tags.clone(),
                    // The TS is an opaque mux; its inner essences are NOT exposed as
                    // separate flows (per AMWA BCP-006-04), so no flow_collection.
                    flow_collection: vec![],
                    essence_parameters: Default::default(),
                };
                build_flow_chain(
                    instance_id,
                    "mux",
                    Container::MpegTs,
                    segment_ns,
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
            "TAMS Output {}: built ({} video, {} audio, {}), gateway {}, segment {} ms",
            instance_id,
            num_video,
            num_audio,
            container.content_type(),
            gateway_url,
            segment_ns / 1_000_000
        );

        Ok(BlockBuildResult {
            elements,
            internal_links: vec![],
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// One essence input on a flow. Audio carries its track index so a flow can mux
/// (MPEG-TS) or group (MP4) several audio tracks. Video is always a single
/// track: it decides where segments split.
#[derive(Clone, Copy, PartialEq)]
enum Essence {
    Video,
    Audio(usize),
}

impl Essence {
    /// Internal `identity` input element id, matching the external pad mapping
    /// (`video_in_0` -> `video_input_0`, `audio_in_{i}` -> `audio_input_{i}`).
    fn input_id(&self) -> String {
        match self {
            Essence::Video => "video_input_0".to_string(),
            Essence::Audio(i) => format!("audio_input_{}", i),
        }
    }
    /// Short unique tag for element/temp-dir naming and the collection role
    /// (`video`, `audio-0`, `audio-1`, ...).
    fn tag(&self) -> String {
        match self {
            Essence::Video => "video".to_string(),
            Essence::Audio(i) => format!("audio-{}", i),
        }
    }
}

/// Build one TAMS flow chain: N identity inputs -> (dynamic parsers) -> appsinks
/// -> segmenter (one muxer per fragment) -> segment files. For MP4 each chain has a single essence
/// (one flow per essence); for MPEG-TS a single chain muxes all essences into one
/// `format:multi` flow. Wires the per-fragment timerange capture + uploader at start.
///
/// `suffix` distinguishes the temp dir / element names (`video`/`audio`/`mux`).
/// `detect_codec` controls whether the caps probe overrides the flow's codec label
/// (true for single-essence MP4; false for muxed TS, where the codec is the container).
#[allow(clippy::too_many_arguments)]
fn build_flow_chain(
    instance_id: &str,
    suffix: &str,
    container: Container,
    segment_ns: u64,
    inputs: &[Essence],
    detect_codec: bool,
    gateway_url: String,
    auth_plan: TamsAuth,
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

    // Each essence ends in an appsink; the segmenter cuts the streams into
    // fragment files on the TAI grid and closes each file as soon as the
    // first frame after it has arrived (see `crate::tams::segmenter`, and why
    // not `splitmuxsink` there). The appsinks neither sync nor preroll, so an
    // input that carries no data never holds the pipeline short of PLAYING.
    let mux_caps = muxer_sink_caps(container.muxer_factory())?;
    let mut sinks: Vec<gst_app::AppSink> = Vec::with_capacity(inputs.len());
    for &essence in inputs {
        let sink_id = format!("{}:{}_{}_sink", instance_id, suffix, essence.tag());
        let sink = gst::ElementFactory::make("appsink")
            .name(&sink_id)
            .property("sync", false)
            .property("async", false)
            .property("qos", false)
            .property("enable-last-sample", false)
            .property("max-buffers", 0u32)
            // What the fragment muxer takes, so that the parser converts to it
            // (mpegtsmux takes H.264 only as byte-stream).
            .property("caps", &mux_caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("appsink: {}", e)))?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| BlockBuildError::ElementCreation("appsink: wrong type".into()))?;
        elements.push((sink_id, sink.clone().upcast()));
        sinks.push(sink);
    }

    // Codec label for the TAMS flow, derived from caps at runtime by the probes and
    // read by the uploader when it creates the flow.
    let detected_codec: SharedDetected = Arc::default();
    // Where the media written to the open fragment ends, per essence: for the
    // timerange of the last fragment of a run (no next fragment closes it),
    // and, read when a fragment closes, for a fragment before a stall.
    let pad_ends: Vec<Arc<PadMediaEnd>> = inputs
        .iter()
        .map(|_| Arc::new(PadMediaEnd::default()))
        .collect();
    let media_end: MediaEnd = Arc::new(Mutex::new(pad_ends.clone()));
    let media_end_at_split = media_end.clone();
    // On a stop, the open fragment is finished before the pipeline goes down.
    let segmenter_slot: Arc<Mutex<Option<std::sync::Weak<Segmenter>>>> = Arc::default();
    {
        let slot = segmenter_slot.clone();
        ctx.register_stop_drain(Box::new(move || {
            let seg = slot
                .lock()
                .ok()
                .and_then(|s| s.as_ref().and_then(|w| w.upgrade()));
            if let Some(seg) = seg {
                seg.drain();
            }
        }));
    }

    // One identity input + caps probe per essence, each requesting its own sink pad.
    for (essence_idx, &essence) in inputs.iter().enumerate() {
        let input_id = format!("{}:{}", instance_id, essence.input_id());
        let input = gst::ElementFactory::make("identity")
            .name(&input_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("identity: {}", e)))?;
        let src_pad = input
            .static_pad("src")
            .ok_or_else(|| BlockBuildError::ElementCreation("identity has no src pad".into()))?;

        let parser_inserted = Arc::new(AtomicBool::new(false));
        let sink_weak = sinks[essence_idx].downgrade();
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
            // One-shot: only short-circuit once a parser has actually been linked.
            // Caps events are serialized on this pad's streaming thread, so a plain
            // load/store needs no CAS. Crucially we do NOT set the flag here — a
            // first Caps event that is rejected (raw/unsupported) or whose parser
            // insertion fails must not permanently disable a later, valid Caps event.
            let caps = match event.view() {
                gst::EventView::Caps(c) => c.caps().to_owned(),
                _ => return gst::PadProbeReturn::Ok,
            };
            let structure = match caps.structure(0) {
                Some(s) => s,
                None => return gst::PadProbeReturn::Ok,
            };
            if parser_inserted.load(Ordering::SeqCst) {
                // Renegotiation: keep the flow's video parameters current.
                if essence == Essence::Video {
                    merge_video_params(&detected_codec_probe, structure, &instance_id_owned);
                }
                return gst::PadProbeReturn::Ok;
            }
            let caps_name = structure.name().to_string();

            // (GStreamer parser factory, TAMS codec label) derived from caps.
            let (parser_factory, codec_label) = match essence {
                Essence::Video => match caps_name.as_str() {
                    "video/x-h264" => ("h264parse", "video/h264"),
                    "video/x-h265" => ("h265parse", "video/h265"),
                    other => {
                        if let Some(input) = pad.parent_element() {
                            refuse_input(
                                &input,
                                &video_refusal(BLOCK_NAME, "H.264 or H.265", other),
                            );
                        }
                        return gst::PadProbeReturn::Ok;
                    }
                },
                Essence::Audio(_) => {
                    let mpegversion = structure.get::<i32>("mpegversion").unwrap_or(0);
                    match caps_name.as_str() {
                        "audio/mpeg" if mpegversion == 1 => ("mpegaudioparse", "audio/mpeg"),
                        "audio/mpeg" => ("aacparse", "audio/aac"),
                        "audio/x-opus" => ("opusparse", "audio/opus"),
                        "audio/x-ac3" => ("ac3parse", "audio/ac3"),
                        other => {
                            if let Some(input) = pad.parent_element() {
                                refuse_input(
                                    &input,
                                    &audio_refusal(BLOCK_NAME, "MP3, AAC, Opus or AC-3", other),
                                );
                            }
                            return gst::PadProbeReturn::Ok;
                        }
                    }
                }
            };

            if let Ok(mut d) = detected_codec_probe.lock() {
                // For single-essence flows, record the detected codec as the flow's codec.
                if detect_codec {
                    d.codec = Some(codec_label.to_string());
                }
            }
            // Video geometry and rate go on the flow (also a muxed TS flow), so
            // a reader can map frames to the timeline without probing segments.
            if essence == Essence::Video {
                merge_video_params(&detected_codec_probe, structure, &instance_id_owned);
            }

            let sink = match sink_weak.upgrade() {
                Some(e) => e,
                None => return gst::PadProbeReturn::Ok,
            };
            let bin = match sink.parent().and_then(|p| p.downcast::<gst::Bin>().ok()) {
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
            let sink_pad = match sink.static_pad("sink") {
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
            // The parser's output caps carry what the stream itself says (the
            // SPS: size, and the rate when it has VUI timing); its timestamps
            // give the rate when nothing else does.
            if essence == Essence::Video {
                watch_video_essence(&psrc, detected_codec_probe.clone(), &instance_id_owned);
            }
            // Linked successfully — now consume the one-shot guard so subsequent
            // Caps events (renegotiation) don't re-insert a second parser.
            parser_inserted.store(true, Ordering::SeqCst);
            // A refused first Caps event silenced this input; this one was usable.
            if let Some(input) = pad.parent_element() {
                accept_input(&input);
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

    // Wire timerange capture + uploader at pipeline start (flow_id + events available).
    let block_id = instance_id.to_string();
    let temp_dir_for_setup = temp_dir.clone();
    let chain_tag = suffix.to_string();
    let content_type = container.content_type().to_string();
    let file_ext = container.file_ext().to_string();
    let suffix_owned = suffix.to_string();
    let mux_factory = container.muxer_factory();
    ctx.register_element_setup(Box::new(move |flow_id, events| {
        // Remove orphan segment files left by a previous run (no `.meta`
        // sidecar, so their timerange is lost). Parked segments are kept for the
        // uploader's recovery sweep, even across a full restart, and so are the
        // files of a previous run whose uploader still runs.
        let removed = clear_orphans(&temp_dir_for_setup);
        if removed > 0 {
            info!(
                "TAMS {}: cleared {} orphan segment(s) from a previous run",
                block_id, removed
            );
        }

        // OSC auth keys its PAT by the flow id (tenant isolation on a shared
        // instance), which is only known here — so finalize the gateway client now.
        let credential_key = flow_id.to_string();
        if let TamsAuth::Osc { .. } = &auth_plan {
            if !crate::osc::sat_provider().has_pat_for(&credential_key) {
                warn!(
                    "TAMS {}: OSC mode but no PAT for flow {} yet — uploads will fail \
                     until one is configured (STROM_OSC_PAT / OSC_ACCESS_TOKEN, or push \
                     via PUT /api/osc/pat/{})",
                    block_id, credential_key, credential_key
                );
            }
        }
        let client = match TamsClient::new(&gateway_url, auth_plan.method(&credential_key)) {
            Ok(c) => c,
            Err(e) => {
                error!("TAMS {}: failed to build gateway client: {:#}", block_id, e);
                return;
            }
        };

        let (tx, rx) = channel();
        // Holds the currently-open (not-yet-rotated) fragment; flushed on shutdown.
        let tail = new_tail_slot();
        // Segment files of this run carry its id, so they never overwrite what an
        // earlier run parked in the same directory.
        let run = new_run();
        let run_id = run.id().to_string();
        // What a segment parked from the streaming side is meant for.
        let park_target = ParkTarget::new(client.base_url(), &spec.flow_id);
        spawn_uploader(
            client,
            spec,
            detected_codec,
            content_type,
            temp_dir_for_setup.clone(),
            flow_id,
            block_id.clone(),
            events,
            rx,
            tail.clone(),
            media_end,
            segment_ns,
            run,
        );

        // Per-block timeline: TAI = running time + offset. A fragment whose first
        // buffer carries a reference timestamp starts at that capture time and
        // re-anchors the timeline on it. Otherwise the timeline is anchored on the
        // first fragment by the running time -> TAI mapping shared by
        // every TAMS Output of this pipeline run, so equal running time gives equal
        // TAI across blocks (#966). See `crate::tams::tai_mapping`.
        let timeline = Arc::new(Mutex::new(FragmentTimeline::default()));
        let tai = Arc::new(HostTai::new());
        let temp_dir = temp_dir_for_setup;
        let tag = chain_tag;
        let block_id_sig = block_id.clone();
        // Fragments that started off the grid although their keyframe was
        // requested in time (see `warn_off_grid`).
        let off_grid = AtomicU64::new(0);
        // Running time of the last grid point a keyframe was requested for.
        let requested_until: Mutex<Option<u64>> = Mutex::new(None);

        let on_open = move |req: OpenRequest<'_>| -> OpenDecision {
            let element = Some(req.element.clone());
            let fragment_id = req.fragment_id;
            let buffer = req.sample.buffer_owned();
            // Running time of the fragment's first buffer.
            let running_time_ns = req.running_time_ns;

            // Where the closing fragment's media ends: the new fragment's
            // buffers have not entered the muxer yet.
            let closing_media_end = media_end_running_time_ns(&media_end_at_split);

            let location = temp_dir.join(segment_file_name(&run_id, fragment_id, &file_ext));
            let location_str = location.to_string_lossy().to_string();

            // Capture time of the fragment's first buffer, when it carries one.
            let reference_ns = buffer
                .as_ref()
                .and_then(|b| reference_tai_ns(b.as_ref(), tai.tai_minus_utc_ns()));
            // One frame: a capture time this close to where running time puts
            // the buffer continues the previous fragment.
            let frame_ns = buffer
                .as_ref()
                .and_then(|b| b.duration())
                .map_or(50_000_000, |d| d.nseconds() + 1_000_000);
            // The shared mapping is asked for every fragment without a capture
            // time, so a jump (the pipeline clock stopped during a sleep) is
            // noticed and the timeline re-anchors on the re-sampled mapping.
            let pipeline = if reference_ns.is_none() {
                element.as_ref().and_then(|el| {
                    let (clock, base) = (el.clock()?, el.base_time()?.nseconds());
                    Some((shared_pipeline_mapping(flow_id, &clock, base, &tai), base))
                })
            } else {
                None
            };
            let placed = {
                let mut tl = timeline.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((m, _)) = pipeline {
                    tl.note_pipeline_epoch(m.epoch);
                }
                tl.place(
                    running_time_ns,
                    segment_ns,
                    reference_ns,
                    frame_ns,
                    || match pipeline {
                        Some((m, base)) => (m.tai_ns(base, running_time_ns), AnchorKind::Pipeline),
                        None => (tai.now_ns(), AnchorKind::Arrival),
                    },
                )
            };
            if placed.went_backward {
                warn!(
                    "TAMS {} ({}): running time went backward (to {} ns), re-anchored \
                     ({:?}) at TAI {} ns",
                    block_id_sig,
                    tag,
                    running_time_ns,
                    placed.anchored.unwrap_or(AnchorKind::Arrival),
                    placed.start_ns
                );
                if let Some(by) = placed.clamped_by_ns {
                    warn!(
                        "TAMS {} ({}): moved the segment start {} ms later so it does not \
                         overlap the previous segment's planned end",
                        block_id_sig,
                        tag,
                        by / 1_000_000
                    );
                }
            } else if placed.jumped {
                warn!(
                    "TAMS {} ({}): pipeline clock jumped against TAI (sleep or clock step), \
                     re-anchored at TAI {} ns",
                    block_id_sig, tag, placed.start_ns
                );
            } else if let Some(kind) = placed.anchored {
                info!(
                    "TAMS {} ({}): timeline anchored ({:?}): running time {} ns -> TAI {} ns",
                    block_id_sig, tag, kind, running_time_ns, placed.start_ns
                );
            }
            let abs_start = placed.start_ns;
            if placed.since_anchor >= FIRST_ON_GRID_FRAGMENT {
                // Splits land on the first keyframe at or after the grid point,
                // so up to one frame past it is on the grid.
                if let Some(miss) = grid_miss_ns(abs_start, segment_ns, frame_ns) {
                    warn_off_grid(&block_id_sig, &tag, &off_grid, miss);
                }
            }

            // Schedule the split that ends this fragment on the next grid point.
            let next_split = timeline
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .next_split_running_time(running_time_ns, segment_ns);
            if let (Some(next), Some(el)) = (next_split, element.as_ref()) {
                // The segmenter splits at `next`, on the first keyframe at or
                // after it. Ask upstream for keyframes on the grid points after `next`, up
                // to `KEYFRAME_LEAD_NS` ahead. The encoder's input runs ahead of
                // its output (where this fragment opens) by its latency, which
                // can be most of a segment, so the encoder is often past `next`
                // already; a request for a time already passed would force a
                // keyframe off the grid. The split at `next` itself was requested
                // by an earlier fragment.
                let ahead = {
                    let mut until = requested_until.lock().unwrap_or_else(|e| e.into_inner());
                    let (points, last) = keyframe_request_points(next, segment_ns, *until);
                    *until = Some(last);
                    points
                };
                // Sent from GStreamer's thread pool, not from this streaming
                // thread with the segmenter locked.
                el.call_async(move |el| {
                    for rt in &ahead {
                        let rt = gst::ClockTime::from_nseconds(*rt);
                        for pad in el.sink_pads() {
                            let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
                                .running_time(rt)
                                .all_headers(true)
                                .build();
                            pad.push_event(event);
                        }
                    }
                });
            }

            // The previously open fragment ends here: [its start, this start),
            // or [its start, its planned end) after a backward jump. It is
            // uploaded once the segmenter has finished its file.
            // The just-opened one becomes the new tail (flushed on shutdown).
            let mut prev_ready = None;
            if let Ok(mut tail) = tail.lock() {
                if let Some(prev) = tail.take() {
                    let end_ns = closed_end_ns(
                        &prev,
                        placed.prev_end_ns,
                        closing_media_end,
                        running_time_ns,
                        frame_ns,
                    );
                    if end_ns < placed.prev_end_ns {
                        info!(
                            "TAMS {} ({}): the input stalled; the segment before it ends \
                             at its media, {} ms before the next starts",
                            block_id_sig,
                            tag,
                            (placed.prev_end_ns - end_ns) / 1_000_000
                        );
                    }
                    prev_ready = Some(FragmentReady {
                        path: prev.path,
                        start_ns: prev.start_ns,
                        end_ns: end_ns.max(prev.start_ns + 1),
                    });
                }
                *tail = Some(TailFragment {
                    path: location.clone(),
                    start_ns: abs_start,
                    start_running_time_ns: running_time_ns,
                });
            }

            debug!(
                "TAMS {} ({}): opening segment {}",
                block_id_sig, tag, location_str
            );
            OpenDecision {
                location,
                prev_ready,
                next_split_ns: next_split,
            }
        };

        // Hands a finished fragment to the uploader. When its channel is full
        // (or closed), the fragment is parked on disk instead, for the recovery
        // sweep of this run or the next; only when that fails too (the disk is
        // full) is it dropped.
        let deliver_label = format!("{} ({})", block_id, suffix_owned);
        let overflowed = AtomicU64::new(0);
        let deliver = move |ready: FragmentReady| {
            let Err(e) = tx.try_send(ready) else {
                return;
            };
            let full = matches!(e, tokio::sync::mpsc::error::TrySendError::Full(_));
            let ready = e.into_inner();
            match park_on_disk(&ready, &park_target) {
                Ok(()) => {
                    let n = overflowed.fetch_add(1, Ordering::Relaxed);
                    if n.is_multiple_of(100) {
                        warn!(
                            "TAMS {}: uploader {}, parked segment {} on disk for the \
                             recovery sweep ({} so far)",
                            deliver_label,
                            if full { "queue full" } else { "stopped" },
                            ready.path.display(),
                            n + 1
                        );
                    }
                }
                Err(err) => {
                    error!(
                        "TAMS {}: uploader queue full and segment {} could not be parked \
                         ({}); dropping it",
                        deliver_label,
                        ready.path.display(),
                        err
                    );
                    let _ = std::fs::remove_file(&ready.path);
                }
            }
        };

        let segmenter = Segmenter::new(
            format!("{} ({})", block_id, suffix_owned),
            mux_factory,
            container == Container::MpegTs,
            sinks.len(),
            0, // video comes first when there is one
            pad_ends,
            Box::new(on_open),
            Box::new(deliver),
        );
        if let Ok(mut slot) = segmenter_slot.lock() {
            *slot = Some(Arc::downgrade(&segmenter));
        }
        for (idx, sink) in sinks.iter().enumerate() {
            let on_sample = segmenter.clone();
            let on_eos = segmenter.clone();
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        on_sample.push(idx, sample, sink.upcast_ref());
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .eos(move |_| on_eos.eos(idx))
                    .build(),
            );
        }
    }));

    Ok(())
}

/// Everything the sink pads of `factory` accept, for the appsinks in front
/// of the fragment muxer.
fn muxer_sink_caps(factory: &str) -> Result<gst::Caps, BlockBuildError> {
    let f = gst::ElementFactory::find(factory)
        .ok_or_else(|| BlockBuildError::ElementCreation(format!("{} not found", factory)))?;
    let mut caps = gst::Caps::new_empty();
    for tmpl in f.static_pad_templates() {
        if tmpl.direction() == gst::PadDirection::Sink {
            caps.merge(tmpl.caps());
        }
    }
    Ok(caps)
}

/// How far ahead of a fragment's next split TAMS Output asks upstream for
/// keyframes on the grid. It must exceed the upstream encoder's latency (x264
/// with lookahead holds most of a second), or the request comes after the
/// encoder has passed that point and forces a keyframe off the grid. With 1 s
/// segments this is one grid point ahead, as before sub-second segments.
const KEYFRAME_LEAD_NS: u64 = 1_000_000_000;

/// Grid points (running time) to ask upstream for keyframes at when a fragment
/// whose next split is `next` opens: every grid point after `next` up to
/// `KEYFRAME_LEAD_NS` past it (at least one), skipping those already requested
/// (`requested_until`, the last one asked for). Returns the points and the new
/// `requested_until`. After a gap or a backward jump it starts over from the
/// grid point after `next`.
fn keyframe_request_points(
    next: u64,
    segment_ns: u64,
    requested_until: Option<u64>,
) -> (Vec<u64>, u64) {
    let segment_ns = segment_ns.max(1);
    let lead = KEYFRAME_LEAD_NS.div_ceil(segment_ns).max(1);
    let last = next.saturating_add(lead.saturating_mul(segment_ns));
    let first_new = next.saturating_add(segment_ns);
    let first = match requested_until {
        Some(r) if r >= first_new && r <= last && (r - next).is_multiple_of(segment_ns) => {
            r + segment_ns
        }
        _ => first_new,
    };
    let points = (0..)
        .map(|i: u64| first.saturating_add(i * segment_ns))
        .take_while(|p| *p <= last)
        .collect();
    (points, last)
}

/// Report a segment that started `miss_ns` past its TAI grid point although its
/// keyframe was requested one fragment ahead: the encoder did not honour the
/// request. Boundaries then follow the encoder's own keyframes, off the grid and
/// differently on each angle. Logged on the first miss and every 30th after.
fn warn_off_grid(block_id: &str, tag: &str, count: &AtomicU64, miss_ns: u64) {
    let n = count.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n.is_multiple_of(30) {
        warn!(
            "TAMS {} ({}): segment boundary {} ms past the TAI grid ({} so far): the \
             upstream encoder did not make the keyframe requested for it. Check that it \
             honours force-keyunit requests on time: a Video Encoder block applies \
             min_keyframe_request_interval_ms only to immediate requests, but other \
             encoders may drop requests (e.g. min-force-key-unit-interval) or need a GOP \
             that divides the segment duration.",
            block_id,
            tag,
            miss_ns / 1_000_000,
            n
        );
    } else {
        debug!(
            "TAMS {} ({}): segment boundary {} ms past the TAI grid",
            block_id,
            tag,
            miss_ns / 1_000_000
        );
    }
}

/// Register a metadata-only Multi-Flow that groups the per-essence flows via
/// `flow_collection` (the canonical TAMS multi-essence model). It carries no
/// segments of its own, so unlike the essence flows it is not created lazily by
/// an uploader — it is PUT (and re-PUT) on a short schedule at pipeline start so
/// the gateway can derive its source_collection once the member flows exist.
/// Best-effort: a failure is logged but never stops the recording, since the
/// essence flows record independently of the grouping.
fn register_multiflow(
    instance_id: &str,
    spec: FlowSpec,
    gateway_url: String,
    auth_plan: TamsAuth,
    ctx: &BlockBuildContext,
) {
    let block_id = instance_id.to_string();
    ctx.register_element_setup(Box::new(move |flow_id, _events| {
        // OSC auth keys its PAT by the flow id, matching the essence uploaders.
        let credential_key = flow_id.to_string();
        let client = match TamsClient::new(&gateway_url, auth_plan.method(&credential_key)) {
            Ok(c) => c,
            Err(e) => {
                error!(
                    "TAMS {}: failed to build gateway client for multi-flow: {:#}",
                    block_id, e
                );
                return;
            }
        };
        tokio::spawn(async move {
            let members = spec.flow_collection.len();
            // The gateway derives the multi-Source's source_collection from the
            // members' source_ids at PUT time, and does NOT re-derive when members
            // are registered later. Member flows are created lazily by their
            // uploaders on the first segment (a few seconds in), so re-PUT the
            // (idempotent) multi-flow on a short schedule — a PUT after the members
            // exist resolves them into source_collection. Cumulative PUTs at
            // ~0/3/11/31s cover both fast and slow source startup.
            const SCHEDULE_SECS: [u64; 4] = [0, 3, 8, 20];
            let mut registered = false;
            for (i, delay) in SCHEDULE_SECS.iter().enumerate() {
                if *delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(*delay)).await;
                }
                match client.ensure_flow(&spec).await {
                    Ok(()) => {
                        if !registered {
                            registered = true;
                            info!(
                                "TAMS {}: multi-flow {} registered (collects {} essence flows); \
                                 re-PUT scheduled so the gateway can resolve members for source_collection",
                                block_id, spec.flow_id, members
                            );
                        }
                    }
                    Err(e) => warn!(
                        "TAMS {}: multi-flow {} PUT failed (attempt {}): {:#}",
                        block_id,
                        spec.flow_id,
                        i + 1,
                        e
                    ),
                }
            }
            if !registered {
                error!(
                    "TAMS {}: multi-flow {} was never registered after {} attempts",
                    block_id,
                    spec.flow_id,
                    SCHEDULE_SECS.len()
                );
            }
        });
    }));
}

/// Merge the video parameters in `caps` into the detected essence. A key the
/// caps lack keeps its value (a `0/1` rate does not erase a known one).
fn merge_video_params(detected: &SharedDetected, caps: &gst::StructureRef, block_id: &str) {
    let params = video_essence_parameters(caps);
    let Ok(mut d) = detected.lock() else {
        return;
    };
    for (k, v) in params {
        if d.essence_parameters.get(&k) != Some(&v) {
            debug!("TAMS Output {}: video {} = {}", block_id, k, v);
            d.essence_parameters.insert(k, v);
        }
    }
}

/// Timestamps collected before giving up on estimating a frame rate.
const RATE_SAMPLES_MAX: usize = 150;
/// Timestamps an estimate is first tried from.
const RATE_SAMPLES_MIN: usize = 30;

/// Watch the parser's output for the flow's video parameters: its caps, and,
/// while the caps give no frame rate, the buffer timestamps, from which the
/// rate is estimated ([`crate::gst::frame_rate::estimate_frame_rate`]). A
/// stream whose SPS has no timing (VideoToolbox) has `framerate=0/1` after a
/// transport hop, but its timestamps still advance one frame at a time.
fn watch_video_essence(pad: &gst::Pad, detected: SharedDetected, block_id: &str) {
    let block_id = block_id.to_string();
    // Timestamps seen while no rate is known; `None` once estimating gave up.
    let samples: Mutex<Option<Vec<u64>>> = Mutex::new(Some(Vec::new()));
    pad.add_probe(
        gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::BUFFER,
        move |_pad, info| {
            match info.data.as_ref() {
                Some(gst::PadProbeData::Event(e)) => {
                    if let gst::EventView::Caps(c) = e.view() {
                        if let Some(s) = c.caps().structure(0) {
                            merge_video_params(&detected, s, &block_id);
                        }
                    }
                }
                Some(gst::PadProbeData::Buffer(buf)) => {
                    let Ok(mut samples) = samples.lock() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let Some(pts) = samples.as_mut() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let has_rate = detected
                        .lock()
                        .map(|d| d.essence_parameters.contains_key("frame_rate"))
                        .unwrap_or(true);
                    if has_rate {
                        pts.clear();
                        return gst::PadProbeReturn::Ok;
                    }
                    let Some(t) = buf.pts() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    pts.push(t.nseconds());
                    if pts.len() < RATE_SAMPLES_MIN {
                        return gst::PadProbeReturn::Ok;
                    }
                    match crate::gst::frame_rate::estimate_frame_rate(pts.iter().copied()) {
                        Some((n, d)) => {
                            info!(
                                "TAMS Output {}: stream has no frame rate, {}/{} estimated \
                                 from timestamps",
                                block_id, n, d
                            );
                            if let Ok(mut det) = detected.lock() {
                                det.essence_parameters.insert(
                                    "frame_rate".into(),
                                    serde_json::json!({ "numerator": n, "denominator": d }),
                                );
                            }
                            pts.clear();
                        }
                        None if pts.len() >= RATE_SAMPLES_MAX => {
                            warn!(
                                "TAMS Output {}: stream has no frame rate and none could be \
                                 estimated from {} timestamps; the flow has no frame_rate",
                                block_id,
                                pts.len()
                            );
                            *samples = None;
                        }
                        None => {}
                    }
                }
                _ => {}
            }
            gst::PadProbeReturn::Ok
        },
    );
}

/// TAMS video `essence_parameters` from encoded video caps: `frame_rate`
/// (`{numerator, denominator}`), `frame_width` and `frame_height`, as far as the
/// caps carry them. A variable frame rate (`0/1`) is left out.
fn video_essence_parameters(
    caps: &gst::StructureRef,
) -> serde_json::Map<String, serde_json::Value> {
    let mut params = serde_json::Map::new();
    if let Ok(fr) = caps.get::<gst::Fraction>("framerate") {
        if fr.numer() > 0 && fr.denom() > 0 {
            params.insert(
                "frame_rate".into(),
                serde_json::json!({ "numerator": fr.numer(), "denominator": fr.denom() }),
            );
        }
    }
    for (field, key) in [("width", "frame_width"), ("height", "frame_height")] {
        if let Ok(v) = caps.get::<i32>(field) {
            params.insert(key.into(), v.into());
        }
    }
    params
}

/// Get TAMS Output block definitions.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![tams_output_definition()]
}

fn tams_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: TAMS_OUTPUT_BLOCK_ID.to_string(),
        name: BLOCK_NAME.to_string(),
        description: "Records pre-encoded video/audio into a TAMS store (Time-Addressable Media Store) via an Eyevinn TAMS Gateway. Either separate single-essence MP4 flows or one muxed MPEG-TS flow.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: [
            vec![
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
                description: "Segment container. MP4 = separate single-essence flows (one per essence). MPEG-TS = one muxed flow (video+audio together, NMOS format:multi).".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "mpegts".to_string(),
                            label: Some("MPEG-TS (muxed)".to_string()),
                            visible_when: None,
                        },
                        EnumValue {
                            value: "mp4".to_string(),
                            label: Some("MP4 (separate flows)".to_string()),
                            visible_when: None,
                        },
                    ],
                },
                default_value: Some(PropertyValue::String(DEFAULT_CONTAINER.to_string())),
                mapping: block_mapping("container"),
                live: false,
                persist: None,
                visible_when: None,
            },
            ],
            // Shared with the Media Player's timeline mode.
            crate::tams::auth::auth_properties(None),
            vec![
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
                "Number of audio tracks (0 or more; muxed into one MPEG-TS flow, or one MP4 flow each).",
                PropertyType::Int,
                PropertyValue::Int(1),
            ),
            simple_prop(
                SEGMENT_DURATION_MS,
                "Segment Duration (ms)",
                "Target duration of each TAMS segment in milliseconds (at least 200). Boundaries stay on the TAI grid (multiples of this length from the TAI epoch), and segments split on a keyframe (one is requested from upstream encoders at each split), so the GOP should divide the segment length. For replay, 500 ms segments with a GOP of 500 ms or a divisor of it.",
                PropertyType::Int,
                PropertyValue::Int(DEFAULT_SEGMENT_MS as i64),
            ),
            simple_prop(
                "tams_source_id",
                "TAMS Source ID",
                "Optional source UUID. Leave empty to derive a stable one from this block.",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
            ],
        ]
        .concat(),
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
        visible_when: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframes_requested_a_second_ahead_once_each() {
        const MS: u64 = 1_000_000;
        // 1 s segments: one grid point ahead, as before.
        assert_eq!(
            keyframe_request_points(10_000 * MS, 1000 * MS, None),
            (vec![11_000 * MS], 11_000 * MS)
        );
        assert_eq!(
            keyframe_request_points(11_000 * MS, 1000 * MS, Some(11_000 * MS)),
            (vec![12_000 * MS], 12_000 * MS)
        );
        // 500 ms: the first fragment asks for both points within the second,
        // the next one only for the new point.
        assert_eq!(
            keyframe_request_points(10_000 * MS, 500 * MS, None),
            (vec![10_500 * MS, 11_000 * MS], 11_000 * MS)
        );
        assert_eq!(
            keyframe_request_points(10_500 * MS, 500 * MS, Some(11_000 * MS)),
            (vec![11_500 * MS], 11_500 * MS)
        );
        // A fragment that spans grid points (a missed keyframe) leaves a gap:
        // start over after `next`.
        assert_eq!(
            keyframe_request_points(12_500 * MS, 500 * MS, Some(11_500 * MS)),
            (vec![13_000 * MS, 13_500 * MS], 13_500 * MS)
        );
        // A backward jump in running time starts over too.
        assert_eq!(
            keyframe_request_points(1_000 * MS, 500 * MS, Some(11_500 * MS)),
            (vec![1_500 * MS, 2_000 * MS], 2_000 * MS)
        );
        // 200 ms: five points within the second.
        assert_eq!(keyframe_request_points(0, 200 * MS, None).0.len(), 5);
    }

    #[test]
    fn segment_length_from_seconds_or_milliseconds() {
        let props = |pairs: &[(&str, PropertyValue)]| -> HashMap<String, PropertyValue> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect()
        };
        const MS: u64 = 1_000_000;
        // Unset: the default.
        assert_eq!(segment_duration_ns(&props(&[])), DEFAULT_SEGMENT_MS * MS);
        assert_eq!(
            segment_duration_ns(&props(&[("segment_duration_ms", PropertyValue::Int(500))])),
            500 * MS
        );
        // Clamped to the minimum.
        assert_eq!(
            segment_duration_ns(&props(&[("segment_duration_ms", PropertyValue::Int(40))])),
            MIN_SEGMENT_MS * MS
        );
        // A legacy seconds value not yet migrated is still read.
        assert_eq!(
            segment_duration_ns(&props(&[("segment_duration_secs", PropertyValue::Int(1))])),
            1000 * MS
        );
    }

    #[test]
    fn migrate_legacy_segment_duration() {
        let props = |pairs: &[(&str, PropertyValue)]| -> HashMap<String, PropertyValue> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect()
        };
        let migrated = |pairs: &[(&str, PropertyValue)]| {
            let mut p = props(pairs);
            let change = migrate_legacy_properties(&mut p);
            assert!(!p.contains_key("segment_duration_secs"));
            let ms = p
                .contains_key("segment_duration_ms")
                .then(|| prop_u64(&p, "segment_duration_ms", u64::MAX));
            (change.is_some(), ms)
        };
        // Seconds only: × 1000.
        assert_eq!(
            migrated(&[("segment_duration_secs", PropertyValue::Int(3))]),
            (true, Some(3000))
        );
        // Seconds with the old "unset" 0 ms: × 1000, at least 1 s as before.
        assert_eq!(
            migrated(&[
                ("segment_duration_secs", PropertyValue::UInt(0)),
                ("segment_duration_ms", PropertyValue::Int(0)),
            ]),
            (true, Some(1000))
        );
        // Both set: milliseconds win.
        assert_eq!(
            migrated(&[
                ("segment_duration_secs", PropertyValue::Int(2)),
                ("segment_duration_ms", PropertyValue::Int(500)),
            ]),
            (true, Some(500))
        );
        // 0 ms alone: the default.
        assert_eq!(
            migrated(&[("segment_duration_ms", PropertyValue::Int(0))]),
            (true, Some(DEFAULT_SEGMENT_MS))
        );
        // Current flows are left alone.
        assert_eq!(
            migrated(&[("segment_duration_ms", PropertyValue::Int(500))]),
            (false, Some(500))
        );
        assert_eq!(migrated(&[]), (false, None));
    }

    #[test]
    fn video_essence_parameters_from_caps() {
        gst::init().unwrap();
        let caps = gst::Caps::builder("video/x-h264")
            .field("framerate", gst::Fraction::new(50, 1))
            .field("width", 1920i32)
            .field("height", 1080i32)
            .build();
        let params = video_essence_parameters(caps.structure(0).unwrap());
        assert_eq!(
            serde_json::Value::Object(params),
            serde_json::json!({
                "frame_rate": { "numerator": 50, "denominator": 1 },
                "frame_width": 1920,
                "frame_height": 1080
            })
        );
        // Variable frame rate and missing fields are left out.
        let caps = gst::Caps::builder("video/x-h264")
            .field("framerate", gst::Fraction::new(0, 1))
            .build();
        assert!(video_essence_parameters(caps.structure(0).unwrap()).is_empty());
    }

    /// Caps without a frame rate (an SPS without timing): the rate comes from
    /// the buffer timestamps, and later caps add to what is known.
    #[test]
    fn frame_rate_from_timestamps_when_caps_have_none() {
        gst::init().unwrap();
        let pipeline =
            gst::parse::launch("appsrc name=src format=time ! identity name=p ! fakesink")
                .unwrap()
                .downcast::<gst::Pipeline>()
                .unwrap();
        let src = pipeline
            .by_name("src")
            .unwrap()
            .downcast::<gstreamer_app::AppSrc>()
            .unwrap();
        let detected: SharedDetected = Arc::default();
        let pad = pipeline.by_name("p").unwrap().static_pad("src").unwrap();
        watch_video_essence(&pad, detected.clone(), "test");
        src.set_caps(Some(
            &gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .field("width", 640i32)
                .field("height", 360i32)
                .field("framerate", gst::Fraction::new(0, 1))
                .build(),
        ));
        pipeline.set_state(gst::State::Playing).unwrap();
        for i in 0..40u64 {
            let mut buf = gst::Buffer::with_size(16).unwrap();
            buf.get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_nseconds(i * 1_000_000_000 / 30));
            src.push_buffer(buf).unwrap();
        }
        src.end_of_stream().unwrap();
        let bus = pipeline.bus().unwrap();
        bus.timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Eos])
            .expect("eos");
        pipeline.set_state(gst::State::Null).unwrap();
        assert_eq!(
            serde_json::Value::Object(detected.lock().unwrap().essence_parameters.clone()),
            serde_json::json!({
                "frame_rate": { "numerator": 30, "denominator": 1 },
                "frame_width": 640,
                "frame_height": 360
            })
        );
    }
}
