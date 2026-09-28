//! Regression test: a source that changes shape mid-stream must be
//! aspect-fitted in its new shape on the GPU (OpenGL) vision mixer.
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
        (format!("{block_id}:pgm_out"), "pgmsink:sink".to_string()),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }
    flow
}

/// Fraction of PGM pixels that are white.
fn white_fraction(sample: &gstreamer::Sample) -> f64 {
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
    let mut white = 0u64;
    let mut total = 0u64;
    // Flat colour fields: every 4th pixel gives the same fraction, faster.
    for px in map.chunks_exact(4).take(w * h).step_by(4) {
        if px[0] > 200 && px[1] > 200 && px[2] > 200 {
            white += 1;
        }
        total += 1;
    }
    white as f64 / total.max(1) as f64
}

/// Pull PGM frames until one satisfies `done`, or fail after 30 s.
fn wait_for_pgm(appsink: &gstreamer_app::AppSink, what: &str, done: impl Fn(f64) -> bool) -> f64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut last = None;
    while std::time::Instant::now() < deadline {
        if let Some(s) = appsink.try_pull_sample(gstreamer::ClockTime::from_mseconds(500)) {
            let f = white_fraction(&s);
            if done(f) {
                return f;
            }
            last = Some(f);
        }
    }
    panic!(
        "PGM never showed {} within 30s, last white fraction {:?}",
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
        self.element("caps0")
            .set_property("caps", source_caps(shape));
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
