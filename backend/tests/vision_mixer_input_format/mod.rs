//! The Vision Mixer flows and measurements shared by the input format tests
//! (#674): a white source into `video_in_0`, PGM measured in RGBA.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

use crate::common;

pub const GPU_ELEMENTS: &[&str] = &[
    "glupload",
    "glcolorconvert",
    "glvideomixerelement",
    "gldownload",
];
pub const CPU_ELEMENTS: &[&str] = &["compositor", "videoconvert", "videocrop", "videotestsrc"];

pub const W: i32 = 320;
pub const H: i32 = 240;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Cpu,
    Gpu,
}

/// What sits between the white `videotestsrc` and `video_in_0`.
#[derive(Clone, Copy, Debug)]
pub enum Front {
    /// Nothing: the source settles on what the mixer input answers.
    Direct,
    /// A `videoconvert` with no caps after it (#674 case d).
    ViaVideoconvert,
    /// A capsfilter pinning a pixel format.
    Format(&'static str),
}

pub fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

pub fn format_caps(format: &str) -> String {
    format!("video/x-raw,format={format},width={W},height={H},framerate=30/1")
}

pub fn mixer_block(block_id: &str, backend: Backend) -> strom_types::BlockInstance {
    let mut p = HashMap::new();
    p.insert(
        "compositor_preference".to_string(),
        PV::String(match backend {
            Backend::Cpu => "cpu".into(),
            Backend::Gpu => "gpu".into(),
        }),
    );
    p.insert("num_inputs".to_string(), PV::UInt(2));
    p.insert("pgm_resolution".to_string(), PV::String(format!("{W}x{H}")));
    p.insert(
        "multiview_resolution".to_string(),
        PV::String(format!("{W}x{H}")),
    );
    if backend == Backend::Gpu {
        // Download PGM to system memory so the appsink can map pixels.
        p.insert("gl_download".to_string(), PV::Bool(true));
    }
    strom_types::BlockInstance {
        id: block_id.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: p,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    }
}

/// PGM measured in RGBA, whatever the mixer negotiated.
pub fn add_pgm_tap(flow: &mut Flow, block_id: &str) {
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
            ("max-buffers", PV::UInt(1)),
            ("drop", PV::Bool(true)),
        ],
    ));
    for (from, to) in [
        (format!("{block_id}:pgm_out"), "pgmconv:sink".to_string()),
        ("pgmconv:src".to_string(), "pgmcaps:sink".to_string()),
        ("pgmcaps:src".to_string(), "pgmsink:sink".to_string()),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }
}

pub fn link(flow: &mut Flow, from: &str, to: &str) {
    flow.links.push(strom_types::Link {
        from: from.to_string(),
        to: to.to_string(),
    });
}

