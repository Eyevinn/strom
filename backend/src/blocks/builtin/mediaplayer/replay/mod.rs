//! Timeline mode of the media player (`mode=timeline`), used for replay.
//!
//! In this mode the block does not play a playlist. It holds
//! decoded frames of a segmented recording around a cursor shared by every
//! player of the group, and outputs the frame under the cursor on every tick
//! of the flow clock. Jog, scrub, slow motion and reverse move the cursor.
//!
//! Modules, from source to output:
//! - `source`: a `SourceIndex` per protocol (local `index.json`, TAMS);
//! - `cache`: index window and coded bytes around the cursor, fetched async;
//! - `decode`: segment bytes to frames on the player's own TAI frame grid
//!   (`grid`, the source's rate);
//! - `timeline`: the group's shared cursor (a TAI instant), range and live
//!   edge; each player shows its latest frame at or before the cursor;
//! - `player`: decode scheduling, frame store and the output tick; a source
//!   switch at runtime (`cue {source}`, `source`) without a rebuild.
//!
//! `pacing=offline` exports a clip (`clip_in`, `clip_out`, `clip_speed`):
//! the same player on a frame counter instead of the flow clock, with a
//! non-live, blocking appsrc, ending the stream at the out point. Follow it
//! with an encoder, a muxer and a sink with `sync=false` (e.g. `filesink`).

pub mod backoff;
pub mod cache;
pub mod decode;
pub mod grid;
pub mod player;
pub mod source;
pub mod timeline;

use super::bridge::NetworkBuffer;
use super::definition::DEFAULT_DECODED_AHEAD_S;
use crate::blocks::{
    live_bridge_appsrc, BlockBuildError, BlockBuildResult, BusMessageConnectFn,
    APPSRC_MAX_BYTES_VIDEO,
};
use crate::client_auth::AuthMethod;
use crate::tams::auth::TamsAuth;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use strom_types::element::ElementPadRef;
use strom_types::mediaplayer::{
    timeline_group, CLIP_IN_PROPERTY, CLIP_OUT_PROPERTY, CLIP_SPEED_PROPERTY,
    DECODED_AHEAD_PROPERTY, FOLLOW_LIVE_PROPERTY, GROUP_PROPERTY, JOG_GRID_PROPERTY,
    LIVE_MARGIN_PROPERTY, OFFSET_MS_PROPERTY, OUTPUT_FRAMERATE_PROPERTY, PACING_OFFLINE,
    PACING_PROPERTY, PACING_REALTIME, PRIVATE_GROUP_PREFIX, READ_AHEAD_PROPERTY, SOURCE_PROPERTY,
    START_MODE_PROPERTY,
};
use strom_types::replay::{ReplayJogGrid, ReplayTarget};
use strom_types::{FlowId, PropertyValue};
use tracing::{info, warn};
use uuid::Uuid;

/// Parse a frame rate: `N/D`, or a whole number. Empty = `None`.
pub fn parse_frame_rate(s: &str) -> Result<Option<(u32, u32)>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let bad = || format!("bad frame rate '{}', want e.g. 50/1 or 30000/1001", s);
    let (n, d) = match s.split_once('/') {
        Some((n, d)) => (n.trim().parse::<u32>(), d.trim().parse::<u32>()),
        None => (s.parse::<u32>(), Ok(1)),
    };
    match (n, d) {
        (Ok(n), Ok(d)) if n > 0 && d > 0 => Ok(Some((n, d))),
        _ => Err(bad()),
    }
}

/// A clip's in or out point: UTC (RFC 3339, has a `T`), TAI as TAMS
/// `"<seconds>:<nanoseconds>"`, or the group time `t` (integer ns).
pub fn parse_time_ref(s: &str) -> Result<ReplayTarget, String> {
    let s = s.trim();
    if s.contains('T') {
        strom_types::tai::parse_utc(s).ok_or_else(|| format!("bad UTC time '{}'", s))?;
        return Ok(ReplayTarget {
            utc: Some(s.to_string()),
            ..Default::default()
        });
    }
    let t = if s.contains(':') {
        strom_types::tams::parse_timestamp_signed(s)
    } else {
        s.parse::<i64>().ok()
    };
    t.map(ReplayTarget::t).ok_or_else(|| {
        format!(
            "bad time '{}', want UTC (2026-11-24T13:21:56.080Z), TAI (<s>:<ns>) or ns",
            s
        )
    })
}

/// Largest `offset_ms` either way: a day. Keeps every time moved by it far
/// from overflow.
const MAX_OFFSET_MS: i64 = 86_400_000;

