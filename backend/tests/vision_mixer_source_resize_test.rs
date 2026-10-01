//! Regression tests: a source that changes shape mid-stream must be
//! aspect-fitted in its new shape on the GPU (OpenGL) vision mixer, also
//! across takes and Fade to Black, and Fade to Black must play out in full.
//!
//! Builds a real vision mixer flow through `PipelineManager` with a white
//! source on input 0 and measures the white area of PGM. A 720x1280 source on
//! a 1280x720 canvas is pillarboxed to about a third of the width; stretched,
//! PGM is entirely white.

use gstreamer::prelude::*;
use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

/// A take index for `trigger_transition`, which takes `usize` on main and
/// `Option<usize>` once #806 lands. Either signature accepts this.
fn idx<T: From<usize>>(i: usize) -> T {
    T::from(i)
}

/// Same probe as `vision_mixer_fx_test`: a trivial GL run must reach EOS.
/// Having the GL plugins installed is not enough on headless runners.
fn gl_environment_available() -> bool {
    if gstreamer::ElementFactory::find("glvideomixerelement").is_none()
        || gstreamer::ElementFactory::find("glshader").is_none()
        || gstreamer::ElementFactory::find("gltestsrc").is_none()
    {
        return false;
    }
    let Ok(pipeline) = gstreamer::parse::launch(
        "gltestsrc num-buffers=3 ! video/x-raw(memory:GLMemory),format=RGBA,width=64,height=64,framerate=30/1 ! fakesink sync=false",
    ) else {
        return false;
    };
    let Ok(pipeline) = pipeline.downcast::<gstreamer::Pipeline>() else {
        return false;
    };
    if pipeline.set_state(gstreamer::State::Playing).is_err() {
        return false;
    }
    let bus = pipeline.bus().expect("pipeline has a bus");
    let ok = matches!(
        bus.timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(20),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        ),
        Some(msg) if matches!(msg.view(), gstreamer::MessageView::Eos(_))
    );
    let _ = pipeline.set_state(gstreamer::State::Null);
    ok
}

/// Skip unless GL works; `STROM_REQUIRE_GL=1` (set on the macOS CI job)
/// turns the skip into a failure.
fn gl_available_or_required() -> bool {
    if gl_environment_available() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GL").is_none(),
        "STROM_REQUIRE_GL is set but no GL context could be created — this platform \
         is supposed to render, so a skip here would hide a GL regression"
    );
    eprintln!("SKIP: GL environment unavailable (no context or GL elements missing)");
    false
}

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

const LANDSCAPE: (i32, i32) = (1280, 720);
const PORTRAIT: (i32, i32) = (720, 1280);

fn source_caps((width, height): (i32, i32)) -> gstreamer::Caps {
    gstreamer::Caps::builder("video/x-raw")
        .field("width", width)
        .field("height", height)
        .field("framerate", gstreamer::Fraction::new(30, 1))
        .build()
}

/// A 720x1280 source aspect-fitted on 1280x720 covers 405/1280 of the width.
fn pillarboxed(white: f64) -> bool {
    (0.25..0.40).contains(&white)
}

