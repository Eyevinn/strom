//! Shared rig for the stinger integration tests: clips whose frames say
//! which clip frame is on air, a flow with a vision mixer and a stinger
//! source, and readers for the program output. Included with `#[path]`.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video::prelude::*;
use std::collections::HashMap;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::element::Link;
use strom_types::stinger::{StingerClipSettings, StingerVariant};
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

pub const W: u32 = 320;
pub const H: u32 = 180;
/// Clip frames; one second at 30 fps.
pub const N: usize = 30;
pub const FRAME_NS: u64 = 33_333_333;

pub const GL_ELEMENTS: &[&str] = &[
    "glvideomixerelement",
    "glshader",
    "glupload",
    "glcolorconvert",
];
/// PNG frames in QuickTime: lossless, alpha-capable, and decoded by libpng
/// (`pngdec`) without frame threads, so every frame comes out. FFV1 would be
/// smaller, but GStreamer 1.24.2's FFV1 decoder (Linux CI) drops a stream's
/// last frame now and then, which makes frame counts flaky.
pub const CODEC_ELEMENTS: &[&str] = &["pngenc", "pngdec", "qtmux", "qtdemux"];

/// Where clip frame `i`'s matte edge is: white (new source) left of it.
pub fn matte_edge(i: usize) -> u32 {
    (i as u32 * W) / (N as u32 - 1)
}

pub fn write_clip(
    path: &std::path::Path,
    width: u32,
    format: &str,
    frame: &dyn Fn(usize, &mut [u8]),
) {
    write_clip_sized(path, width, H, format, frame)
}

/// [`write_clip`] at any frame height.
pub fn write_clip_sized(
    path: &std::path::Path,
    width: u32,
    height: u32,
    format: &str,
    frame: &dyn Fn(usize, &mut [u8]),
) {
    let bpp = if format == "GRAY8" { 1 } else { 4 };
    let pipeline = gst::Pipeline::new();
    let appsrc = gst_app::AppSrc::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", format)
                .field("width", width as i32)
                .field("height", height as i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        )
        .format(gst::Format::Time)
        .is_live(false)
        .build();
    let convert = gst::ElementFactory::make("videoconvert").build().unwrap();
    let to_codec = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", if format == "GRAY8" { "GRAY8" } else { "RGBA" })
                .build(),
        )
        .build()
        .unwrap();
    let enc = gst::ElementFactory::make("pngenc")
        .property("compression-level", 1u32)
        .build()
        .unwrap();
    let mux = gst::ElementFactory::make("qtmux").build().unwrap();
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path.to_string_lossy().to_string())
        .build()
        .unwrap();
    pipeline
        .add_many([appsrc.upcast_ref(), &convert, &to_codec, &enc, &mux, &sink])
        .unwrap();
    gst::Element::link_many([appsrc.upcast_ref(), &convert, &to_codec, &enc, &mux, &sink]).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    for i in 0..N {
        let mut data = vec![0u8; (width * height) as usize * bpp];
        frame(i, &mut data);
        let mut buf = gst::Buffer::from_mut_slice(data);
        {
            let b = buf.get_mut().unwrap();
            b.set_pts(gst::ClockTime::from_nseconds(i as u64 * FRAME_NS));
            b.set_duration(gst::ClockTime::from_nseconds(FRAME_NS));
        }
        appsrc.push_buffer(buf).unwrap();
    }
    appsrc.end_of_stream().unwrap();
    let msg = pipeline.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(
        matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
        "clip encode failed"
    );
}

/// Classic: opaque green on the left half, transparent on the right.
pub fn classic_clip(path: &std::path::Path) {
    write_clip(path, W, "BGRA", &|_, d| {
        for (i, px) in d.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            if (i as u32 % W) < W / 2 {
                px.copy_from_slice(&[0, 255, 0, 255]);
            }
        }
    });
}