/// A player's replay properties and identity.
fn player_config(
    properties: &HashMap<String, PropertyValue>,
    source: &str,
    spec: &source::SourceSpec,
) -> Result<timeline::PlayerConfig, String> {
    let live_margin_ms = match prop_str(properties, LIVE_MARGIN_PROPERTY) {
        None => match properties.get(LIVE_MARGIN_PROPERTY) {
            Some(PropertyValue::UInt(u)) => Some(*u),
            Some(PropertyValue::Int(i)) if *i >= 0 => Some(*i as u64),
            _ => None,
        },
        Some(s) => Some(
            s.parse::<u64>()
                .map_err(|_| format!("bad {} '{}', want ms or empty", LIVE_MARGIN_PROPERTY, s))?,
        ),
    };
    let follow_live = match properties.get(FOLLOW_LIVE_PROPERTY) {
        None => true,
        Some(PropertyValue::Bool(b)) => *b,
        Some(PropertyValue::String(s)) => s.trim() != "false",
        Some(v) => return Err(format!("bad {} {:?}", FOLLOW_LIVE_PROPERTY, v)),
    };
    let jog_grid = match prop_str(properties, JOG_GRID_PROPERTY).as_deref() {
        None | Some("source") => ReplayJogGrid::Source,
        Some("output") => ReplayJogGrid::Output,
        Some(o) => {
            return Err(format!(
                "bad {} '{}', want source or output",
                JOG_GRID_PROPERTY, o
            ))
        }
    };
    let start_mode = match prop_str(properties, START_MODE_PROPERTY).as_deref() {
        None | Some("paused") | Some("first") => timeline::StartMode::Paused,
        Some("live") => timeline::StartMode::Live,
        Some(o) => {
            return Err(format!(
                "bad {} '{}', want paused, first or live",
                START_MODE_PROPERTY, o
            ))
        }
    };
    let offset_ms = match properties.get(OFFSET_MS_PROPERTY) {
        None => 0,
        Some(PropertyValue::Int(i)) => *i,
        Some(PropertyValue::UInt(u)) => i64::try_from(*u).unwrap_or(i64::MAX),
        Some(PropertyValue::String(s)) if s.trim().is_empty() => 0,
        Some(PropertyValue::String(s)) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("bad {} '{}'", OFFSET_MS_PROPERTY, s))?,
        Some(v) => return Err(format!("bad {} {:?}", OFFSET_MS_PROPERTY, v)),
    };
    if !(-MAX_OFFSET_MS..=MAX_OFFSET_MS).contains(&offset_ms) {
        return Err(format!(
            "bad {} {}, want -{} .. {} (24 h)",
            OFFSET_MS_PROPERTY, offset_ms, MAX_OFFSET_MS, MAX_OFFSET_MS
        ));
    }
    let timeline::SourceIdent {
        source,
        source_id,
        store,
    } = source_ident(spec, source);
    Ok(timeline::PlayerConfig {
        name: prop_str(properties, "_block_name"),
        source,
        source_id,
        store,
        follow_live,
        jog_grid,
        start_mode,
        live_margin_ms,
        offset_ns: offset_ms * 1_000_000,
    })
}

/// How a source shows in discovery and the state: without credentials.
fn source_ident(spec: &source::SourceSpec, source: &str) -> timeline::SourceIdent {
    match spec {
        source::SourceSpec::Tams { gateway, flow_id } => {
            let store = format!(
                "tams+{}",
                crate::client_auth::redact_url(gateway).trim_end_matches('/')
            );
            // A query or fragment (a token, say) is not part of the id, and
            // discovery must not show it.
            let flow_id = flow_id.split(['?', '#']).next().unwrap_or_default();
            let uri = format!("{}/flows/{}", store, flow_id);
            timeline::SourceIdent {
                source: Some(uri),
                source_id: Some(flow_id.to_string()),
                store: Some(store),
            }
        }
        // Local: the property as given (relative to the media path, or a
        // path inside it); the directory's name is its id.
        source::SourceSpec::Local(dir) => timeline::SourceIdent {
            source: Some(source.to_string()),
            source_id: dir.file_name().map(|n| n.to_string_lossy().into_owned()),
            store: None,
        },
    }
}

/// Whether a switch from the built source `from` to `to` stays in the same
/// store: the same TAMS gateway, or local directories. The block's gateway
/// credentials (an `api_token` property) were given for its own gateway;
/// a switch must not carry them anywhere else.
fn same_store(from: &source::SourceSpec, to: &source::SourceSpec) -> bool {
    match (from, to) {
        (
            source::SourceSpec::Tams { gateway: a, .. },
            source::SourceSpec::Tams { gateway: b, .. },
        ) => a.trim_end_matches('/') == b.trim_end_matches('/'),
        (source::SourceSpec::Local(_), source::SourceSpec::Local(_)) => true,
        _ => false,
    }
}

/// How the player opens another source for a switch: in the same store as
/// its built source, with the block's auth, a local directory inside the
/// media path.
fn source_opener(
    properties: &HashMap<String, PropertyValue>,
    media_path: &str,
    flow_id: FlowId,
    built: source::SourceSpec,
) -> std::sync::Arc<player::OpenSource> {
    let properties = properties.clone();
    let media_path = media_path.to_string();
    std::sync::Arc::new(move |uri: &str| {
        let spec = source::parse_source(uri, &media_path)?;
        if !same_store(&built, &spec) {
            let ident = |s: &source::SourceSpec| {
                source_ident(s, "")
                    .store
                    .unwrap_or_else(|| "local media".to_string())
            };
            return Err(format!(
                "{} is not in the player's store ({}); a switch stays in it",
                ident(&spec),
                ident(&built)
            ));
        }
        let auth = match &spec {
            source::SourceSpec::Tams { gateway, .. } => {
                TamsAuth::from_properties(&properties, gateway)?.method(&flow_id.to_string())
            }
            source::SourceSpec::Local(_) => AuthMethod::None,
        };
        let index = source::open(&spec, auth, std::path::Path::new(&media_path))?;
        Ok(timeline::OpenedSource {
            index,
            uri: uri.to_string(),
            ident: source_ident(&spec, uri),
        })
    })
}

