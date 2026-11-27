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
//! - `player`: decode scheduling, frame store and the output tick.
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
    DECODED_AHEAD_PROPERTY, GROUP_PROPERTY, OUTPUT_FRAMERATE_PROPERTY, PACING_OFFLINE,
    PACING_PROPERTY, PACING_REALTIME, PRIVATE_GROUP_PREFIX, READ_AHEAD_PROPERTY, SOURCE_PROPERTY,
};
use strom_types::replay::ReplayTimeRef;
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

/// A clip's in or out point: UTC (RFC 3339, has a `T`) or TAI
/// (`"<seconds>:<nanoseconds>"`).
pub fn parse_time_ref(s: &str) -> Result<ReplayTimeRef, String> {
    let s = s.trim();
    if s.contains('T') {
        strom_types::tai::parse_utc(s).ok_or_else(|| format!("bad UTC time '{}'", s))?;
        Ok(ReplayTimeRef {
            utc: Some(s.to_string()),
            ..Default::default()
        })
    } else {
        strom_types::tams::parse_timestamp_signed(s).ok_or_else(|| {
            format!(
                "bad time '{}', want UTC (2026-11-24T13:21:56.080Z) or TAI (<s>:<ns>)",
                s
            )
        })?;
        Ok(ReplayTimeRef {
            tai: Some(s.to_string()),
            ..Default::default()
        })
    }
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
        assert_eq!(c.out.tai.as_deref(), Some("1795600000:500000000"));
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

    /// Four 1 s segments of 25p H.264 (with `bframes` B-frames) in MPEG-TS
    /// and an `index.json`, from TAI `BASE`, under `root/segs`.
    fn write_segments(root: &std::path::Path, bframes: u32) {
        const BASE: i64 = 1_800_000_000_000_000_000;
        let p = gst::parse::launch(&format!(
            "videotestsrc num-buffers=25 ! video/x-raw,width=320,height=240,framerate=25/1 ! \
             x264enc bframes={} key-int-max=25 ! h264parse ! mpegtsmux ! \
             appsink name=sink sync=false",
            bframes
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
        let dir = root.join("segs");
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
            "segments": segs,
        });
        std::fs::write(dir.join("index.json"), index.to_string()).unwrap();
    }

    /// Run an offline player over the segments to end of stream: each
    /// buffer's PTS and the TAI of the source frame it shows.
    fn export(speed: f64) -> Vec<(u64, u64)> {
        gst::init().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_segments(root.path(), 2);
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

    /// GL frames from the player's decode pipeline convert in the flow: the
    /// decode session shares the flow's GL context through
    /// `context_forward::install`. Without it `vtdec_hw` makes a GL context
    /// of its own, and the flow's first GL converter fails on the first
    /// frame: "input memory OpenGL context is different" in the log, "Failed
    /// to convert video buffer" on the bus (#1099).
    ///
    /// The real block, an offline clip over a local `index.json`, into
    /// `glcolorconvert` (to RGBA, so it converts rather than passes through).
    /// Runs only where `vtdec_hw` exists, the one decoder that outputs GL
    /// memory (macOS: this runs in the macOS CI job); skipped elsewhere.
    #[test]
    fn gl_frames_from_the_decode_pipeline_convert_in_the_flow() {
        gst::init().unwrap();
        if ["vtdec_hw", "glcolorconvert", "gldownload", "x264enc"]
            .iter()
            .any(|f| gst::ElementFactory::find(f).is_none())
        {
            eprintln!("skipped: needs vtdec_hw and GL (macOS)");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        // No B-frames: vtdec_hw loses two frames of the first segment a
        // session decodes with them, and an offline clip waits for every
        // source frame.
        write_segments(root.path(), 0);
        let props = props(&[
            ("_media_path", st(root.path().to_str().unwrap())),
            (SOURCE_PROPERTY, st("segs")),
            (PACING_PROPERTY, st("offline")),
            (CLIP_IN_PROPERTY, st("1800000001:0")),
            (CLIP_OUT_PROPERTY, st("1800000003:0")),
        ]);
        decode::test_decoder::set(Some("vtdec_hw"));
        let built = build("gl_context_test", &props);
        decode::test_decoder::set(None);
        let built = built.unwrap();

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
        let consumer = gst::parse::bin_from_description(
            "glcolorconvert ! video/x-raw(memory:GLMemory),format=RGBA ! gldownload ! \
             appsink name=sink sync=false",
            true,
        )
        .unwrap();
        pipeline.add(&consumer).unwrap();
        elements["gl_context_test:video_out"]
            .link(&consumer)
            .unwrap();
        let sink = consumer
            .by_name("sink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        let bus = pipeline.bus().unwrap();
        let handler = (built.bus_message_handler.unwrap())(
            &bus,
            Uuid::nil(),
            crate::events::EventBroadcaster::default(),
        );
        pipeline.set_state(gst::State::Playing).unwrap();

        let mut frames = 0;
        let mut error = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline && !sink.is_eos() {
            if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
                if let gst::MessageView::Error(e) = msg.view() {
                    error = Some(format!("{} ({:?})", e.error(), e.debug()));
                }
                break;
            }
            if sink
                .try_pull_sample(gst::ClockTime::from_mseconds(50))
                .is_some()
            {
                frames += 1;
            }
        }
        pipeline.set_state(gst::State::Null).unwrap();
        bus.disconnect(handler);

        assert!(
            error.is_none(),
            "the flow's GL converter failed on the player's frames: {:?}",
            error
        );
        // [1 s, 3 s) of 25p: 50 frames, then end of stream.
        assert_eq!(frames, 50, "frames that reached the flow's sink");
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
}