/// [`classic_clip`] at 1920x1080: long enough to analyse that a take
/// straight after adding it finds the analysis still running.
pub fn big_classic_clip(path: &std::path::Path) {
    let (w, h) = (1920u32, 1080u32);
    write_clip_sized(path, w, h, "BGRA", &|_, d| {
        for (i, px) in d.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            if (i as u32 % w) < w / 2 {
                px.copy_from_slice(&[0, 255, 0, 255]);
            }
        }
    });
}

/// [`classic_clip`] with a stereo PCM track that starts `video_delay_ns`
/// before the video: the video's first frame is stamped that late, so the
/// file carries an edit list and the demuxer hands video out at that running
/// time, audio at zero.
pub fn classic_clip_with_early_audio(path: &std::path::Path, video_delay_ns: u64) {
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=v format=time caps=video/x-raw,format=BGRA,width={W},height={H},framerate=30/1 \
           ! videoconvert ! video/x-raw,format=RGBA ! pngenc compression-level=1 ! queue ! mux. \
         audiotestsrc num-buffers={} samplesperbuffer=480 \
           ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! queue ! mux. \
         qtmux name=mux ! filesink location=\"{}\"",
        (video_delay_ns / 10_000_000) as usize + N * 4,
        path.display()
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let appsrc = pipeline
        .by_name("v")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    for i in 0..N {
        let mut data = vec![0u8; (W * H * 4) as usize];
        for (j, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            if (j as u32 % W) < W / 2 {
                px.copy_from_slice(&[0, 255, 0, 255]);
            }
        }
        let mut buf = gst::Buffer::from_mut_slice(data);
        {
            let b = buf.get_mut().unwrap();
            b.set_pts(gst::ClockTime::from_nseconds(
                video_delay_ns + i as u64 * FRAME_NS,
            ));
            b.set_duration(gst::ClockTime::from_nseconds(FRAME_NS));
        }
        appsrc.push_buffer(buf).unwrap();
    }
    appsrc.end_of_stream().unwrap();
    let msg = pipeline.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(
        matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
        "clip encode failed"
    );
}

/// Track matte, side by side: a 20 px yellow marker top left on the graphic,
/// and a matte whose edge moves `matte_edge` per frame.
pub fn sbs_clip(path: &std::path::Path) {
    write_clip(path, 2 * W, "BGRA", &|f, d| {
        for (i, px) in d.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i as u32 % (2 * W), i as u32 / (2 * W));
            if x < W {
                if x < 20 && y < 20 {
                    px.copy_from_slice(&[0, 255, 255, 255]);
                }
            } else {
                let v = if x - W < matte_edge(f) { 255 } else { 0 };
                px.copy_from_slice(&[v, v, v, 255]);
            }
        }
    });
}

/// Mask only: the same moving edge over the whole frame.
pub fn mask_clip(path: &std::path::Path) {
    write_clip(path, W, "GRAY8", &|f, d| {
        for (i, px) in d.iter_mut().enumerate() {
            *px = if (i as u32 % W) < matte_edge(f) {
                255
            } else {
                0
            };
        }
    });
}

/// The classic clip as ProRes 4444: 10-bit 4:4:4 with alpha, which a GL
/// upload does not take as it is, and which a hardware decoder (VideoToolbox)
/// would decode without its alpha.
pub fn prores_classic_clip(path: &std::path::Path) {
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=src ! videoconvert ! video/x-raw,format=A444_10LE ! avenc_prores_ks profile=4444 ! qtmux ! filesink location=\"{}\"",
        path.display()
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let appsrc = pipeline
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    appsrc.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", W as i32)
            .field("height", H as i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .build(),
    ));
    appsrc.set_format(gst::Format::Time);
    pipeline.set_state(gst::State::Playing).unwrap();
    for i in 0..N {
        let mut data = vec![0u8; (W * H * 4) as usize];
        for (j, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            if (j as u32 % W) < W / 2 {
                px.copy_from_slice(&[0, 255, 0, 255]);
            }
        }
        let mut buf = gst::Buffer::from_mut_slice(data);
        {
            let b = buf.get_mut().unwrap();
            b.set_pts(gst::ClockTime::from_nseconds(i as u64 * FRAME_NS));
            b.set_duration(gst::ClockTime::from_nseconds(FRAME_NS));
        }
        appsrc.push_buffer(buf).unwrap();
    }
    appsrc.end_of_stream().unwrap();
    let msg = pipeline.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(
        matches!(msg.map(|m| m.type_()), Some(gst::MessageType::Eos)),
        "ProRes encode failed"
    );
}