/// The clip of an offline player, from its properties; `None` when the
/// player is paced in real time.
fn clip_of(properties: &HashMap<String, PropertyValue>) -> Result<Option<timeline::Clip>, String> {
    let pacing = prop_str(properties, PACING_PROPERTY).unwrap_or_else(|| PACING_REALTIME.into());
    let clip_in = prop_str(properties, CLIP_IN_PROPERTY);
    let clip_out = prop_str(properties, CLIP_OUT_PROPERTY);
    match pacing.as_str() {
        PACING_REALTIME => {
            if clip_in.is_some() || clip_out.is_some() {
                return Err(format!(
                    "{} and {} need {}={} (in real time, play a clip with the cue and \
                     play commands)",
                    CLIP_IN_PROPERTY, CLIP_OUT_PROPERTY, PACING_PROPERTY, PACING_OFFLINE
                ));
            }
            Ok(None)
        }
        PACING_OFFLINE => {
            let (Some(i), Some(o)) = (clip_in, clip_out) else {
                return Err(format!(
                    "{}={} needs {} and {}",
                    PACING_PROPERTY, PACING_OFFLINE, CLIP_IN_PROPERTY, CLIP_OUT_PROPERTY
                ));
            };
            let speed = match properties.get(CLIP_SPEED_PROPERTY) {
                None => 1.0,
                Some(PropertyValue::Float(f)) => *f,
                Some(PropertyValue::Int(i)) => *i as f64,
                Some(PropertyValue::UInt(u)) => *u as f64,
                Some(PropertyValue::String(s)) if s.trim().is_empty() => 1.0,
                Some(PropertyValue::String(s)) => s
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| format!("bad {} '{}'", CLIP_SPEED_PROPERTY, s))?,
                Some(v) => return Err(format!("bad {} {:?}", CLIP_SPEED_PROPERTY, v)),
            };
            if !(speed > 0.0 && speed <= timeline::MAX_SPEED) {
                return Err(format!(
                    "{} {} is not in (0, {}]",
                    CLIP_SPEED_PROPERTY,
                    speed,
                    timeline::MAX_SPEED
                ));
            }
            Ok(Some(timeline::Clip {
                in_point: parse_time_ref(&i)?,
                out: parse_time_ref(&o)?,
                speed,
            }))
        }
        other => Err(format!(
            "bad {} '{}', want {} or {}",
            PACING_PROPERTY, other, PACING_REALTIME, PACING_OFFLINE
        )),
    }
}

