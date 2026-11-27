//! TAMS Output block: records pre-encoded streams into a TAMS store.
//!
//! Two container modes (the `container` property):
//! - **MPEG-TS (default):** all essences are muxed by `mpegtsmux`
//!   into TS segments and registered on a single NMOS `format:multi` flow.
//!   Broadcast-native, inherent A/V sync, fewer objects. Each track has a
//!   fixed PID, and the flow collects one segment-less member flow per track
//!   (`flow_collection` with its role, PID and channel range), so a reader
//!   finds the audio pairs (`audio_labels`, e.g. `PGM,IS`) without probing.
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
use crate::tams::client::{CollectedFlow, CollectionItem, FlowSpec, TamsClient};
use crate::tams::retention::Retention;
use crate::tams::segmenter::{MuxStream, OpenDecision, OpenRequest, Segmenter};
use crate::tams::tai_mapping::{
    first_on_grid_fragment, grid_miss_ns, reference_tai_ns, shared_pipeline_mapping, AnchorKind,
    FragmentTimeline, HostTai,
};
use crate::tams::uploader::{
    channel, clear_orphans, closed_end_ns, media_end_running_time_ns, new_run, new_tail_slot,
    park_on_disk, segment_file_name, spawn_uploader, FragmentReady, MediaEnd, PadMediaEnd,
    ParkTarget, SharedDetected, TailFragment, TrackEssence,
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
/// Labels of the audio tracks in track order, e.g. `PGM,IS`.
const AUDIO_LABELS: &str = "audio_labels";
/// ISO 639 languages of the audio tracks in track order, e.g. `swe,eng`.
const AUDIO_LANGUAGES: &str = "audio_languages";
/// Rolling-buffer window in seconds (0 = keep everything).
const RETENTION_S: &str = "retention_s";
/// MPEG-TS PID of the video track. Audio track `k` gets `PID_AUDIO_0 + k`.
/// These are what `mpegtsmux` picks itself when every track is present
/// (from 0x41 in pad order), now fixed so that a fragment that lacks a track
/// does not move the others.
const PID_VIDEO: u32 = 0x41;
const PID_AUDIO_0: u32 = 0x42;

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

/// Rewrite the released `segment_duration_secs` (at least 1) to
/// `segment_duration_ms`: `secs × 1000` ms. When `segment_duration_ms` is set
/// too (a client that knows both), it stays. The seconds key is removed.
/// Returns a description of the change, or `None` when there was nothing to
/// migrate.
pub fn migrate_legacy_properties(
    properties: &mut HashMap<String, PropertyValue>,
) -> Option<String> {
    let secs = properties.remove(LEGACY_SEGMENT_DURATION_SECS)?;
    if properties.contains_key(SEGMENT_DURATION_MS) {
        return Some(format!(
            "dropped {} ({} is set)",
            LEGACY_SEGMENT_DURATION_SECS, SEGMENT_DURATION_MS
        ));
    }
    let new_ms = match &secs {
        PropertyValue::UInt(u) => (*u).max(1).saturating_mul(1000),
        PropertyValue::Int(i) => (*i).max(1).unsigned_abs().saturating_mul(1000),
        _ => DEFAULT_SEGMENT_MS,
    };
    properties.insert(SEGMENT_DURATION_MS.to_string(), PropertyValue::UInt(new_ms));
    Some(format!(
        "{} = {:?} -> {} = {}",
        LEGACY_SEGMENT_DURATION_SECS, secs, SEGMENT_DURATION_MS, new_ms
    ))
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

/// Parse a comma-separated list that follows track order (`PGM,IS`). Blank
/// entries stay as `None`, so `,IS` labels only the second track.
fn parse_track_list(s: &Option<String>) -> Vec<Option<String>> {
    s.as_deref()
        .map(|raw| {
            raw.split(',')
                .map(|v| {
                    let v = v.trim();
                    (!v.is_empty()).then(|| v.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What the properties say about one audio track.
struct AudioTrackMeta {
    /// Role in the flow collection: the label, or `audio-<k>` without one.
    role: String,
    label: Option<String>,
    language: Option<String>,
}

/// The audio tracks' labels and languages from the properties, checked
/// against the track count and each other. Refused (with a message that names
/// the property): more entries than tracks; a label that is `video` or
/// `audio-<k>` (the roles of the video track and of unlabelled tracks); two
/// tracks with the same role, which a reader that finds members by role could
/// not tell apart; and a language GStreamer does not know.
///
/// A language is normalised to the ISO 639-2 form the container's muxer writes
/// (`mpegtsmux`: 639-2/B, so `deu` and `de` become `ger`; `mp4mux`: 639-2/T,
/// `deu`), so the flow's tag says what is in the container.
fn audio_track_meta(
    props: &HashMap<String, PropertyValue>,
    count: usize,
    container: Container,
) -> Result<Vec<AudioTrackMeta>, String> {
    let labels = parse_track_list(&prop_str(props, AUDIO_LABELS));
    let languages = parse_track_list(&prop_str(props, AUDIO_LANGUAGES));
    for (name, list) in [(AUDIO_LABELS, &labels), (AUDIO_LANGUAGES, &languages)] {
        // Trailing empty entries (`PGM,IS,`) name no track.
        let named = list.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
        if named > count {
            return Err(format!(
                "{} has {} entries but there are {} audio tracks (num_audio_tracks)",
                name, named, count
            ));
        }
    }
    let mut meta: Vec<AudioTrackMeta> = Vec::with_capacity(count);
    for k in 0..count {
        let label = labels.get(k).cloned().flatten();
        if let Some(l) = &label {
            let reserved = l == "video"
                || l.strip_prefix("audio-")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            if reserved {
                return Err(format!(
                    "{}: \"{}\" is reserved (the role of the video track or of an \
                     unlabelled audio track); choose another label",
                    AUDIO_LABELS, l
                ));
            }
        }
        let role = label.clone().unwrap_or_else(|| Essence::Audio(k).tag());
        if meta.iter().any(|m| m.role == role) {
            return Err(format!(
                "{}: \"{}\" labels more than one audio track; each label must be unique",
                AUDIO_LABELS, role
            ));
        }
        let language = match languages.get(k).cloned().flatten() {
            Some(code) => Some(normalise_language(&code, container).ok_or_else(|| {
                format!(
                    "{}: \"{}\" is not an ISO 639 language code (e.g. \"swe\", \"eng\")",
                    AUDIO_LANGUAGES, code
                )
            })?),
            None => None,
        };
        meta.push(AudioTrackMeta {
            role,
            label,
            language,
        });
    }
    Ok(meta)
}

/// `code` (ISO 639-1 or 639-2, any case) as the ISO 639-2 code the
/// container's muxer writes, or `None` when GStreamer does not know it.
fn normalise_language(code: &str, container: Container) -> Option<String> {
    let code = code.to_ascii_lowercase();
    let normalised = match container {
        Container::MpegTs => gstreamer_tag::language_codes::language_code_iso_639_2b(&code),
        Container::Mp4 => gstreamer_tag::language_codes::language_code_iso_639_2t(&code),
    };
    normalised.map(|c| c.to_string())
}

/// Tags of an audio member flow: its label and language, so the tracks can be
/// found by tag.
fn audio_member_tags(meta: &AudioTrackMeta) -> Vec<(String, String)> {
    let mut tags = Vec::new();
    if let Some(l) = &meta.label {
        tags.push(("audio_label".to_string(), l.clone()));
    }
    if let Some(l) = &meta.language {
        tags.push(("audio_language".to_string(), l.clone()));
    }
    tags
}

/// A `container_mapping` from JSON (an object).
fn mapping(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    }
}

/// The property that sets what a block's TAMS ids derive from.
const TAMS_KEY: &str = "tams_key";

/// Derive a deterministic UUID (string) for a TAMS entity: UUID v5 of
/// `"<key>:<suffix>"`, where `key` is the block's [`tams_id_key`].
fn derive_id(key: &str, suffix: &str) -> String {
    Uuid::new_v5(&TAMS_NAMESPACE, format!("{}:{}", key, suffix).as_bytes()).to_string()
}

/// What every TAMS id of block `instance_id` derives from: its `tams_key`
/// when set, else `"<flow id>/<block id>"`. A block id is only unique within
/// a Strom flow, so the flow id keeps two flows with a TAMS Output of the same
/// id apart; without a flow id (`_flow_id`, injected by block expansion; a
/// bare builder in a test) it is the block id alone. A block id is random per
/// Strom flow when a product creates a new flow for each run (Open Live does,
/// on every activation of a production); a `tams_key` that is stable for the
/// product (e.g. `"<production id>/<source>"`) keeps a new run writing to the
/// same TAMS source and flows, and gives the same ids as before.
pub fn tams_id_key(instance_id: &str, properties: &HashMap<String, PropertyValue>) -> String {
    prop_str(properties, TAMS_KEY)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| match prop_str(properties, "_flow_id") {
            Some(flow_id) => format!("{}/{}", flow_id, instance_id),
            None => instance_id.to_string(),
        })
}

/// The segment directory of the TAMS flow chain `suffix` (`video`, `audio-0`,
/// `mux`) of the block whose TAMS ids derive from `key` (see [`tams_id_key`]),
/// writing to `gateway_url`: [`crate::tams::park::flow_dir`] of the TAMS flow
/// that chain writes.
pub fn segment_dir(gateway_url: &str, key: &str, suffix: &str) -> std::path::PathBuf {
    crate::tams::park::flow_dir(gateway_url, &derive_id(key, suffix))
}

/// The TAMS flow ids a block writes to (registers, or uploads segments to),
/// as `build` derives them below: per essence in MP4 plus the Multi-Flow when
/// there are two or more; in MPEG-TS the muxed flow and one member per track.
fn written_flow_ids(
    key: &str,
    container: Container,
    num_video: usize,
    num_audio: usize,
) -> Vec<String> {
    let audio = (0..num_audio).map(|i| Essence::Audio(i).tag());
    let video = (num_video >= 1).then(|| Essence::Video.tag());
    let tracks: Vec<String> = video.into_iter().chain(audio).collect();
    match container {
        Container::Mp4 => {
            let mut ids: Vec<String> = tracks.iter().map(|t| derive_id(key, t)).collect();
            if tracks.len() >= 2 {
                ids.push(derive_id(key, "mux"));
            }
            ids
        }
        Container::MpegTs => std::iter::once(derive_id(key, "mux"))
            .chain(tracks.iter().map(|t| derive_id(key, &format!("mux-{}", t))))
            .collect(),
    }
}

/// Claim the block's TAMS flows for its Strom flow (`_flow_id`, injected by
/// block expansion), or fail the build when another running Strom flow, or
/// another block of this one with the same `tams_key`, already writes to them:
/// the TAMS flow ids derive from the block id (or the `tams_key`), and block
/// ids are only unique within a Strom flow. See
/// `crate::tams::flow_claims`. A build without `_flow_id` (a bare builder in a
/// test) claims nothing.
fn claim_tams_flows(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    gateway_url: &str,
    tams_flow_ids: &[String],
) -> Result<(), BlockBuildError> {
    let Some(flow_id) = prop_str(properties, "_flow_id").and_then(|f| f.parse::<FlowId>().ok())
    else {
        return Ok(());
    };
    crate::tams::flow_claims::claim(gateway_url, tams_flow_ids, flow_id, instance_id).map_err(|c| {
        if c.owner.flow_id == flow_id {
            return BlockBuildError::InvalidConfiguration(format!(
                "TAMS Output '{}': TAMS flow {} is already written by block '{}' of this \
                 Strom flow: both blocks have the same TAMS key. Give each TAMS Output its \
                 own TAMS key",
                instance_id, c.tams_flow_id, c.owner.block_id
            ));
        }
        BlockBuildError::InvalidConfiguration(format!(
            "TAMS Output '{}': TAMS flow {} is already written by block '{}' of running \
                 Strom flow {} to {} (this is Strom flow {}). TAMS flow ids derive from the \
                 TAMS key, or the block id when no key is set, which is only unique within a \
                 Strom flow, and segments are parked per TAMS flow id whatever the gateway: \
                 give this block a TAMS key (or an id) that no other running flow uses for a \
                 TAMS Output, or stop the other flow",
            instance_id,
            c.tams_flow_id,
            c.owner.block_id,
            c.owner.flow_id,
            c.owner.gateway,
            flow_id
        ))
    })
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
        // Checked here, not first in the element setup: a gateway client that
        // cannot be built there leaves the run recording nowhere.
        crate::tams::client::check_gateway_url(gateway_url.trim_end_matches('/'))
            .map_err(|e| BlockBuildError::InvalidProperty(format!("TAMS Output: {:#}", e)))?;
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
        let retention_s = prop_u64(properties, RETENTION_S, 0);
        let retention = crate::tams::retention::window_from_secs(retention_s);
        if let Some(window) = retention.filter(|w| w.as_secs() != retention_s) {
            warn!(
                "TAMS Output {}: retention_s {} is under the minimum, keeping {} s",
                instance_id,
                retention_s,
                window.as_secs()
            );
        }
        let num_video = prop_usize(properties, "num_video_tracks", DEFAULT_NUM_VIDEO_TRACKS).min(1);
        let num_audio = prop_usize(properties, "num_audio_tracks", DEFAULT_NUM_AUDIO_TRACKS)
            .min(MAX_AUDIO_TRACKS);
        if num_video == 0 && num_audio == 0 {
            return Err(BlockBuildError::InvalidProperty(
                "TAMS Output: at least one of video/audio tracks is required".to_string(),
            ));
        }

        // Every TAMS id below derives from this key (fixed at build time).
        let key = tams_id_key(instance_id, properties);
        let source_id =
            prop_str(properties, "tams_source_id").unwrap_or_else(|| derive_id(&key, "source"));
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
        // Languages go into the container (MPEG-TS ISO 639 descriptor, MP4
        // media language); the muxer pads are fixed per container below.
        let audio_meta = audio_track_meta(properties, num_audio, container)
            .map_err(|e| BlockBuildError::InvalidProperty(format!("TAMS Output: {}", e)))?;

        claim_tams_flows(
            instance_id,
            properties,
            &gateway_url,
            &written_flow_ids(&key, container, num_video, num_audio),
        )?;

        let mut elements: Vec<(String, gst::Element)> = Vec::new();

        match container {
            // Separate single-essence MP4 flows: one per essence, grouped under a
            // Multi-Flow. Canonical TAMS model (per-essence flows collected via
            // flow_collection), giving downstream consumers elemental access.
            Container::Mp4 => {
                // Members collected by the Multi-Flow; the track index is the
                // position in the collection (video first, then audio).
                let mut members: Vec<CollectionItem> = Vec::new();
                let mut add_member = |id: &str, role: &str| {
                    members.push(CollectionItem {
                        id: id.to_string(),
                        role: role.to_string(),
                        container_mapping: mapping(
                            serde_json::json!({ "track_index": members.len() }),
                        ),
                    })
                };
                if num_video >= 1 {
                    let flow_id = derive_id(&key, "video");
                    add_member(&flow_id, "video");
                    let spec = FlowSpec {
                        flow_id,
                        // Own mono-essence Source. The gateway derives the Multi-Flow's
                        // source_collection by resolving each member flow to its
                        // source_id, so members must NOT share the program source.
                        source_id: derive_id(&key, "video-source"),
                        format: FORMAT_VIDEO.to_string(),
                        codec: Some("video/h264".to_string()), // fallback; real codec from caps
                        container: Some(CONTENT_TYPE_MP4.to_string()),
                        // Distinguish the two flows of one recording in flow listings.
                        label: label.as_ref().map(|l| format!("{} (video)", l)),
                        description: description.clone(),
                        tags: tags.clone(),
                        flow_collection: vec![],
                        collected: vec![],
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
                        vec![MuxStream::default()],
                        retention,
                        &mut elements,
                    )?;
                }
                for (i, meta) in audio_meta.iter().enumerate() {
                    let essence = Essence::Audio(i);
                    let tag = essence.tag(); // "audio-0", "audio-1", ...
                    let flow_id = derive_id(&key, &tag);
                    add_member(&flow_id, &meta.role);
                    let spec = FlowSpec {
                        flow_id,
                        // Own mono-essence Source (see video above).
                        source_id: derive_id(&key, &format!("{}-source", tag)),
                        format: FORMAT_AUDIO.to_string(),
                        codec: Some("audio/aac".to_string()), // fallback; real codec from caps
                        container: Some(CONTENT_TYPE_MP4.to_string()),
                        label: label.as_ref().map(|l| format!("{} ({})", l, meta.role)),
                        description: description.clone(),
                        tags: [tags.clone(), audio_member_tags(meta)].concat(),
                        flow_collection: vec![],
                        collected: vec![],
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
                        vec![MuxStream {
                            pad_name: None,
                            language: meta.language.clone(),
                        }],
                        retention,
                        &mut elements,
                    )?;
                }
                // Group the per-essence flows under one NMOS format:multi Multi-Flow.
                // It carries no essence of its own — segments live on the member
                // flows — so it is registered once at startup, not via an uploader.
                if members.len() >= 2 {
                    let multi = FlowSpec {
                        flow_id: derive_id(&key, "mux"),
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
                        collected: vec![],
                        essence_parameters: Default::default(),
                    };
                    register_multiflow(instance_id, multi, gateway_url.clone(), auth_plan, ctx);
                }
            }
            // One muxed MPEG-TS flow carrying all essences together.
            Container::MpegTs => {
                let mut inputs = Vec::new();
                // Fixed PID per track, and a member flow per track that the
                // muxed flow collects with its PID (registered by the uploader
                // once the track's essence is known from caps).
                let mut streams = Vec::new();
                let mut collected = Vec::new();
                let member_spec = |tag: &str, format: &str, role: &str, extra_tags| FlowSpec {
                    flow_id: derive_id(&key, &format!("mux-{}", tag)),
                    source_id: derive_id(&key, &format!("mux-{}-source", tag)),
                    format: format.to_string(),
                    codec: None, // from caps
                    // No segments of its own (see `CollectedFlow`).
                    container: None,
                    label: Some(match &label {
                        Some(l) => format!("{} ({})", l, role),
                        None => role.to_string(),
                    }),
                    description: description.clone(),
                    // Not the flow's own tags: the members are tracks of the
                    // muxed flow, not recordings a tag search should list
                    // beside it.
                    tags: extra_tags,
                    flow_collection: vec![],
                    collected: vec![],
                    essence_parameters: Default::default(),
                };
                if num_video >= 1 {
                    inputs.push(Essence::Video);
                    streams.push(MuxStream {
                        pad_name: Some(format!("sink_{}", PID_VIDEO)),
                        language: None,
                    });
                    collected.push(CollectedFlow {
                        track: Essence::Video.tag(),
                        role: "video".to_string(),
                        spec: member_spec("video", FORMAT_VIDEO, "video", vec![]),
                        container_mapping: mapping(serde_json::json!({
                            "track_index": 0,
                            "format_track_index": 0,
                            "mp2ts_container": { "pid": PID_VIDEO },
                        })),
                        inherit_parameters: true,
                    });
                }
                for (i, meta) in audio_meta.iter().enumerate() {
                    let essence = Essence::Audio(i);
                    let pid = PID_AUDIO_0 + i as u32;
                    inputs.push(essence);
                    streams.push(MuxStream {
                        pad_name: Some(format!("sink_{}", pid)),
                        language: meta.language.clone(),
                    });
                    collected.push(CollectedFlow {
                        track: essence.tag(),
                        role: meta.role.clone(),
                        spec: member_spec(
                            &essence.tag(),
                            FORMAT_AUDIO,
                            &meta.role,
                            audio_member_tags(meta),
                        ),
                        container_mapping: mapping(serde_json::json!({
                            "track_index": num_video + i,
                            "format_track_index": i,
                            "mp2ts_container": { "pid": pid },
                        })),
                        inherit_parameters: false,
                    });
                }
                let spec = FlowSpec {
                    flow_id: derive_id(&key, "mux"),
                    source_id: source_id.clone(),
                    format: FORMAT_MULTI.to_string(),
                    codec: Some(CONTENT_TYPE_MPEGTS.to_string()),
                    container: Some(CONTENT_TYPE_MPEGTS.to_string()),
                    label: label.clone(),
                    description: description.clone(),
                    tags: tags.clone(),
                    // Filled by the uploader from `collected`: one member flow
                    // per track, mapped to its PID in this flow's segments.
                    // The members have no segments of their own.
                    flow_collection: vec![],
                    collected,
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
                    streams,
                    retention,
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

/// Queue bound of each essence's `appsink` (see `build_flow_chain`).
const APPSINK_MAX_BUFFERS: u32 = 64;

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
    mux_streams: Vec<MuxStream>,
    retention: Option<std::time::Duration>,
    elements: &mut Vec<(String, gst::Element)>,
) -> Result<(), BlockBuildError> {
    if inputs.is_empty() {
        return Ok(());
    }
    // One mux stream per input (the segmenter's essences).
    if mux_streams.len() != inputs.len() {
        return Err(BlockBuildError::InvalidConfiguration(format!(
            "TAMS Output: {} mux streams for {} inputs",
            mux_streams.len(),
            inputs.len()
        )));
    }

    // Directory for this TAMS flow's segment files, parked ones included. Keyed
    // by the TAMS flow id and the gateway, so a backlog survives a restart of
    // Strom and a recreated Strom flow, and is never pointed at another
    // gateway (see `crate::tams::park`).
    let temp_dir = crate::tams::park::flow_dir(&gateway_url, &spec.flow_id);
    if let Err(e) = std::fs::create_dir_all(&temp_dir) {
        return Err(BlockBuildError::InvalidConfiguration(format!(
            "TAMS Output: cannot create segment dir {} (STROM_TAMS_PARK_DIR): {}",
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
            // Bounded and leaky: the segmenter pulls every sample in the
            // callback, so the queue stays short. When no callback is
            // installed (the element setup failed), buffers are dropped
            // instead of piling up in memory for the whole run.
            .property("max-buffers", APPSINK_MAX_BUFFERS)
            .property("drop", true)
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
                // Renegotiation: keep the flow's video parameters current
                // (audio is followed on the parser's output).
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
                } else {
                    // A track of a muxed flow: its member flow's codec.
                    d.tracks.entry(essence.tag()).or_default().codec = codec_label.to_string();
                }
            }
            if let Essence::Audio(_) = essence {
                merge_audio_params(
                    &detected_codec_probe,
                    essence,
                    detect_codec,
                    structure,
                    &instance_id_owned,
                );
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
            match essence {
                Essence::Video => {
                    watch_video_essence(&psrc, detected_codec_probe.clone(), &instance_id_owned)
                }
                // The parser's caps add what the encoder's may lack (the
                // AAC profile).
                Essence::Audio(_) => watch_audio_essence(
                    &psrc,
                    detected_codec_probe.clone(),
                    essence,
                    detect_codec,
                    &instance_id_owned,
                ),
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
        // server-wide drainer, even across a full restart, and so are the
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
        // Rolling buffer: delete this flow's segments older than the window,
        // for as long as the chain runs (its sender lives with the segmenter).
        if let Some(window) = retention {
            let alive = tx.downgrade();
            let tai = HostTai::new();
            crate::tams::retention::spawn(
                client.clone(),
                Retention {
                    flow_ids: vec![spec.flow_id.clone()],
                    window,
                    grid_ns: segment_ns,
                    protected: None,
                },
                format!("{} ({})", block_id, suffix_owned),
                move || alive.upgrade().is_some(),
                move || tai.now_ns(),
            );
        }
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
            retention.map_or(0, |w| w.as_secs()),
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
            if placed.since_anchor >= first_on_grid_fragment(segment_ns) {
                // Splits land on the first keyframe at or after the grid point,
                // so up to one frame past it is on the grid.
                if let Some(miss) = grid_miss_ns(abs_start, segment_ns, frame_ns) {
                    warn_off_grid(&block_id_sig, &tag, &off_grid, miss);
                }
            }

            // Schedule the split that ends this fragment on the next grid point.
            // The segmenter splits on the first keyframe at or after it. Ask
            // upstream for keyframes on the grid points after it, up to
            // `KEYFRAME_LEAD_NS` ahead. The encoder's input runs ahead of its
            // output (where this fragment opens) by its latency, which can be
            // most of a segment, so the encoder is often past the split
            // already; a request for a time already passed would force a
            // keyframe off the grid. The split itself was requested by an
            // earlier fragment, except right after the timeline was anchored
            // (see `FragmentTimeline::keyframe_requests`).
            let (next_split, ahead) = {
                let mut tl = timeline.lock().unwrap_or_else(|e| e.into_inner());
                let ahead = tl.keyframe_requests(running_time_ns, segment_ns);
                (
                    tl.next_split_running_time(running_time_ns, segment_ns),
                    ahead,
                )
            };
            if let (Some(_), Some(el)) = (next_split, element.as_ref()) {
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
                    // A fragment that spans more than one segment although
                    // it was not cut short: its split was missed (the
                    // keyframe asked for it never came, or came before it).
                    let span = end_ns.saturating_sub(prev.start_ns);
                    if placed.since_anchor > first_on_grid_fragment(segment_ns)
                        && end_ns >= placed.prev_end_ns
                        && span > segment_ns + frame_ns
                    {
                        warn_off_grid(&block_id_sig, &tag, &off_grid, span - segment_ns);
                    }
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
        // (or closed), the fragment is parked on disk instead, for the
        // server-wide drainer; only when that fails too (over the parking
        // limits, or the disk is full) is it dropped. This runs on the
        // streaming thread, so the park runs on the parking thread: it can
        // delete older segments for space and syncs to disk.
        let deliver_label = Arc::new(format!("{} ({})", block_id, suffix_owned));
        let overflowed = Arc::new(AtomicU64::new(0));
        let deliver = move |ready: FragmentReady| {
            let Err(e) = tx.try_send(ready) else {
                return;
            };
            let full = matches!(e, tokio::sync::mpsc::error::TrySendError::Full(_));
            let ready = e.into_inner();
            let (label, overflowed, target) = (
                deliver_label.clone(),
                overflowed.clone(),
                park_target.clone(),
            );
            crate::tams::park::park_off_thread(move || match park_on_disk(&ready, &target) {
                Ok(()) => {
                    let n = overflowed.fetch_add(1, Ordering::Relaxed);
                    if n.is_multiple_of(100) {
                        warn!(
                            "TAMS {}: uploader {}, parked segment {} on disk for the \
                             drainer ({} so far)",
                            label,
                            if full { "queue full" } else { "stopped" },
                            ready.path.display(),
                            n + 1
                        );
                    }
                }
                Err(err) => {
                    error!(
                        "TAMS {}: uploader {} and segment {} could not be parked ({}); \
                         dropping it",
                        label,
                        if full { "queue full" } else { "stopped" },
                        ready.path.display(),
                        err
                    );
                    let _ = std::fs::remove_file(&ready.path);
                }
            });
        };

        let segmenter = Segmenter::new(
            format!("{} ({})", block_id, suffix_owned),
            mux_factory,
            container == Container::MpegTs,
            mux_streams,
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

/// Report a segment boundary `miss_ns` off its TAI grid point, or a segment
/// `miss_ns` longer than one segment (a missed split), although its keyframe
/// was requested one fragment ahead: the encoder did not honour the request. Boundaries then follow the encoder's own keyframes, off the grid and
/// differently on each angle. Logged on the first miss and every 30th after.
fn warn_off_grid(block_id: &str, tag: &str, count: &AtomicU64, miss_ns: u64) {
    let n = count.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n.is_multiple_of(30) {
        warn!(
            "TAMS {} ({}): segment boundary {} ms off the TAI grid ({} so far): the \
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
            "TAMS {} ({}): segment boundary {} ms off the TAI grid",
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
    let has_rate = |d: &SharedDetected| {
        d.lock()
            .map(|d| d.essence_parameters.contains_key("frame_rate"))
            .unwrap_or(true)
    };
    // Set once the flow has a frame rate: the buffer probe is then removed.
    let rate_known = Arc::new(AtomicBool::new(has_rate(&detected)));
    {
        let (detected, block_id, rate_known) =
            (detected.clone(), block_id.to_string(), rate_known.clone());
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(e)) = info.data.as_ref() {
                if let gst::EventView::Caps(c) = e.view() {
                    if let Some(s) = c.caps().structure(0) {
                        merge_video_params(&detected, s, &block_id);
                        if has_rate(&detected) {
                            rate_known.store(true, Ordering::Release);
                        }
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
    if rate_known.load(Ordering::Acquire) {
        return;
    }
    let block_id = block_id.to_string();
    // Timestamps seen while no rate is known. Only this probe locks it.
    let samples: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        if rate_known.load(Ordering::Acquire) {
            return gst::PadProbeReturn::Remove;
        }
        let Some(t) = info.buffer().and_then(|b| b.pts()) else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(mut pts) = samples.lock() else {
            return gst::PadProbeReturn::Remove;
        };
        pts.push(t.nseconds());
        if pts.len() < RATE_SAMPLES_MIN {
            return gst::PadProbeReturn::Ok;
        }
        match crate::gst::frame_rate::estimate_frame_rate(pts.iter().copied()) {
            Some((n, d)) => {
                info!(
                    "TAMS Output {}: stream has no frame rate, {}/{} estimated from \
                     timestamps",
                    block_id, n, d
                );
                if let Ok(mut det) = detected.lock() {
                    det.essence_parameters
                        .entry("frame_rate".to_string())
                        .or_insert_with(|| serde_json::json!({ "numerator": n, "denominator": d }));
                }
                rate_known.store(true, Ordering::Release);
                gst::PadProbeReturn::Remove
            }
            None if pts.len() >= RATE_SAMPLES_MAX => {
                warn!(
                    "TAMS Output {}: stream has no frame rate and none could be estimated \
                     from {} timestamps; the flow has no frame_rate",
                    block_id,
                    pts.len()
                );
                gst::PadProbeReturn::Remove
            }
            None => gst::PadProbeReturn::Ok,
        }
    });
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

/// Merge the audio parameters in `caps` into the detected essence: the flow's
/// own (`single`, an MP4 audio flow) or the track's member flow (a track of a
/// muxed flow).
fn merge_audio_params(
    detected: &SharedDetected,
    essence: Essence,
    single: bool,
    caps: &gst::StructureRef,
    block_id: &str,
) {
    let params = audio_essence_parameters(caps);
    let Ok(mut d) = detected.lock() else {
        return;
    };
    let target = if single {
        &mut d.essence_parameters
    } else {
        &mut d
            .tracks
            .entry(essence.tag())
            .or_insert_with(TrackEssence::default)
            .essence_parameters
    };
    for (k, v) in params {
        if target.get(&k) != Some(&v) {
            debug!("TAMS Output {}: {} {} = {}", block_id, essence.tag(), k, v);
            target.insert(k, v);
        }
    }
}

/// Follow the audio parser's output caps (an event probe, not per buffer).
fn watch_audio_essence(
    pad: &gst::Pad,
    detected: SharedDetected,
    essence: Essence,
    single: bool,
    block_id: &str,
) {
    let block_id = block_id.to_string();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
        if let Some(gst::PadProbeData::Event(e)) = info.data.as_ref() {
            if let gst::EventView::Caps(c) = e.view() {
                if let Some(s) = c.caps().structure(0) {
                    merge_audio_params(&detected, essence, single, s, &block_id);
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// TAMS audio `essence_parameters` from encoded audio caps: `sample_rate` and
/// `channels` (both required by the TAMS audio flow schema), and
/// `codec_parameters` (`coded_frame_size`, and `mp4_oti`, the MPEG-4 audio
/// object type, for AAC) as far as the caps tell them.
fn audio_essence_parameters(
    caps: &gst::StructureRef,
) -> serde_json::Map<String, serde_json::Value> {
    let mut params = serde_json::Map::new();
    if let Ok(rate) = caps.get::<i32>("rate") {
        if rate > 0 {
            params.insert("sample_rate".into(), rate.into());
        }
    }
    if let Ok(channels) = caps.get::<i32>("channels") {
        if channels > 0 {
            params.insert("channels".into(), channels.into());
        }
    }
    let mut codec = serde_json::Map::new();
    let mpegversion = caps.get::<i32>("mpegversion").unwrap_or(0);
    match caps.name().as_str() {
        "audio/mpeg" if mpegversion == 1 => {
            // Layer I frames are 384 samples and layer II 1152. Layer III is
            // 1152 in MPEG-1 but 576 in MPEG-2 and 2.5 (the low sample rates,
            // `mpegaudioversion` 2 and 3 in mpegaudioparse's caps; without
            // it, a rate under 32 kHz, which MPEG-1 does not have).
            let lsf = match caps.get::<i32>("mpegaudioversion") {
                Ok(v) => v >= 2,
                Err(_) => caps.get::<i32>("rate").is_ok_and(|r| r > 0 && r < 32000),
            };
            let frame = match caps.get::<i32>("layer") {
                Ok(1) => Some(384),
                Ok(2) => Some(1152),
                Ok(3) => Some(if lsf { 576 } else { 1152 }),
                _ => None,
            };
            if let Some(frame) = frame {
                codec.insert("coded_frame_size".into(), frame.into());
            }
        }
        "audio/mpeg" => {
            // aacparse names only object types 1-4 (gst_codec_utils_aac_get_profile);
            // HE-AAC with implicit SBR shows as `lc`, at the core rate.
            let oti = match caps.get::<&str>("profile") {
                Ok("main") => Some(1),
                Ok("lc") => Some(2),
                Ok("ssr") => Some(3),
                Ok("ltp") => Some(4),
                _ => None,
            };
            if let Some(oti) = oti {
                codec.insert("mp4_oti".into(), oti.into());
                // 1024 samples per frame for all four.
                codec.insert("coded_frame_size".into(), 1024.into());
            }
        }
        "audio/x-ac3" => {
            codec.insert("coded_frame_size".into(), 1536.into());
        }
        _ => {}
    }
    if !codec.is_empty() {
        params.insert("codec_parameters".into(), codec.into());
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
            // Shared by every block that talks to a TAMS gateway (see
            // `crate::tams::auth`): the env vars and allow-list are documented
            // there.
            crate::tams::auth::auth_properties(None),
            vec![
            simple_prop(
                "label",
                "Title",
                "Human-readable title stored on the TAMS flow(s), e.g. \"Studio A - Camera 4\". Per-track flows get \" (video)\" or \" (<audio label>)\" appended.",
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
                AUDIO_LABELS,
                "Audio Labels",
                "Labels of the audio tracks in track order, comma-separated, e.g. \"PGM,IS\". Used as each track's role and in its flow title. Must be unique.",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
            simple_prop(
                AUDIO_LANGUAGES,
                "Audio Languages",
                "ISO 639 language codes of the audio tracks in track order, comma-separated, e.g. \"swe,eng\". Written into the container and as a flow tag. Empty entry: none.",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
            simple_prop(
                SEGMENT_DURATION_MS,
                "Segment Duration (ms)",
                "Segment length in ms (min 200). Boundaries sit on a TAI grid of this length; use a GOP that divides it.",
                PropertyType::Int,
                PropertyValue::Int(DEFAULT_SEGMENT_MS as i64),
            ),
            simple_prop(
                RETENTION_S,
                "Retention (s)",
                "Rolling buffer: keep this many seconds of media in the store and delete older segments of this block's TAMS flows (at least 30; 0 keeps everything). Deletes through the TAMS index (DELETE segments by timerange), so the store removes the media objects no other flow references: a clip copied into a flow of its own survives. Runs every tenth of the window (5 to 60 s), so the store holds up to that much more. With a TAMS Key the flows outlive a run, and media of earlier runs older than the window is deleted too. Deletion only runs while the flow runs.",
                PropertyType::Int,
                PropertyValue::Int(0),
            ),
            simple_prop(
                TAMS_KEY,
                "TAMS Key",
                "Optional key that every TAMS id (source and flows) derives from, instead of this block's id. Set it when the Strom flow is created anew for each run with new block ids (a product that creates a flow per activation): a key that is stable for the product, e.g. \"<production id>/<source>\", makes a new run write to the same TAMS source and flows. Two running TAMS Outputs with the same key on one gateway are refused. Leave empty to derive the ids from the block id. TAMS Source ID, when set, still sets the source id.",
                PropertyType::String,
                PropertyValue::String(String::new()),
            ),
            simple_prop(
                "tams_source_id",
                "TAMS Source ID",
                "Optional source UUID. Leave empty to derive a stable one from the TAMS key, or from this block's id when no key is set.",
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

    fn key_props(key: Option<&str>) -> HashMap<String, PropertyValue> {
        key.map(|k| (TAMS_KEY.to_string(), PropertyValue::String(k.into())))
            .into_iter()
            .collect()
    }

    /// Without a `tams_key` or a flow id (a bare builder) the ids derive from
    /// the block id. Pinned, so a change that would orphan existing
    /// recordings fails here.
    #[test]
    fn tams_ids_without_key_derive_from_block_id() {
        assert_eq!(tams_id_key("cam1", &key_props(None)), "cam1");
        assert_eq!(tams_id_key("cam1", &key_props(Some(""))), "cam1");
        assert_eq!(tams_id_key("cam1", &key_props(Some("  "))), "cam1");
        assert_eq!(
            derive_id("cam1", "mux"),
            Uuid::new_v5(&TAMS_NAMESPACE, b"cam1:mux").to_string()
        );
        assert_eq!(
            derive_id("cam1", "mux"),
            "0b2fb420-2f9c-5cff-9b6c-6cdd1a876b15"
        );
    }

    /// Two Strom flows with a TAMS Output of the same block id get their own
    /// TAMS ids (block ids are only unique within a flow). A `tams_key`
    /// overrides that: its ids are exactly those it gave before.
    #[test]
    fn tams_ids_without_key_are_per_strom_flow() {
        let with = |flow: &str, key: Option<&str>| {
            let mut p = key_props(key);
            p.insert("_flow_id".to_string(), PropertyValue::String(flow.into()));
            tams_id_key("cam1", &p)
        };
        assert_eq!(with("f1", None), "f1/cam1");
        assert_ne!(
            derive_id(&with("f1", None), "mux"),
            derive_id(&with("f2", None), "mux")
        );
        assert_eq!(with("f1", Some("prod-42/cam1")), "prod-42/cam1");
        assert_eq!(with("f2", Some("prod-42/cam1")), "prod-42/cam1");
        assert_eq!(with("f1", Some("  ")), "f1/cam1");
    }

    /// With a `tams_key` every TAMS id is the same whatever the block id, so
    /// a production recreated with new block ids writes to the same flows.
    #[test]
    fn tams_ids_with_key_ignore_block_id() {
        let props = key_props(Some("prod-42/cam1"));
        let a = tams_id_key("b7c1e0f2-random-1", &props);
        let b = tams_id_key("0d9a33aa-random-2", &props);
        assert_eq!(a, "prod-42/cam1");
        assert_eq!(a, b);
        for container in [Container::Mp4, Container::MpegTs] {
            assert_eq!(
                written_flow_ids(&a, container, 1, 2),
                written_flow_ids(&b, container, 1, 2)
            );
            // Not the ids the block id alone would give.
            assert_ne!(
                written_flow_ids(&a, container, 1, 2),
                written_flow_ids("b7c1e0f2-random-1", container, 1, 2)
            );
        }
        let gw = "https://gw.example";
        assert_eq!(segment_dir(gw, &a, "mux"), segment_dir(gw, &b, "mux"));
        assert_eq!(
            derive_id(&a, "source"),
            Uuid::new_v5(&TAMS_NAMESPACE, b"prod-42/cam1:source").to_string()
        );
    }

    fn build_with(gateway: &str) -> Result<BlockBuildResult, BlockBuildError> {
        gst::init().unwrap();
        let properties: HashMap<String, PropertyValue> = [(
            "gateway_url".to_string(),
            PropertyValue::String(gateway.into()),
        )]
        .into_iter()
        .collect();
        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        TamsOutputBuilder.build("gw_test", &properties, &ctx)
    }

    /// A gateway URL the client cannot use fails the build. Before, the
    /// element setup gave up on it at start, and the appsinks queued the
    /// whole run in memory.
    #[test]
    fn gateway_url_without_scheme_fails_the_build() {
        for gateway in ["gw.example.com", "ftp://gw.example.com", "not a url"] {
            match build_with(gateway) {
                Err(BlockBuildError::InvalidProperty(msg)) => {
                    assert!(msg.contains("gateway url"), "{gateway}: {msg}")
                }
                Err(e) => panic!("{gateway}: unexpected error {e}"),
                Ok(_) => panic!("{gateway}: built"),
            }
        }
        assert!(build_with("http://gw.example.com/").is_ok());
    }

    /// The appsinks are bounded and leaky, so nothing piles up in memory
    /// when no callback drains them.
    #[test]
    fn appsinks_are_bounded() {
        let built = build_with("http://gw.example.com").unwrap();
        let sinks: Vec<_> = built
            .elements
            .iter()
            .filter(|(_, e)| e.factory().is_some_and(|f| f.name() == "appsink"))
            .collect();
        assert!(!sinks.is_empty());
        for (id, sink) in sinks {
            let max: u32 = sink.property("max-buffers");
            assert!(max > 0, "{id}: unbounded");
            assert!(sink.property::<bool>("drop"), "{id}: not leaky");
        }
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
        // At least 1 s, as the seconds property was.
        assert_eq!(
            migrated(&[("segment_duration_secs", PropertyValue::UInt(0))]),
            (true, Some(1000))
        );
        // Both set: milliseconds stay.
        assert_eq!(
            migrated(&[
                ("segment_duration_secs", PropertyValue::Int(2)),
                ("segment_duration_ms", PropertyValue::Int(500)),
            ]),
            (true, Some(500))
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

    #[test]
    fn audio_essence_parameters_from_caps() {
        gst::init().unwrap();
        let params = |caps: gst::Caps| {
            serde_json::Value::Object(audio_essence_parameters(caps.structure(0).unwrap()))
        };
        // AAC-LC after aacparse: profile known.
        assert_eq!(
            params(
                gst::Caps::builder("audio/mpeg")
                    .field("mpegversion", 4i32)
                    .field("rate", 48000i32)
                    .field("channels", 2i32)
                    .field("profile", "lc")
                    .build()
            ),
            serde_json::json!({
                "sample_rate": 48000,
                "channels": 2,
                "codec_parameters": { "coded_frame_size": 1024, "mp4_oti": 2 }
            })
        );
        // A profile aacparse does not name: no object type, no frame size.
        assert_eq!(
            params(
                gst::Caps::builder("audio/mpeg")
                    .field("mpegversion", 4i32)
                    .field("rate", 48000i32)
                    .field("channels", 2i32)
                    .field("profile", "he-aac")
                    .build()
            ),
            serde_json::json!({ "sample_rate": 48000, "channels": 2 })
        );
        // MPEG audio: layer I 384, layer II 1152, layer III 1152 in MPEG-1
        // and 576 in MPEG-2/2.5 (by mpegaudioversion, else by rate).
        let frame = |layer: i32, version: Option<i32>, rate: i32| {
            let mut caps = gst::Caps::builder("audio/mpeg")
                .field("mpegversion", 1i32)
                .field("layer", layer)
                .field("rate", rate)
                .field("channels", 2i32);
            if let Some(v) = version {
                caps = caps.field("mpegaudioversion", v);
            }
            params(caps.build())["codec_parameters"]["coded_frame_size"].clone()
        };
        assert_eq!(frame(1, Some(1), 48000), 384);
        assert_eq!(frame(2, Some(1), 48000), 1152);
        assert_eq!(frame(2, Some(2), 24000), 1152);
        assert_eq!(frame(3, Some(1), 48000), 1152);
        assert_eq!(frame(3, Some(2), 24000), 576);
        assert_eq!(frame(3, Some(3), 12000), 576);
        assert_eq!(frame(3, None, 44100), 1152);
        assert_eq!(frame(3, None, 22050), 576);
        // AC-3.
        assert_eq!(
            params(
                gst::Caps::builder("audio/x-ac3")
                    .field("rate", 48000i32)
                    .build()
            ),
            serde_json::json!({
                "sample_rate": 48000,
                "codec_parameters": { "coded_frame_size": 1536 }
            })
        );
        // Encoder caps before the parser: no profile yet.
        assert_eq!(
            params(
                gst::Caps::builder("audio/mpeg")
                    .field("mpegversion", 4i32)
                    .field("rate", 44100i32)
                    .field("channels", 1i32)
                    .build()
            ),
            serde_json::json!({ "sample_rate": 44100, "channels": 1 })
        );
    }

    #[test]
    fn audio_labels_and_languages_follow_track_order() {
        let props: HashMap<String, PropertyValue> =
            [(AUDIO_LABELS, " PGM , IS,"), (AUDIO_LANGUAGES, ",eng")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), PropertyValue::String(v.into())))
                .collect();
        let meta = audio_track_meta(&props, 3, Container::MpegTs).unwrap();
        let got: Vec<_> = meta
            .iter()
            .map(|m| (m.role.as_str(), m.label.as_deref(), m.language.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("PGM", Some("PGM"), None),
                ("IS", Some("IS"), Some("eng")),
                // Unlabelled tracks keep the role they had.
                ("audio-2", None, None),
            ]
        );
        assert_eq!(
            audio_member_tags(&meta[1]),
            vec![
                ("audio_label".to_string(), "IS".to_string()),
                ("audio_language".to_string(), "eng".to_string())
            ]
        );
        assert_eq!(
            audio_track_meta(&HashMap::new(), 1, Container::MpegTs).unwrap()[0].role,
            "audio-0"
        );
    }

    /// Labels and languages that do not fit the tracks are refused at build
    /// time, with a message that names the property; languages are written
    /// in the form the container's muxer uses.
    #[test]
    fn audio_labels_and_languages_are_checked() {
        let meta = |labels: &str, languages: &str, count: usize, container: Container| {
            let props: HashMap<String, PropertyValue> =
                [(AUDIO_LABELS, labels), (AUDIO_LANGUAGES, languages)]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), PropertyValue::String(v.into())))
                    .collect();
            audio_track_meta(&props, count, container)
        };
        let refused = |labels: &str, languages: &str, count: usize, property: &str| {
            let err = meta(labels, languages, count, Container::MpegTs)
                .err()
                .unwrap_or_else(|| panic!("{:?} {:?} accepted", labels, languages));
            assert!(err.starts_with(property), "{}", err);
        };
        // More entries than tracks.
        refused("PGM,IS,EXTRA", "", 2, AUDIO_LABELS);
        refused("", "swe,eng,fin", 2, AUDIO_LANGUAGES);
        // Duplicates, and labels that take another track's role.
        refused("PGM,PGM", "", 2, AUDIO_LABELS);
        refused("video", "", 1, AUDIO_LABELS);
        refused("audio-1", "", 2, AUDIO_LABELS);
        refused(",audio-0", "", 2, AUDIO_LABELS);
        // Unknown language.
        refused("", "swe,xx9", 2, AUDIO_LANGUAGES);

        // Fewer entries, and trailing empty ones, are fine.
        assert!(meta("PGM", "", 2, Container::MpegTs).is_ok());
        assert!(meta("PGM,IS,,", "swe,,", 2, Container::MpegTs).is_ok());
        // A label that only looks like a fallback role is fine.
        assert!(meta("audio-x,Audio-1", "", 2, Container::MpegTs).is_ok());

        // ISO 639-2/B for MPEG-TS (as mpegtsmux writes it), 639-2/T for MP4.
        let languages = |container| {
            meta("", "deu,FR,swe", 3, container)
                .unwrap()
                .into_iter()
                .map(|m| m.language.unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(languages(Container::MpegTs), ["ger", "fre", "swe"]);
        assert_eq!(languages(Container::Mp4), ["deu", "fra", "swe"]);
    }

    /// Caps without a frame rate (an SPS without timing): the rate comes from
    /// the buffer timestamps, and later caps add to what is known.
    /// Once the flow has a frame rate, the buffer probe is gone: buffers no
    /// longer take the detected-essence lock (shared with the uploader).
    #[test]
    fn rate_watch_leaves_buffers_alone_once_the_rate_is_known() {
        gst::init().unwrap();
        let pipeline = gst::parse::launch(
            "appsrc name=src format=time ! identity name=p ! fakesink name=sink",
        )
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
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let tx = Mutex::new(tx);
        pipeline
            .by_name("sink")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                let _ = tx.lock().unwrap().send(());
                gst::PadProbeReturn::Ok
            });
        src.set_caps(Some(
            &gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .field("framerate", gst::Fraction::new(25, 1))
                .build(),
        ));
        pipeline.set_state(gst::State::Playing).unwrap();
        let push = |i: u64| {
            let mut buf = gst::Buffer::with_size(16).unwrap();
            buf.get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_nseconds(i * 40_000_000));
            src.push_buffer(buf).unwrap();
        };
        push(0);
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("first buffer");
        // Hold the lock the old per-buffer probe took on every buffer.
        let guard = detected.lock().unwrap();
        for i in 1..5 {
            push(i);
        }
        src.end_of_stream().unwrap();
        let eos = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(gst::ClockTime::from_seconds(3), &[gst::MessageType::Eos]);
        drop(guard);
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(
            eos.is_some(),
            "buffers waited for the detected-essence lock"
        );
    }

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