pub fn mixer_id(tag: &str) -> String {
    format!("mixer-{tag}")
}

pub fn build_flow(tag: &str, backend: &str, clips: &[std::path::PathBuf]) -> Flow {
    let mut flow = Flow::new(format!("stinger_{tag}"));
    let mixer = mixer_id(tag);
    let props = |pairs: Vec<(&str, PV)>| -> HashMap<String, PV> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    };
    flow.blocks.push(strom_types::BlockInstance {
        id: mixer.clone(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: props(vec![
            ("compositor_preference", PV::String(backend.into())),
            ("num_inputs", PV::UInt(2)),
            ("pgm_resolution", PV::String(format!("{W}x{H}"))),
            ("multiview_resolution", PV::String(format!("{W}x{H}"))),
            ("pgm_framerate", PV::String("30/1".into())),
            ("enable_stinger", PV::Bool(true)),
        ]),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    let playlist: Vec<String> = clips
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    flow.blocks.push(strom_types::BlockInstance {
        id: format!("sting-{tag}"),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: props(vec![
            ("stinger_mode", PV::Bool(true)),
            (
                "playlist",
                PV::String(serde_json::to_string(&playlist).unwrap()),
            ),
            ("num_audio_tracks", PV::UInt(0)),
        ]),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    let elem = |id: &str, ty: &str, p: Vec<(&str, PV)>| strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: p.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    };
    let caps = format!("video/x-raw,width={W},height={H},framerate=30/1");
    // Input 0 red, input 1 blue.
    for (id, colour) in [("src0", 0xffff0000u64), ("src1", 0xff0000ffu64)] {
        flow.elements.push(elem(
            id,
            "videotestsrc",
            vec![
                ("pattern", PV::String("solid-color".into())),
                ("foreground-color", PV::UInt(colour)),
                ("is-live", PV::Bool(true)),
            ],
        ));
    }
    for id in ["caps0", "caps1"] {
        flow.elements.push(elem(
            id,
            "capsfilter",
            vec![("caps", PV::String(caps.clone()))],
        ));
    }
    flow.elements.push(elem("pgmconv", "videoconvert", vec![]));
    flow.elements.push(elem(
        "pgmcaps",
        "capsfilter",
        vec![("caps", PV::String("video/x-raw,format=RGBA".into()))],
    ));
    flow.elements.push(elem(
        "pgmsink",
        "appsink",
        vec![
            ("sync", PV::Bool(false)),
            ("max-buffers", PV::UInt(2000)),
            ("drop", PV::Bool(true)),
        ],
    ));
    for (from, to) in [
        ("src0:src".to_string(), "caps0:sink".to_string()),
        ("caps0:src".to_string(), format!("{mixer}:video_in_0")),
        ("src1:src".to_string(), "caps1:sink".to_string()),
        ("caps1:src".to_string(), format!("{mixer}:video_in_1")),
        (
            format!("sting-{tag}:video_out"),
            format!("{mixer}:stinger_in"),
        ),
        (format!("{mixer}:pgm_out"), "pgmconv:sink".to_string()),
        ("pgmconv:src".to_string(), "pgmcaps:sink".to_string()),
        ("pgmcaps:src".to_string(), "pgmsink:sink".to_string()),
    ] {
        flow.links.push(Link { from, to });
    }
    flow
}

pub struct Running {
    pub tag: String,
    pub state: AppState,
    pub flow_id: strom_types::FlowId,
    pub _storage: NamedTempFile,
    pub _blocks: NamedTempFile,
    pub _dir: tempfile::TempDir,
}

impl Running {
    pub fn mixer(&self) -> String {
        mixer_id(&self.tag)
    }

    pub async fn sink(&self) -> gst_app::AppSink {
        let pipelines = self.state.pipelines_read().await;
        pipelines
            .get(&self.flow_id)
            .and_then(|m| m.pipeline().by_name("pgmsink"))
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .expect("pgm appsink")
    }

    pub async fn drain(&self) {
        let sink = self.sink().await;
        while sink
            .try_pull_sample(gst::ClockTime::from_mseconds(1))
            .is_some()
        {}
    }

    /// PGM frames, timestamp and packed RGBA, spanning the next `ms` of
    /// stream time. Bounded by stream time, not wall time: a slow runner's
    /// debug-build mixer falls behind real time, and a wall-clock window then
    /// ends before the clip does. The wall deadline only stops a stalled
    /// pipeline.
    pub async fn collect(&self, ms: u64) -> Vec<(u64, Vec<u8>)> {
        let sink = self.sink().await;
        let span = ms * 1_000_000;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms + 10_000);
        let mut frames: Vec<(u64, Vec<u8>)> = Vec::new();
        while std::time::Instant::now() < deadline
            && frames
                .first()
                .zip(frames.last())
                .is_none_or(|((first, _), (last, _))| last - first < span)
        {
            let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) else {
                continue;
            };
            let info = gstreamer_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
            let buffer = sample.buffer().unwrap();
            let frame =
                gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).unwrap();
            let stride = frame.plane_stride()[0] as usize;
            let src = frame.plane_data(0).unwrap();
            let mut packed = vec![0u8; (W * H * 4) as usize];
            for y in 0..H as usize {
                let row = y * W as usize * 4;
                packed[row..row + W as usize * 4]
                    .copy_from_slice(&src[y * stride..y * stride + W as usize * 4]);
            }
            frames.push((buffer.pts().unwrap().nseconds(), packed));
        }
        frames
    }

    pub async fn wait_until_parked(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let s = self
                .state
                .stinger_state(&self.flow_id, &self.mixer())
                .await
                .unwrap();
            if s.ready && !s.running {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "stinger never ready: {:?}",
                s.problem
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Wait until the take finished and its report is in.
    /// Every dist mixer sink pad: what feeds it, and its z-order, alpha and
    /// blend settings. For failure messages.
    pub async fn mixer_pads(&self) -> String {
        mixer_pads_of(&self.state, self.flow_id, &self.mixer()).await
    }

    /// Frames clip `index` decodes to. GStreamer 1.24.2's FFV1 decoder drops
    /// the last frame of a stream (fixed in 1.24.4), so this is read from the
    /// analysis instead of assumed.
    pub async fn clip_frames(&self, index: usize) -> u32 {
        let s = self
            .state
            .stinger_state(&self.flow_id, &self.mixer())
            .await
            .unwrap();
        let frames = s.clips[index].info.as_ref().expect("analysed").frames;
        assert!(
            frames + 1 >= N as u32 && frames <= N as u32,
            "clip {index} decodes to {frames} frames"
        );
        frames
    }

    pub async fn wait_for_report(&self, take: usize) -> strom_types::stinger::StingerTakeReport {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let s = self
                .state
                .stinger_state(&self.flow_id, &self.mixer())
                .await
                .unwrap();
            if let (false, Some(r)) = (s.running, s.last_take.clone()) {
                if r.index == take {
                    return r;
                }
            }
            assert!(std::time::Instant::now() < deadline, "take never finished");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Analysis has run for every clip, so takes plan from it.
    pub async fn wait_for_analysis(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let s = self
                .state
                .stinger_state(&self.flow_id, &self.mixer())
                .await
                .unwrap();
            if !s.clips.is_empty() && s.clips.iter().all(|c| c.info.is_some()) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "analysis never ran");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

pub async fn start(tag: &str, backend: &str) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let clips = vec![
        dir.path().join("classic.mov"),
        dir.path().join("sbs.mov"),
        dir.path().join("mask.mov"),
    ];
    classic_clip(&clips[0]);
    sbs_clip(&clips[1]);
    mask_clip(&clips[2]);
    start_with(tag, backend, dir, clips).await
}

pub async fn start_with(
    tag: &str,
    backend: &str,
    dir: tempfile::TempDir,
    clips: Vec<std::path::PathBuf>,
) -> Running {
    start_edited(tag, backend, dir, clips, |_| {}).await
}

/// [`start_with`], with the flow edited before it starts.
pub async fn start_edited(
    tag: &str,
    backend: &str,
    dir: tempfile::TempDir,
    clips: Vec<std::path::PathBuf>,
    edit: impl FnOnce(&mut Flow),
) -> Running {
    if std::env::var("STINGER_TEST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("STINGER_TEST_LOG").unwrap())
            .with_test_writer()
            .try_init();
    }
    let _ = strom::gpu::detect_gpu_capabilities();

    let storage = NamedTempFile::new().unwrap();
    let blocks = NamedTempFile::new().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(storage.path()),
        blocks.path(),
        dir.path(),
        vec![],
        "all".to_string(),
        vec![],
        false,
        false,
    );
    let mut flow = build_flow(tag, backend, &clips);
    edit(&mut flow);
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert");
    state.start_flow(&flow_id).await.expect("start");
    let running = Running {
        tag: tag.to_string(),
        state,
        flow_id,
        _storage: storage,
        _blocks: blocks,
        _dir: dir,
    };
    running.wait_until_parked().await;
    running.wait_for_analysis().await;
    running
}

pub fn px(frame: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    Red,
    Blue,
    Green,
    Yellow,
    Other,
}

pub fn colour(p: [u8; 4]) -> Colour {
    let [r, g, b, _] = p;
    match (r > 180, g > 180, b > 180, r < 80, g < 80, b < 80) {
        (true, _, _, _, true, true) => Colour::Red,
        (_, _, true, true, true, _) => Colour::Blue,
        (_, true, _, true, _, true) => Colour::Green,
        (true, true, _, _, _, true) => Colour::Yellow,
        _ => Colour::Other,
    }
}

/// Where the program changes from `new` (left) to `old` (right) on the
/// middle row.
pub fn edge(frame: &[u8], new: Colour) -> u32 {
    (0..W)
        .find(|&x| colour(px(frame, x, H / 2)) != new)
        .unwrap_or(W)
}

pub fn frame_index(pts: u64, start: u64) -> i64 {
    ((pts as i64 - start as i64) as f64 / FRAME_NS as f64).round() as i64
}

/// Classic: the graphic is on air for exactly the clip's frames, and the
/// program cuts beneath it on the cut point's frame.
pub async fn classic_take(r: &Running) {
    r.state
        .stinger_set_clip_settings(
            &r.flow_id,
            &r.mixer(),
            0,
            None,
            StingerClipSettings {
                cut_point_ms: Some(500),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    r.drain().await;
    let take = r
        .state
        .stinger_take(&r.flow_id, &r.mixer(), Some(0), None)
        .await
        .expect("take");
    assert_eq!(take.variant, StingerVariant::Classic);
    let n = r.clip_frames(0).await;
    let frames = r.collect(take.take_to_air_ms as u64 + 1600).await;

    let green: Vec<u64> = frames
        .iter()
        .filter(|(_, f)| colour(px(f, W / 4, H / 2)) == Colour::Green)
        .map(|(t, _)| *t)
        .collect();
    let start = *green.first().expect("the graphic never went on air");
    let indices: Vec<i64> = green.iter().map(|t| frame_index(*t, start)).collect();
    assert_eq!(
        indices,
        (0..n as i64).collect::<Vec<_>>(),
        "the graphic must be on air for exactly its {} frames",
        n
    );
    let cut = frames
        .iter()
        .find(|(t, f)| *t >= start && colour(px(f, 3 * W / 4, H / 2)) == Colour::Blue)
        .map(|(t, _)| frame_index(*t, start))
        .expect("the program never cut");
    assert_eq!(cut, 15, "the cut lands on the cut point's frame (500 ms)");
    // Before the cut the old source, after the clip the new one everywhere.
    for (t, f) in &frames {
        let k = frame_index(*t, start);
        let right = colour(px(f, 3 * W / 4, H / 2));
        if k < 15 {
            assert_eq!(right, Colour::Red, "frame {k}: old source before the cut");
        } else {
            assert_eq!(right, Colour::Blue, "frame {k}: new source after the cut");
        }
        if k >= n as i64 {
            assert_eq!(colour(px(f, W / 4, H / 2)), Colour::Blue, "frame {k}");
        }
    }

    let report = r.wait_for_report(0).await;
    assert_eq!(report.frames_expected, n);
    assert_eq!(report.frames_arrived, n, "{report:?}");
    assert_eq!(report.warning, None, "{report:?}");
    eprintln!(
        "classic: on air {:.0} ms after the take, {} late, worst margin {:?} ms",
        report.take_to_air_ms, report.frames_late, report.worst_margin_ms
    );
}

/// [`Running::mixer_pads`] for a flow, callable from a task of its own.
pub async fn mixer_pads_of(
    state: &AppState,
    flow_id: strom_types::FlowId,
    mixer_block: &str,
) -> String {
    let pipelines = state.pipelines_read().await;
    let Some(mixer) = pipelines
        .get(&flow_id)
        .and_then(|m| m.pipeline().by_name(&format!("{}:mixer", mixer_block)))
    else {
        return "no mixer".into();
    };
    let enum_nick = |pad: &gst::Pad, prop: &str| -> String {
        if pad.find_property(prop).is_none() {
            return "-".into();
        }
        let v = pad.property_value(prop);
        gst::glib::EnumValue::from_value(&v)
            .map(|(_, e)| e.nick().to_string())
            .unwrap_or_else(|| "?".into())
    };
    mixer
        .sink_pads()
        .iter()
        .map(|pad| {
            let peer_element = pad.peer().and_then(|p| p.parent_element());
            let peer = peer_element
                .as_ref()
                .map(|e| e.name().to_string())
                .unwrap_or_default();
            // A glshader peer's uniforms, to see what it is drawing with.
            let uniforms = peer_element
                .as_ref()
                .filter(|e| e.find_property("uniforms").is_some())
                .and_then(|e| e.property::<Option<gst::Structure>>("uniforms"))
                .map(|s| format!(" uniforms={s}"))
                .unwrap_or_default();
            let caps = pad
                .current_caps()
                .and_then(|c| c.structure(0).map(|s| s.to_string()))
                .unwrap_or_else(|| "none".into());
            format!(
                "{} <- {} z={} a={:.2} pos={},{} size={}x{} src={} dst={} eqa={} srca={} dsta={}{} caps={}",
                pad.name(),
                peer,
                pad.property::<u32>("zorder"),
                pad.property::<f64>("alpha"),
                pad.property::<i32>("xpos"),
                pad.property::<i32>("ypos"),
                pad.property::<i32>("width"),
                pad.property::<i32>("height"),
                enum_nick(pad, "blend-function-src-rgb"),
                enum_nick(pad, "blend-function-dst-rgb"),
                enum_nick(pad, "blend-equation-alpha"),
                enum_nick(pad, "blend-function-src-alpha"),
                enum_nick(pad, "blend-function-dst-alpha"),
                uniforms,
                caps,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