fn prop_str(properties: &HashMap<String, PropertyValue>, key: &str) -> Option<String> {
    match properties.get(key) {
        Some(PropertyValue::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Build a timeline player. `properties` are current names (migrated).
pub fn build(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
) -> Result<BlockBuildResult, BlockBuildError> {
    let flow_id: FlowId = prop_str(properties, "_flow_id")
        .and_then(|s| Uuid::parse_str(&s).ok())
        .unwrap_or_else(Uuid::nil);
    let media_path = prop_str(properties, "_media_path").unwrap_or_else(|| "./media".to_string());
    let invalid =
        |e: String| BlockBuildError::InvalidProperty(format!("Media Player timeline mode: {}", e));
    let source = prop_str(properties, SOURCE_PROPERTY).ok_or_else(|| {
        invalid(
            "source is required (a segment directory or \
             tams+https://<gateway>/flows/<flow_id>)"
                .to_string(),
        )
    })?;
    if prop_str(properties, GROUP_PROPERTY).is_some_and(|g| g.starts_with(PRIVATE_GROUP_PREFIX)) {
        return Err(invalid(format!(
            "group may not start with '{}' (reserved for a player's own group)",
            PRIVATE_GROUP_PREFIX
        )));
    }
    let group = timeline_group(properties, instance_id);
    let uint = |key: &str, default: u64| match properties.get(key) {
        Some(PropertyValue::UInt(u)) => *u,
        Some(PropertyValue::Int(i)) if *i >= 0 => *i as u64,
        _ => default,
    };
    let decoded_ahead_s =
        uint(DECODED_AHEAD_PROPERTY, DEFAULT_DECODED_AHEAD_S).min(player::MAX_DECODE_AHEAD_S);
    let read_ahead_s = uint(READ_AHEAD_PROPERTY, NetworkBuffer::DEFAULT_READ_AHEAD_S)
        .min(NetworkBuffer::MAX_READ_AHEAD_S);
    // The fetcher must hold what the decoder reads ahead (both capped, so
    // this stays within MAX_READ_AHEAD_S).
    if read_ahead_s < decoded_ahead_s + 2 {
        warn!(
            "Media Player {}: read_ahead_s {} is less than decoded_ahead_s + 2; \
             fetching {} s ahead",
            instance_id,
            read_ahead_s,
            decoded_ahead_s + 2
        );
    }
    let output_rate =
        parse_frame_rate(&prop_str(properties, OUTPUT_FRAMERATE_PROPERTY).unwrap_or_default())
            .map_err(invalid)?;
    let clip = clip_of(properties).map_err(invalid)?;
    let spec = source::parse_source(&source, &media_path).map_err(invalid)?;
    let config = player_config(properties, &source, &spec).map_err(invalid)?;
    // Gateway auth is checked against the gateway it goes to (env token and
    // OSC SAT only to allowed gateways).
    let auth = match &spec {
        source::SourceSpec::Tams { gateway, .. } => TamsAuth::from_properties(properties, gateway)
            .map_err(invalid)?
            .method(&flow_id.to_string()),
        source::SourceSpec::Local(_) => AuthMethod::None,
    };
    let index = source::open(&spec, auth, std::path::Path::new(&media_path)).map_err(invalid)?;
    info!(
        "Media Player {}: timeline mode, group '{}', source {}",
        instance_id,
        group,
        index.describe()
    );

    let appsrc_id = format!("{}:appsrc_video", instance_id);
    let queue_id = format!("{}:queue_video", instance_id);
    let out_id = format!("{}:video_out", instance_id);
    let appsrc = match clip {
        // Live: a full queue drops the newest picture, the tick never waits.
        None => live_bridge_appsrc(APPSRC_MAX_BYTES_VIDEO)
            .name(&appsrc_id)
            .leaky_type(gst_app::AppLeakyType::Upstream)
            .build(),
        // Offline: not live (the flow prerolls and nothing syncs to the
        // clock), and a full queue blocks the push: backpressure.
        Some(_) => gst_app::AppSrc::builder()
            .name(&appsrc_id)
            .format(gst::Format::Time)
            .is_live(false)
            .block(true)
            .automatic_eos(false)
            .max_bytes(APPSRC_MAX_BYTES_VIDEO)
            .build(),
    };
    let queue = gst::ElementFactory::make("queue")
        .name(&queue_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", queue_id, e)))?;
    let out = gst::ElementFactory::make("identity")
        .name(&out_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", out_id, e)))?;

    let timeline = timeline::get_or_create(flow_id, &group);
    let player = player::ReplayPlayer::new(
        instance_id,
        index,
        timeline,
        &appsrc,
        decoded_ahead_s,
        read_ahead_s,
        player::Output {
            rate: output_rate,
            clip,
            config,
            switching: Some((
                source.clone(),
                source_opener(properties, &media_path, flow_id, spec),
            )),
        },
    );

    // The bus handler owns the player for the flow's run: when the flow
    // stops, the handler is disconnected and dropped, and the guard shuts the
    // player down. A flow built but never started drops the guard with the
    // unused handler.
    let guard = player::PlayerGuard(player);
    let bus_message_handler: BusMessageConnectFn = Box::new(move |bus: &gst::Bus, _, _| {
        guard.0.start();
        bus.connect_message(None, move |_, _| {
            let _ = &guard;
        })
    });

    Ok(BlockBuildResult {
        elements: vec![
            (appsrc_id.clone(), appsrc.upcast()),
            (queue_id.clone(), queue),
            (out_id.clone(), out),
        ],
        internal_links: vec![
            (
                ElementPadRef::pad(&appsrc_id, "src"),
                ElementPadRef::pad(&queue_id, "sink"),
            ),
            (
                ElementPadRef::pad(&queue_id, "src"),
                ElementPadRef::pad(&out_id, "sink"),
            ),
        ],
        bus_message_handler: Some(bus_message_handler),
        pad_properties: HashMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rates_parse() {
        assert_eq!(parse_frame_rate(""), Ok(None));
        assert_eq!(parse_frame_rate("50/1"), Ok(Some((50, 1))));
        assert_eq!(parse_frame_rate(" 30000/1001 "), Ok(Some((30000, 1001))));
        assert_eq!(parse_frame_rate("100"), Ok(Some((100, 1))));
        assert!(parse_frame_rate("0/1").is_err());
        assert!(parse_frame_rate("29.97").is_err());
        assert!(parse_frame_rate("50/").is_err());
    }

    fn props(kv: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn st(s: &str) -> PropertyValue {
        PropertyValue::String(s.to_string())
    }

    /// Discovery never shows a TAMS URI's query, fragment or credentials:
    /// not in the store, the source id or the source.
    #[test]
    fn a_tams_source_shows_no_query_or_credentials() {
        let uri = "tams+https://user:pw@gw.example.com/flows/abc?api_key=SECRET#frag";
        let spec = source::parse_source(uri, "/tmp").unwrap();
        let c = player_config(&props(&[]), uri, &spec).unwrap();
        assert_eq!(c.store.as_deref(), Some("tams+https://gw.example.com"));
        assert_eq!(c.source_id.as_deref(), Some("abc"));
        assert_eq!(
            c.source.as_deref(),
            Some("tams+https://gw.example.com/flows/abc")
        );
    }

    /// `offset_ms` is kept within a day either way, so no time moved by it
    /// can overflow.
    #[test]
    fn offset_ms_is_bounded() {
        let spec = source::parse_source("/tmp/x", "/tmp").unwrap();
        let cfg = |v: PropertyValue| player_config(&props(&[(OFFSET_MS_PROPERTY, v)]), "x", &spec);
        assert_eq!(
            cfg(PropertyValue::Int(-MAX_OFFSET_MS)).unwrap().offset_ns,
            -MAX_OFFSET_MS * 1_000_000
        );
        assert!(cfg(PropertyValue::Int(MAX_OFFSET_MS + 1)).is_err());
        assert!(cfg(PropertyValue::Int(i64::MIN)).is_err());
        assert!(cfg(PropertyValue::UInt(u64::MAX)).is_err());
        assert!(cfg(st("9223372036854775807")).is_err());
    }

    #[test]
    fn clips_parse_from_properties() {
        assert_eq!(clip_of(&props(&[])), Ok(None));
        let c = clip_of(&props(&[
            (PACING_PROPERTY, st("offline")),
            (CLIP_IN_PROPERTY, st("2026-11-26T10:00:00.000Z")),
            (CLIP_OUT_PROPERTY, st("1795600000:500000000")),
            (CLIP_SPEED_PROPERTY, PropertyValue::Float(0.5)),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.in_point.utc.as_deref(), Some("2026-11-26T10:00:00.000Z"));
        assert_eq!(c.out.t.as_deref(), Some("1795600000500000000"));
        assert_eq!(c.speed, 0.5);
        // Offline needs both ends; real time takes no clip.
        assert!(clip_of(&props(&[(PACING_PROPERTY, st("offline"))])).is_err());
        assert!(clip_of(&props(&[(CLIP_IN_PROPERTY, st("1:0"))])).is_err());
        let bad_speed = props(&[
            (PACING_PROPERTY, st("offline")),
            (CLIP_IN_PROPERTY, st("1:0")),
            (CLIP_OUT_PROPERTY, st("2:0")),
            (CLIP_SPEED_PROPERTY, PropertyValue::Float(-1.0)),
        ]);
        assert!(clip_of(&bad_speed).is_err());
        assert!(parse_time_ref("yesterday").is_err());
        assert!(clip_of(&props(&[(PACING_PROPERTY, st("fast"))])).is_err());
    }

    /// Four 1 s segments of 25p H.264 (with B-frames) in MPEG-TS and an
    /// `index.json`, from TAI `BASE`, under `root/segs`.
    fn write_segments(root: &std::path::Path) {
        write_segments_as(root, "segs", "smpte", false);
    }

    /// The same under `root/dir`, of videotestsrc `pattern`; `absolute`:
    /// the index says its times are real TAI.
    fn write_segments_as(root: &std::path::Path, dir: &str, pattern: &str, absolute: bool) {
        const BASE: i64 = 1_800_000_000_000_000_000;
        let p = gst::parse::launch(&format!(
            "videotestsrc num-buffers=25 pattern={} ! \
             video/x-raw,width=320,height=240,framerate=25/1 ! \
             x264enc bframes=2 key-int-max=25 ! h264parse ! mpegtsmux ! \
             appsink name=sink sync=false",
            pattern
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let sink = p
            .by_name("sink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let mut ts = Vec::new();
        while let Ok(s) = sink.pull_sample() {
            ts.extend_from_slice(&s.buffer().unwrap().map_readable().unwrap());
        }
        p.set_state(gst::State::Null).unwrap();
        let dir = root.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut segs = Vec::new();
        for i in 0..4i64 {
            let name = format!("seg_{}.ts", i);
            std::fs::write(dir.join(&name), &ts).unwrap();
            segs.push(serde_json::json!({
                "object": name,
                "start_ns": BASE + i * 1_000_000_000,
                "end_ns": BASE + (i + 1) * 1_000_000_000,
            }));
        }
        let index = serde_json::json!({
            "container": "video/mp2t",
            "codec": "video/h264",
            "frame_rate": {"numerator": 25, "denominator": 1},
            "absolute_time": absolute,
            "segments": segs,
        });
        std::fs::write(dir.join("index.json"), index.to_string()).unwrap();
    }

    /// Run an offline player over the segments to end of stream: each
    /// buffer's PTS and the TAI of the source frame it shows.
    fn export(speed: f64) -> Vec<(u64, u64)> {
        gst::init().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_segments(root.path());
        let props = props(&[
            ("_media_path", st(root.path().to_str().unwrap())),
            (SOURCE_PROPERTY, st("segs")),
            (PACING_PROPERTY, st("offline")),
            (CLIP_IN_PROPERTY, st("1800000001:0")),
            (CLIP_OUT_PROPERTY, st("1800000003:0")),
            (CLIP_SPEED_PROPERTY, PropertyValue::Float(speed)),
        ]);
        let built = build(&format!("export_test_{}", speed), &props).unwrap();
        let pipeline = gst::Pipeline::new();
        let mut elements = HashMap::new();
        for (id, e) in &built.elements {
            pipeline.add(e).unwrap();
            elements.insert(id.clone(), e.clone());
        }
        for (a, b) in &built.internal_links {
            elements[&a.element_id]
                .link_pads(
                    a.pad_name.as_deref(),
                    &elements[&b.element_id],
                    b.pad_name.as_deref(),
                )
                .unwrap();
        }
        let sink = gst_app::AppSink::builder().sync(false).build();
        pipeline.add(&sink).unwrap();
        let out = built
            .elements
            .iter()
            .find(|(id, _)| id.ends_with(":video_out"))
            .unwrap();
        out.1.link(&sink).unwrap();
        let bus = pipeline.bus().unwrap();
        let handler = (built.bus_message_handler.unwrap())(
            &bus,
            Uuid::nil(),
            crate::events::EventBroadcaster::default(),
        );
        pipeline.set_state(gst::State::Playing).unwrap();
        let tai_caps = gst::Caps::builder("timestamp/x-tai").build();
        let mut frames = Vec::new();
        while let Some(s) = sink.try_pull_sample(gst::ClockTime::from_seconds(20)) {
            let b = s.buffer().unwrap();
            let tai = b
                .iter_meta::<gst::ReferenceTimestampMeta>()
                .find(|m| m.reference().can_intersect(&tai_caps))
                .map(|m| m.timestamp().nseconds())
                .unwrap();
            frames.push((b.pts().unwrap().nseconds(), tai));
        }
        assert!(sink.is_eos(), "the export ended without end of stream");
        pipeline.set_state(gst::State::Null).unwrap();
        bus.disconnect(handler);
        frames
    }

    #[test]
    fn an_offline_clip_is_every_frame_once_then_end_of_stream() {
        let frames = export(1.0);
        // [1 s, 3 s) of 25p: 50 frames, PTS from 0, each source frame once.
        assert_eq!(frames.len(), 50);
        for (k, (pts, tai)) in frames.iter().enumerate() {
            let k = k as u64;
            assert_eq!(*pts, k * 40_000_000);
            assert_eq!(*tai, 1_800_000_001_000_000_000 + k * 40_000_000);
        }
    }

    #[test]
    fn an_offline_clip_at_half_speed_shows_each_frame_twice() {
        let frames = export(0.5);
        assert_eq!(frames.len(), 100);
        for (k, (pts, tai)) in frames.iter().enumerate() {
            let k = k as u64;
            assert_eq!(*pts, k * 40_000_000);
            assert_eq!(*tai, 1_800_000_001_000_000_000 + k / 2 * 40_000_000);
        }
    }

    /// A switch stays in the player's store: a TAMS player does not take a
    /// local directory or another gateway (its `api_token` was given for
    /// its own), and the source must parse.
    #[test]
    fn a_switch_stays_in_the_players_store() {
        let tams = |g: &str| source::parse_source(&format!("tams+{}/flows/a", g), "/m").unwrap();
        let local = source::parse_source("cam1", "/m").unwrap();
        let gw = "https://gw.example.com";
        assert!(same_store(&tams(gw), &tams(&format!("{}/", gw))));
        assert!(!same_store(&tams(gw), &tams("https://other.example.com")));
        assert!(!same_store(&tams(gw), &local));
        assert!(!same_store(&local, &tams(gw)));
        assert!(same_store(
            &local,
            &source::parse_source("cam2", "/m").unwrap()
        ));

        let props = props(&[("api_token", st("SECRET"))]);
        let open = source_opener(&props, "/m", Uuid::nil(), tams(gw));
        let err = |uri: &str| match open(uri) {
            Err(e) => e,
            Ok(_) => panic!("{} opened", uri),
        };
        let e = err("tams+https://other.example.com/flows/b");
        assert!(e.contains("not in the player's store"), "{}", e);
        assert!(!e.contains("SECRET"));
        assert!(err("cam2").contains("not in the player's store"));
        assert!(err("tams+ftp://gw.example.com/flows/b").contains("unsupported"));
        let ok = open("tams+https://gw.example.com/flows/b?k=1").unwrap();
        assert_eq!(
            ok.ident.source.as_deref(),
            Some("tams+https://gw.example.com/flows/b")
        );
        assert_eq!(ok.ident.source_id.as_deref(), Some("b"));
    }

    /// The luma of the top left pixel of an NV12 frame.
    fn luma(s: &gst::Sample) -> u8 {
        let info = gstreamer_video::VideoInfo::from_caps(s.caps().unwrap()).unwrap();
        let frame =
            gstreamer_video::VideoFrameRef::from_buffer_ref_readable(s.buffer().unwrap(), &info)
                .unwrap();
        frame.plane_data(0).unwrap()[0]
    }

    /// The next event of a kind `want` picks, within `within`.
    fn wait_event<T>(
        events: &mut tokio::sync::broadcast::Receiver<strom_types::replay::ReplayEvent>,
        within: std::time::Duration,
        want: impl Fn(&strom_types::replay::ReplayEvent) -> Option<T>,
    ) -> Option<T> {
        let t0 = std::time::Instant::now();
        while t0.elapsed() < within {
            match events.try_recv() {
                Ok(ev) => {
                    if let Some(t) = want(&ev) {
                        return Some(t);
                    }
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        None
    }

    /// A running player switches source with the `source` command, without
    /// a rebuild: the same pipeline goes on, and the picture changes from
    /// the old source to the new one. A switch to a source that is not on
    /// real TAI gives up and the old source stays.
    #[test]
    fn a_running_player_switches_source_without_a_rebuild() {
        use strom_types::replay::{ReplayEventKind, ReplayTimeDomain};
        gst::init().unwrap();
        let root = tempfile::tempdir().unwrap();
        // Black and white on the same TAI range, and one not on real TAI.
        write_segments_as(root.path(), "black", "black", true);
        write_segments_as(root.path(), "white", "white", true);
        write_segments_as(root.path(), "loose", "white", false);
        let flow_id = Uuid::new_v4();
        let props = props(&[
            ("_media_path", st(root.path().to_str().unwrap())),
            ("_flow_id", st(&flow_id.to_string())),
            (SOURCE_PROPERTY, st("black")),
            (GROUP_PROPERTY, st("g")),
        ]);
        let built = build("switch_test", &props).unwrap();
        let pipeline = gst::Pipeline::new();
        let mut elements = HashMap::new();
        for (id, e) in &built.elements {
            pipeline.add(e).unwrap();
            elements.insert(id.clone(), e.clone());
        }
        for (a, b) in &built.internal_links {
            elements[&a.element_id]
                .link_pads(
                    a.pad_name.as_deref(),
                    &elements[&b.element_id],
                    b.pad_name.as_deref(),
                )
                .unwrap();
        }
        // System memory, so the test can read the pixels.
        let sink = gst_app::AppSink::builder()
            .caps(&gst::Caps::builder("video/x-raw").build())
            .sync(false)
            .build();
        pipeline.add(&sink).unwrap();
        elements["switch_test:video_out"].link(&sink).unwrap();
        let bus = pipeline.bus().unwrap();
        let handler = (built.bus_message_handler.unwrap())(
            &bus,
            flow_id,
            crate::events::EventBroadcaster::default(),
        );
        pipeline.set_state(gst::State::Playing).unwrap();
        let tl = timeline::get(flow_id, "g").unwrap();
        let mut events = tl.subscribe();
        let pull = || {
            sink.try_pull_sample(gst::ClockTime::from_seconds(10))
                .map(|s| luma(&s))
        };
        // The old source: black.
        let t0 = std::time::Instant::now();
        while pull().is_none_or(|y| y > 40) {
            assert!(t0.elapsed().as_secs() < 20, "no black picture");
        }
        assert_eq!(tl.state().time_domain, ReplayTimeDomain::Tai);
        let switch = |source: &str, client_ref: &str| {
            tl.apply(
                timeline::parse_command(serde_json::json!({
                    "cmd": "source", "source": source, "client_ref": client_ref
                }))
                .unwrap(),
            )
        };

        // Not on real TAI: refused by the player once it reads the index.
        switch("loose", "c1").unwrap();
        let failed = wait_event(
            &mut events,
            std::time::Duration::from_secs(15),
            |ev| match &ev.kind {
                ReplayEventKind::SourceFailed { error, .. } => {
                    Some((error.clone(), ev.client_ref.clone()))
                }
                ReplayEventKind::SourceChanged { .. } => panic!("switched to a loose source"),
                _ => None,
            },
        )
        .expect("no source_failed");
        assert!(failed.0.contains("TAI"), "{}", failed.0);
        assert_eq!(failed.1.as_deref(), Some("c1"));
        assert!(pull().unwrap() < 40, "the old source did not stay");
        assert_eq!(tl.state().angles[0].source.as_deref(), Some("black"));

        // On TAI: the same pipeline shows the new source.
        switch("white", "c2").unwrap();
        let changed = wait_event(
            &mut events,
            std::time::Duration::from_secs(15),
            |ev| match &ev.kind {
                ReplayEventKind::SourceChanged { block, source } => {
                    Some((block.clone(), source.clone(), ev.client_ref.clone()))
                }
                ReplayEventKind::SourceFailed { error, .. } => panic!("switch failed: {}", error),
                _ => None,
            },
        )
        .expect("no source_changed");
        assert_eq!(
            changed,
            (
                "switch_test".to_string(),
                Some("white".to_string()),
                Some("c2".to_string())
            )
        );
        let t0 = std::time::Instant::now();
        while pull().is_none_or(|y| y < 200) {
            assert!(
                t0.elapsed().as_secs() < 10,
                "no white picture after the switch"
            );
        }
        let state = tl.state();
        assert_eq!(state.angles[0].source.as_deref(), Some("white"));
        assert_eq!(state.angles[0].pending_source, None);
        assert_eq!(
            tl.info(flow_id, "g").angles[0].source_id.as_deref(),
            Some("white")
        );
        pipeline.set_state(gst::State::Null).unwrap();
        bus.disconnect(handler);
    }

    /// A running player plays a queue of clips: the first from a `cue`, the
    /// next on another source (`cue_next {source}`), the last on the source
    /// then shown. Each clip is prepared while the one before plays and is
    /// shown from the tick its turn comes: the last frame before the out,
    /// then the next clip's in point, no frame dropped or repeated, each
    /// frame from the right source. The last clip freezes at its out.
    #[test]
    fn a_running_player_plays_queued_clips_back_to_back() {
        use strom_types::replay::ReplayEventKind;
        const BASE: u64 = 1_800_000_000_000_000_000;
        const MS: u64 = 1_000_000;
        const FRAME: u64 = 40 * MS;
        gst::init().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_segments_as(root.path(), "black", "black", true);
        write_segments_as(root.path(), "white", "white", true);
        let flow_id = Uuid::new_v4();
        let props = props(&[
            ("_media_path", st(root.path().to_str().unwrap())),
            ("_flow_id", st(&flow_id.to_string())),
            (SOURCE_PROPERTY, st("black")),
            (GROUP_PROPERTY, st("g")),
        ]);
        let built = build("queue_test", &props).unwrap();
        let pipeline = gst::Pipeline::new();
        let mut elements = HashMap::new();
        for (id, e) in &built.elements {
            pipeline.add(e).unwrap();
            elements.insert(id.clone(), e.clone());
        }
        for (a, b) in &built.internal_links {
            elements[&a.element_id]
                .link_pads(
                    a.pad_name.as_deref(),
                    &elements[&b.element_id],
                    b.pad_name.as_deref(),
                )
                .unwrap();
        }
        let sink = gst_app::AppSink::builder()
            .caps(&gst::Caps::builder("video/x-raw").build())
            .sync(false)
            .build();
        pipeline.add(&sink).unwrap();
        elements["queue_test:video_out"].link(&sink).unwrap();
        let bus = pipeline.bus().unwrap();
        let handler = (built.bus_message_handler.unwrap())(
            &bus,
            flow_id,
            crate::events::EventBroadcaster::default(),
        );
        pipeline.set_state(gst::State::Playing).unwrap();
        let tl = timeline::get(flow_id, "g").unwrap();
        let mut events = tl.subscribe();
        let tai_caps = gst::Caps::builder("timestamp/x-tai").build();
        // (TAI of the frame shown, luma)
        let pull = || {
            sink.try_pull_sample(gst::ClockTime::from_seconds(10))
                .map(|s| {
                    let tai = s
                        .buffer()
                        .unwrap()
                        .iter_meta::<gst::ReferenceTimestampMeta>()
                        .find(|m| m.reference().can_intersect(&tai_caps))
                        .map(|m| m.timestamp().nseconds())
                        .unwrap();
                    (tai, luma(&s))
                })
        };
        let t0 = std::time::Instant::now();
        while pull().is_none_or(|(_, y)| y > 40) {
            assert!(t0.elapsed().as_secs() < 20, "no black picture");
        }
        let cmd = |v: serde_json::Value| {
            tl.apply(timeline::parse_command(v).unwrap()).unwrap();
        };
        let t = |ms: u64| (BASE + ms * MS).to_string();
        // a: black [0.5 s, 1.5 s); b: white [2 s, 3 s); c: [0.2 s, 0.6 s),
        // on the source shown then (white).
        cmd(
            serde_json::json!({"cmd": "cue", "in": {"t": t(500)}, "out": {"t": t(1500)},
            "client_ref": "a"}),
        );
        cmd(
            serde_json::json!({"cmd": "cue_next", "in": {"t": t(2000)}, "out": {"t": t(3000)},
            "source": "white", "client_ref": "b"}),
        );
        cmd(
            serde_json::json!({"cmd": "cue_next", "in": {"t": t(200)}, "out": {"t": t(600)},
            "client_ref": "c"}),
        );
        // Give the first clip time to decode at its in point.
        std::thread::sleep(std::time::Duration::from_millis(500));
        cmd(serde_json::json!({"cmd": "play"}));
        let mut frames = Vec::new();
        let t0 = std::time::Instant::now();
        // Until c has frozen at its last frame for a while.
        while frames
            .iter()
            .rev()
            .take(10)
            .filter(|(tai, _)| *tai == BASE + 560 * MS)
            .count()
            < 10
        {
            assert!(t0.elapsed().as_secs() < 20, "the queue did not play out");
            frames.push(pull().expect("no picture"));
        }
        // From the first frame of a on: 0.5 s is inside the 25p frame that
        // starts at 0.48 s, which the cue held, paused, before play (so a
        // starts the frame before its second one).
        let first = frames
            .iter()
            .position(|(tai, _)| *tai == BASE + 520 * MS)
            .expect("clip a not shown")
            - 1;
        let shown: Vec<(u64, u8)> = frames[first..].to_vec();
        let mut want: Vec<(u64, bool)> = Vec::new();
        want.extend((0..25).map(|k| (BASE + 480 * MS + k * FRAME, false)));
        want.extend((0..25).map(|k| (BASE + 2000 * MS + k * FRAME, true)));
        want.extend((0..10).map(|k| (BASE + 200 * MS + k * FRAME, true)));
        let got: Vec<(u64, bool)> = shown
            .iter()
            .take(want.len())
            .map(|(tai, y)| (*tai, *y > 200))
            .collect();
        let late: u64 = tl
            .diagnostics()
            .angles
            .iter()
            .map(|a| a.late_ticks + a.misses)
            .sum();
        assert_eq!(
            got, want,
            "frames shown (tai, white) differ ({} late ticks or misses)",
            late
        );
        for (_, y) in &shown[..25] {
            assert!(*y < 40, "clip a not black");
        }
        // The events: each clip's start and end, and the switch for b.
        let mut seen = Vec::new();
        while let Ok(ev) = events.try_recv() {
            let what = match &ev.kind {
                ReplayEventKind::ItemStarted { .. } => "item_started",
                ReplayEventKind::Ended { .. } => "ended",
                ReplayEventKind::SourceChanged { .. } => "source_changed",
                ReplayEventKind::SourceFailed { error, .. } => panic!("switch failed: {}", error),
                _ => continue,
            };
            seen.push((what, ev.client_ref.clone().unwrap_or_default()));
        }
        for e in [
            ("ended", "a"),
            ("item_started", "b"),
            ("source_changed", "b"),
            ("ended", "b"),
            ("item_started", "c"),
            ("ended", "c"),
        ] {
            assert!(
                seen.contains(&(e.0, e.1.to_string())),
                "{:?} not in {:?}",
                e,
                seen
            );
        }
        assert_eq!(
            seen.iter().filter(|e| e.0 == "source_changed").count(),
            1,
            "c on the source shown is no switch: {:?}",
            seen
        );
        let state = tl.state();
        assert_eq!(state.angles[0].source.as_deref(), Some("white"));
        assert_eq!(state.queue.clip_ref.as_deref(), Some("c"));
        assert_eq!(state.queue.waiting, 0);
        pipeline.set_state(gst::State::Null).unwrap();
        bus.disconnect(handler);
    }
}