/// A white `videotestsrc`, through `front`, into `video_in_0`.
pub fn testsrc_flow(block_id: &str, backend: Backend, front: Front) -> Flow {
    let mut flow = Flow::new(format!("vm_input_format_{block_id}"));
    flow.blocks.push(mixer_block(block_id, backend));
    flow.elements.push(elem(
        "src",
        "videotestsrc",
        vec![
            ("pattern", PV::String("white".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    let input = format!("{block_id}:video_in_0");
    match front {
        Front::Direct => link(&mut flow, "src:src", &input),
        Front::ViaVideoconvert => {
            flow.elements.push(elem("conv", "videoconvert", vec![]));
            link(&mut flow, "src:src", "conv:sink");
            link(&mut flow, "conv:src", &input);
        }
        Front::Format(format) => {
            flow.elements.push(elem(
                "fmt",
                "capsfilter",
                vec![("caps", PV::String(format_caps(format)))],
            ));
            link(&mut flow, "src:src", "fmt:sink");
            link(&mut flow, "fmt:src", &input);
        }
    }
    add_pgm_tap(&mut flow, block_id);
    flow
}

/// A running flow and the glib main loop the mixer's geometry refresh needs.
pub struct Running {
    pub manager: PipelineManager,
    pub appsink: gstreamer_app::AppSink,
    block_id: String,
    main_loop: gst::glib::MainLoop,
    main_loop_thread: std::thread::JoinHandle<()>,
    _registry_file: NamedTempFile,
}

impl Running {
    pub fn start(block_id: &str, flow: Flow) -> Running {
        Self::start_with(block_id, flow, |_| {})
    }

    /// [`Running::start`], with `configure` run on the built pipeline before
    /// it starts.
    pub fn start_with(
        block_id: &str,
        flow: Flow,
        configure: impl FnOnce(&gst::Pipeline),
    ) -> Running {
        gst::init().unwrap();
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        strom::gpu::detect_gpu_capabilities();
        let main_loop = gst::glib::MainLoop::new(None, false);
        let main_loop_thread = {
            let ml = main_loop.clone();
            std::thread::spawn(move || ml.run())
        };
        let registry_file = NamedTempFile::new().unwrap();
        let registry = BlockRegistry::new(registry_file.path());
        let mut manager = PipelineManager::new(
            &flow,
            EventBroadcaster::with_capacity(10),
            &registry,
            vec![],
            "all".to_string(),
            None,
            std::env::temp_dir(),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .expect("build vision mixer pipeline");
        configure(manager.pipeline());
        manager.start().expect("start vision mixer pipeline");
        let appsink = manager
            .pipeline()
            .by_name("pgmsink")
            .expect("appsink in pipeline")
            .downcast::<gstreamer_app::AppSink>()
            .expect("appsink type");
        Running {
            manager,
            appsink,
            block_id: block_id.to_string(),
            main_loop,
            main_loop_thread,
            _registry_file: registry_file,
        }
    }

    pub fn element(&self, name: &str) -> gst::Element {
        self.manager
            .pipeline()
            .by_name(name)
            .unwrap_or_else(|| panic!("{name} in pipeline"))
    }

    pub fn mixer_element(&self, name: &str) -> gst::Element {
        self.element(&format!("{}:{}", self.block_id, name))
    }

    /// The element feeding `name`'s sink pad.
    pub fn feeder_of(&self, element: &gst::Element) -> Option<gst::Element> {
        element.static_pad("sink")?.peer()?.parent_element()
    }

    /// The elements between input 0's front queue and its `glupload`.
    pub fn gpu_input_adapters(&self) -> Vec<String> {
        let queue = self.mixer_element("queue_0");
        let upload = self.mixer_element("glupload_0");
        let mut names = Vec::new();
        let mut next = self.feeder_of(&upload);
        while let Some(element) = next {
            if element == queue {
                return names;
            }
            names.insert(0, factory_name(&element));
            assert!(names.len() < 5, "input 0 has a loop: {names:?}");
            next = self.feeder_of(&element);
        }
        panic!("input 0's glupload is not fed from its queue: {names:?}");
    }

    /// Every `videoconvert` in the mixer that is not part of its own build,
    /// i.e. spliced in by an input front.
    pub fn spliced_converters(&self) -> Vec<String> {
        let prefix = format!("{}:glupload", self.block_id);
        self.manager
            .pipeline()
            .iterate_recurse()
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| factory_name(e) == "videoconvert" && e.name().starts_with(&prefix))
            .map(|e| e.name().to_string())
            .collect()
    }

    /// The format leaving input 0's front queue: what the source delivers now.
    pub fn input_format(&self) -> Option<String> {
        self.mixer_element("queue_0")
            .static_pad("src")?
            .current_caps()?
            .structure(0)?
            .get::<String>("format")
            .ok()
    }

    /// The format input 0's `glupload` receives.
    pub fn upload_format(&self) -> Option<String> {
        self.mixer_element("glupload_0")
            .static_pad("sink")?
            .current_caps()?
            .structure(0)?
            .get::<String>("format")
            .ok()
    }

    pub fn wait_for_input_format(&self, format: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.input_format().as_deref() != Some(format) {
            assert!(
                Instant::now() < deadline,
                "input 0 never delivered {format}, it delivers {:?}",
                self.input_format()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until glupload's sink has negotiated `format`. A splice changes
    /// the adapters before the new CAPS event reaches glupload, and PGM can
    /// stay white on frames from before a switch, so neither proves that
    /// glupload already sees the new frames.
    pub fn wait_for_upload_format(&self, format: &str, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.upload_format().as_deref() != Some(format) {
            assert!(
                Instant::now() < deadline,
                "{what}: glupload never received {format}, it receives {:?}",
                self.upload_format()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Pull PGM until `frames` consecutive white frames have arrived, or fail
    /// after 30 s.
    pub fn wait_for_white_pgm(&self, what: &str, frames: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut white_in_a_row = 0;
        let mut last = None;
        while Instant::now() < deadline {
            let Some(sample) = self
                .appsink
                .try_pull_sample(gst::ClockTime::from_mseconds(500))
            else {
                continue;
            };
            let white = white_fraction(&sample);
            last = Some(white);
            if white > 0.9 {
                white_in_a_row += 1;
                if white_in_a_row >= frames {
                    return;
                }
            } else {
                white_in_a_row = 0;
            }
        }
        panic!("PGM never showed {what} within 30 s, last white fraction {last:?}");
    }

    pub fn stop(mut self) {
        self.manager.stop().expect("stop");
        drop(self.manager);
        self.main_loop.quit();
        self.main_loop_thread.join().expect("main loop thread");
    }
}

pub fn factory_name(element: &gst::Element) -> String {
    element
        .factory()
        .map(|f| f.name().to_string())
        .unwrap_or_default()
}

/// Fraction of near-white pixels in an RGBA frame.
pub fn white_fraction(sample: &gst::Sample) -> f64 {
    let caps = sample.caps().expect("caps");
    let s = caps.structure(0).unwrap();
    assert_eq!(s.get::<&str>("format").unwrap(), "RGBA");
    let w = s.get::<i32>("width").unwrap() as usize;
    let h = s.get::<i32>("height").unwrap() as usize;
    let buffer = sample.buffer().expect("buffer");
    let map = buffer.map_readable().expect("map");
    let (mut hits, mut total) = (0u64, 0u64);
    for px in map.as_chunks::<4>().0.iter().take(w * h).step_by(7) {
        if px[0] > 200 && px[1] > 200 && px[2] > 200 {
            hits += 1;
        }
        total += 1;
    }
    hits as f64 / total.max(1) as f64
}

pub fn ready(backend: Backend) -> bool {
    match backend {
        Backend::Cpu => common::plugins_available(CPU_ELEMENTS),
        Backend::Gpu => common::gl_available(GPU_ELEMENTS),
    }
}

/// A white source through `front` reaches PGM. On the GPU backend, a
/// `videoconvert` is in front of `glupload` exactly when `converted`.
pub fn assert_input_reaches_pgm(block_id: &str, backend: Backend, front: Front, converted: bool) {
    if !ready(backend) {
        return;
    }
    let running = Running::start(block_id, testsrc_flow(block_id, backend, front));
    running.wait_for_white_pgm(&format!("the {front:?} source on {backend:?}"), 3);
    if backend == Backend::Gpu {
        let expected: Vec<String> = if converted {
            vec!["videoconvert".into()]
        } else {
            vec![]
        };
        assert_eq!(
            running.gpu_input_adapters(),
            expected,
            "{front:?}: the GPU input's adapters are not what the format needs"
        );
    }
    if let Front::Format(format) = front {
        assert_eq!(running.input_format().as_deref(), Some(format));
    }
    running.stop();
}