/// Mixer state is global per block id, so each test needs its own.
fn build_flow(block_id: &str, first_shape: (i32, i32)) -> Flow {
    let mut flow = Flow::new("vm_source_resize");
    flow.blocks.push(strom_types::BlockInstance {
        id: block_id.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                PV::String("gpu".into()),
            );
            p.insert("num_inputs".to_string(), PV::UInt(2));
            p.insert("pgm_resolution".to_string(), PV::String("1280x720".into()));
            p.insert(
                "multiview_resolution".to_string(),
                PV::String("640x360".into()),
            );
            // Download PGM to system memory so the appsink can map pixels.
            p.insert("gl_download".to_string(), PV::Bool(true));
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow.elements.push(elem(
        "src0",
        "videotestsrc",
        vec![
            ("pattern", PV::String("white".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        "caps0",
        "capsfilter",
        vec![("caps", PV::String(source_caps(first_shape).to_string()))],
    ));
    // Input 1, on PVW: a red source for takes.
    flow.elements.push(elem(
        "src1",
        "videotestsrc",
        vec![
            ("pattern", PV::String("red".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        "caps1",
        "capsfilter",
        vec![("caps", PV::String(source_caps(LANDSCAPE).to_string()))],
    ));
    flow.elements.push(elem(
        "pgmsink",
        "appsink",
        vec![
            ("sync", PV::Bool(false)),
            ("max-buffers", PV::UInt(1)),
            ("drop", PV::Bool(true)),
        ],
    ));
    for (from, to) in [
        ("src0:src".to_string(), "caps0:sink".to_string()),
        ("caps0:src".to_string(), format!("{block_id}:video_in_0")),
        ("src1:src".to_string(), "caps1:sink".to_string()),
        ("caps1:src".to_string(), format!("{block_id}:video_in_1")),
        (format!("{block_id}:pgm_out"), "pgmsink:sink".to_string()),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }
    flow
}

/// Fraction of PGM pixels that are white.
fn white_fraction(sample: &gstreamer::Sample) -> f64 {
    pixel_fraction(sample, |px| px[0] > 200 && px[1] > 200 && px[2] > 200)
}

/// Fraction of PGM pixels that are black.
fn black_fraction(sample: &gstreamer::Sample) -> f64 {
    pixel_fraction(sample, |px| px[0] < 40 && px[1] < 40 && px[2] < 40)
}

/// Mean green level of PGM: 255 for white, 0 for red or black.
fn mean_green(sample: &gstreamer::Sample) -> f64 {
    let buffer = sample.buffer().expect("buffer");
    let map = buffer.map_readable().expect("map");
    let (sum, n) = map
        .chunks_exact(4)
        .step_by(4)
        .fold((0u64, 0u64), |(sum, n), px| (sum + px[1] as u64, n + 1));
    sum as f64 / n.max(1) as f64
}

fn pixel_fraction(sample: &gstreamer::Sample, matches: impl Fn(&[u8]) -> bool) -> f64 {
    let caps = sample.caps().expect("caps");
    let s = caps.structure(0).unwrap();
    let w = s.get::<i32>("width").unwrap() as usize;
    let h = s.get::<i32>("height").unwrap() as usize;
    let format = s.get::<&str>("format").unwrap().to_string();
    assert!(
        matches!(format.as_str(), "RGBA" | "RGBx" | "BGRA" | "BGRx"),
        "unexpected PGM format {}",
        format
    );
    let buffer = sample.buffer().expect("buffer");
    let map = buffer.map_readable().expect("map");
    let mut hits = 0u64;
    let mut total = 0u64;
    // Flat colour fields: every 4th pixel gives the same fraction, faster.
    for px in map.chunks_exact(4).take(w * h).step_by(4) {
        if matches(px) {
            hits += 1;
        }
        total += 1;
    }
    hits as f64 / total.max(1) as f64
}

/// Pull PGM frames until one's white fraction satisfies `done`, or fail
/// after 30 s.
fn wait_for_pgm(appsink: &gstreamer_app::AppSink, what: &str, done: impl Fn(f64) -> bool) -> f64 {
    wait_for_pgm_by(appsink, what, white_fraction, done)
}

fn wait_for_pgm_by(
    appsink: &gstreamer_app::AppSink,
    what: &str,
    measure: impl Fn(&gstreamer::Sample) -> f64,
    done: impl Fn(f64) -> bool,
) -> f64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut last = None;
    while std::time::Instant::now() < deadline {
        if let Some(s) = appsink.try_pull_sample(gstreamer::ClockTime::from_mseconds(500)) {
            let f = measure(&s);
            if done(f) {
                return f;
            }
            last = Some(f);
        }
    }
    panic!(
        "PGM never showed {} within 30s, last measured {:?}",
        what, last
    );
}

/// A running GPU vision mixer flow and the glib main loop its geometry
/// refresh needs: the mixer re-fits a pad's rect from an idle callback on the
/// default main context, which only runs while a main loop does. The server
/// has one.
struct Running {
    manager: PipelineManager,
    appsink: gstreamer_app::AppSink,
    main_loop: gstreamer::glib::MainLoop,
    main_loop_thread: std::thread::JoinHandle<()>,
    _registry_file: NamedTempFile,
}

impl Running {
    fn start(flow: Flow) -> Running {
        let main_loop = gstreamer::glib::MainLoop::new(None, false);
        let main_loop_thread = {
            let ml = main_loop.clone();
            std::thread::spawn(move || ml.run())
        };
        let registry_file = NamedTempFile::new().unwrap();
        let registry = BlockRegistry::new(registry_file.path());
        let events = EventBroadcaster::with_capacity(10);
        let mut manager = PipelineManager::new(
            &flow,
            events,
            &registry,
            vec![],
            "all".to_string(),
            None,
            std::env::temp_dir(),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .expect("build GPU vision mixer pipeline");
        manager.start().expect("start GPU vision mixer pipeline");
        let appsink = manager
            .pipeline()
            .by_name("pgmsink")
            .expect("appsink in pipeline")
            .downcast::<gstreamer_app::AppSink>()
            .expect("appsink type");
        Running {
            manager,
            appsink,
            main_loop,
            main_loop_thread,
            _registry_file: registry_file,
        }
    }

    fn element(&self, name: &str) -> gstreamer::Element {
        self.manager
            .pipeline()
            .by_name(name)
            .unwrap_or_else(|| panic!("{name} in pipeline"))
    }

    fn set_source_shape(&self, shape: (i32, i32)) {
        self.set_input_shape(0, shape);
    }

    fn set_input_shape(&self, input: usize, shape: (i32, i32)) {
        self.element(&format!("caps{input}"))
            .set_property("caps", source_caps(shape));
    }

    /// The dist mixer's position: the clock its keyframes and PGM frame
    /// times are in.
    fn position(&self, block_id: &str) -> gstreamer::ClockTime {
        self.element(&format!("{block_id}:mixer"))
            .query_position::<gstreamer::ClockTime>()
            .expect("mixer position")
    }

    /// Wait until the dist mixer's pad for `input` has stored caps `width`
    /// wide, and return the mixer's position then.
    fn wait_for_input_width(
        &self,
        block_id: &str,
        input: usize,
        width: i32,
    ) -> gstreamer::ClockTime {
        let pad = self
            .element(&format!("{block_id}:mixer"))
            .static_pad(&format!("sink_{input}"))
            .expect("mixer sink pad");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let stored = pad
                .current_caps()
                .and_then(|c| c.structure(0).and_then(|s| s.get::<i32>("width").ok()));
            if stored == Some(width) {
                return self.position(block_id);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "input {input} never reached the mixer {width} wide, caps width {stored:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn stop(mut self) {
        self.manager.stop().expect("stop");
        drop(self.manager);
        self.main_loop.quit();
        self.main_loop_thread.join().expect("main loop thread");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_switched_to_portrait_is_pillarboxed() {
    gstreamer::init().unwrap();
    if !gl_available_or_required() {
        return;
    }
    let running = Running::start(build_flow("vmresize", LANDSCAPE));

    // A 16:9 source fills the 16:9 canvas.
    let before = wait_for_pgm(&running.appsink, "the landscape source filling PGM", |f| {
        f > 0.95
    });
    eprintln!("landscape: white={:.2}", before);

    running.set_source_shape(PORTRAIT);
    let after = wait_for_pgm(
        &running.appsink,
        "the portrait source pillarboxed",
        pillarboxed,
    );
    eprintln!("portrait: white={:.2}", after);

    running.stop();
}

/// The first fit runs before the mixer's output has negotiated its size, so
/// it must use the configured PGM size, not a default canvas.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_portrait_from_the_start_is_pillarboxed() {
    gstreamer::init().unwrap();
    if !gl_available_or_required() {
        return;
    }
    let running = Running::start(build_flow("vmresize_first", PORTRAIT));
    let white = wait_for_pgm(
        &running.appsink,
        "the portrait source pillarboxed",
        pillarboxed,
    );
    eprintln!("portrait from the start: white={:.2}", white);
    running.stop();
}

/// The mixer pad stores new caps only after its event probes return. A
/// streaming thread held up there must not leave the old shape in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_switch_fits_when_caps_are_stored_late() {
    gstreamer::init().unwrap();
    if !gl_available_or_required() {
        return;
    }
    let block_id = "vmresize_late";
    let running = Running::start(build_flow(block_id, LANDSCAPE));
    wait_for_pgm(&running.appsink, "the landscape source filling PGM", |f| {
        f > 0.95
    });

    running
        .element(&format!("{block_id}:mixer"))
        .static_pad("sink_0")
        .expect("mixer sink_0")
        .add_probe(gstreamer::PadProbeType::EVENT_DOWNSTREAM, |_, info| {
            if let Some(gstreamer::PadProbeData::Event(ev)) = &info.data {
                if ev.type_() == gstreamer::EventType::Caps {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            gstreamer::PadProbeReturn::Ok
        });

    running.set_source_shape(PORTRAIT);
    let white = wait_for_pgm(
        &running.appsink,
        "the portrait source pillarboxed",
        pillarboxed,
    );
    eprintln!("portrait, caps stored late: white={:.2}", white);
    running.stop();
}

/// A source that changes shape during a fade must not cut the fade short,
/// and must be fitted in its new shape once the fade is over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_resized_during_a_fade_is_fitted_after_it() {
    gstreamer::init().unwrap();
    if !gl_available_or_required() {
        return;
    }
    let block_id = "vmresize_fade";
    let running = Running::start(build_flow(block_id, LANDSCAPE));
    wait_for_pgm(&running.appsink, "the landscape source filling PGM", |f| {
        f > 0.95
    });

    // Fade from white (input 0) to red (input 1) over 4 s. The red source
    // turns 4:3 about a second in, by the mixer's clock: a throttled mixer
    // catches up in bursts and runs ahead of the wall clock.
    let start = running.position(block_id);
    running
        .manager
        .trigger_transition(block_id, idx(0), idx(1), "fade", 4000)
        .expect("fade");
    running
        .manager
        .update_vision_mixer_after_take(block_id, Some(1), Some(0), 2)
        .expect("after take");
    while running.position(block_id) < start + gstreamer::ClockTime::from_mseconds(800) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    running.set_input_shape(1, (960, 720));
    let resized = running.wait_for_input_width(block_id, 1, 960);

    // A cut drops green (white to red) from over 200 to 0 between two
    // frames; the 4 s fade moves it a few levels a frame. Frames are
    // compared with each other, not placed on the wall clock, so the check
    // holds however far the mixer drifts from real time.
    let mut greens = Vec::new();
    while let Some(s) = running
        .appsink
        .try_pull_sample(gstreamer::ClockTime::from_seconds(5))
    {
        if s.buffer().and_then(|b| b.pts()).expect("PGM pts") <= resized {
            continue;
        }
        let green = mean_green(&s);
        greens.push(green);
        if green < 20.0 {
            break;
        }
    }
    let largest_step = greens
        .windows(2)
        .map(|w| w[0] - w[1])
        .fold(0.0f64, f64::max);
    assert!(
        greens.len() > 2 && greens[0] > 60.0 && largest_step < 40.0,
        "the fade was cut short after the resize, mean green {:?}",
        greens
    );

    // 4:3 red on 16:9: pillarbox bars cover a quarter of PGM.
    let bars = wait_for_pgm_by(
        &running.appsink,
        "the 4:3 source pillarboxed after the fade",
        black_fraction,
        |f| (0.20..0.30).contains(&f),
    );
    eprintln!("after the fade: black={:.2}", bars);
    running.stop();
}

/// A source that changes shape during Fade to Black comes back fitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_switched_during_ftb_is_pillarboxed_after_it() {
    gstreamer::init().unwrap();
    if !gl_available_or_required() {
        return;
    }
    let block_id = "vmresize_ftb";
    let running = Running::start(build_flow(block_id, LANDSCAPE));
    wait_for_pgm(&running.appsink, "the landscape source filling PGM", |f| {
        f > 0.95
    });

    running
        .manager
        .fade_to_black(block_id, 200)
        .expect("FTB on");
    wait_for_pgm_by(&running.appsink, "black", black_fraction, |f| f > 0.95);
    running.set_source_shape(PORTRAIT);
    running.wait_for_input_width(block_id, 0, PORTRAIT.0);
    let start = running.position(block_id);
    running
        .manager
        .fade_to_black(block_id, 1000)
        .expect("FTB off");

    // The source is grey while it fades back in, so count lit pixels: every
    // frame of the fade-in must already show it pillarboxed, not stretched.
    let fade_end = start + gstreamer::ClockTime::from_seconds(1);
    let mut fade_in = Vec::new();
    while let Some(s) = running
        .appsink
        .try_pull_sample(gstreamer::ClockTime::from_seconds(5))
    {
        if s.buffer().and_then(|b| b.pts()).expect("PGM pts") > fade_end {
            break;
        }
        let lit = 1.0 - black_fraction(&s);
        if lit > 0.05 {
            fade_in.push(lit);
        }
    }
    assert!(
        !fade_in.is_empty() && fade_in.iter().all(|lit| *lit < 0.45),
        "the source faded in stretched, lit fractions {:?}",
        fade_in
    );

    let white = wait_for_pgm(
        &running.appsink,
        "the portrait source pillarboxed",
        pillarboxed,
    );
    eprintln!("after FTB: white={:.2}", white);
    running.stop();
}

/// Run the mixer at about 40% of real time: each output frame takes 80 ms
/// instead of 33.
fn slow_down_mixer(running: &Running, block_id: &str) {
    running
        .element(&format!("{block_id}:mixer"))
        .static_pad("src")
        .expect("mixer src")
        .add_probe(gstreamer::PadProbeType::BUFFER, |_, _| {
            std::thread::sleep(std::time::Duration::from_millis(80));
            gstreamer::PadProbeReturn::Ok
        });
}

/// FTB keyframes are in the mixer's stream time. On a mixer slower than real
/// time, both fades must still play out in full.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ftb_plays_out_on_a_slow_mixer() {
    gstreamer::init().unwrap();
    if !gl_available_or_required() {
        return;
    }
    let block_id = "vmftb_slow";
    let running = Running::start(build_flow(block_id, LANDSCAPE));
    wait_for_pgm(&running.appsink, "the landscape source filling PGM", |f| {
        f > 0.95
    });
    slow_down_mixer(&running, block_id);

    running
        .manager
        .fade_to_black(block_id, 200)
        .expect("FTB on");
    wait_for_pgm_by(&running.appsink, "black", black_fraction, |f| f > 0.95);
    running
        .manager
        .fade_to_black(block_id, 1000)
        .expect("FTB off");
    wait_for_pgm(&running.appsink, "the source back at full level", |f| {
        f > 0.95
    });
    running.stop();
}

/// FTB off pressed while FTB on is still fading out brings PGM fully back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ftb_off_during_the_fade_out_restores_pgm() {
    gstreamer::init().unwrap();
    if !gl_available_or_required() {
        return;
    }
    let block_id = "vmftb_abort";
    let running = Running::start(build_flow(block_id, LANDSCAPE));
    wait_for_pgm(&running.appsink, "the landscape source filling PGM", |f| {
        f > 0.95
    });

    running
        .manager
        .fade_to_black(block_id, 500)
        .expect("FTB on");
    std::thread::sleep(std::time::Duration::from_millis(250));
    running
        .manager
        .fade_to_black(block_id, 500)
        .expect("FTB off");
    // FTB on's cleanup comes due in the middle of FTB off's fade. Stuck
    // part-way, PGM stays grey.
    wait_for_pgm(&running.appsink, "the source back at full level", |f| {
        f > 0.95
    });
    running.stop();
}

/// A cropped PiP source that changes shape during FTB comes back with its
/// crop re-derived from the new width: half of 720 px, not of 1280.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pip_crop_follows_a_source_resized_during_ftb() {
    use strom_types::vision_mixer::{PipTransforms, SourceCrop, Zone};
    gstreamer::init().unwrap();
    if !gl_available_or_required() {
        return;
    }
    let block_id = "vmftb_pipcrop";
    let mut flow = build_flow(block_id, LANDSCAPE);
    flow.blocks[0]
        .properties
        .insert("num_pips".to_string(), PV::String("1".into()));
    let running = Running::start(flow);
    wait_for_pgm(&running.appsink, "the landscape source filling PGM", |f| {
        f > 0.95
    });

    // PiP on PGM: red background (input 1), input 0 in one zone with its
    // right half cropped off.
    let mut transforms = PipTransforms::new();
    transforms.insert(
        0,
        SourceCrop {
            left: 0.0,
            top: 0.0,
            right: 0.5,
            bottom: 0.0,
        },
    );
    running
        .manager
        .apply_vision_mixer_pip_config(
            block_id,
            0,
            Some(1),
            vec![Zone {
                rect: None,
                capacity: None,
                sources: vec![0],
                border: None,
            }],
            transforms,
        )
        .expect("PiP config");
    running
        .manager
        .select_vision_mixer_pip_for_preview(block_id, 0)
        .expect("PiP to PVW");
    running
        .manager
        .trigger_transition(block_id, idx(0), idx(0), "cut", 0)
        .expect("take the PiP");
    let pad = running
        .element(&format!("{block_id}:mixer"))
        .static_pad("sink_0")
        .expect("mixer sink_0");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while pad.property::<i32>("crop-right") != 640 {
        assert!(
            std::time::Instant::now() < deadline,
            "PiP crop never applied, crop-right {}",
            pad.property::<i32>("crop-right")
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    running
        .manager
        .fade_to_black(block_id, 200)
        .expect("FTB on");
    wait_for_pgm_by(&running.appsink, "black", black_fraction, |f| f > 0.95);
    running.set_source_shape(PORTRAIT);
    running.wait_for_input_width(block_id, 0, PORTRAIT.0);
    running
        .manager
        .fade_to_black(block_id, 200)
        .expect("FTB off");

    assert_eq!(
        pad.property::<i32>("crop-right"),
        PORTRAIT.0 / 2,
        "the crop after FTB off is still sized for the old source"
    );
    running.stop();
}
