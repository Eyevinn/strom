//! Regression test: a CUDA-memory producer must reach the GPU Vision Mixer (#682).
//!
//! Every video input of the GPU Vision Mixer starts with `glupload`, which does
//! not take `memory:CUDAMemory`. The Media Player decodes in a pipeline of its
//! own and re-pushes whatever its decoder produced from an `appsrc`; behind
//! `nvh264dec` that is CUDA memory and nothing else, so the clip never reached
//! the mixer. The input front now splices `cudadownload` — the CUDA-to-GL interop
//! element — in front of `glupload` when the producer offers CUDA memory only.
//!
//! These tests drive the real Media Player block into the real Vision Mixer
//! block. Only the Media Player's elements are started: the mixer stays in NULL,
//! so no GL context is needed — the queries that decide the splice happen while
//! the Media Player's `identity` settles its caps, before any CAPS event or
//! buffer reaches the mixer. The GL plugin must be installed (CI installs
//! `gstreamer1.0-gl`); `cudadownload` is not, so a stand-in registered under
//! that name takes its place. Real CUDA memory needs an NVIDIA host.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::gl_input_front::CUDA_ADAPTER_FACTORY;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

/// The GL elements the GPU Vision Mixer is built from. Their absence is a CI
/// regression, not a reason to skip: a skip would pass green guarding nothing.
const GL_ELEMENTS: &[&str] = &["glupload", "glcolorconvert", "glvideomixerelement"];

const MIXER: &str = "vm";
const PLAYER: &str = "player";

/// Both tests change what the registry offers under [`CUDA_ADAPTER_FACTORY`],
/// and cargo runs tests in one process in parallel.
static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

const CUDA_CAPS: &str = "video/x-raw(memory:CUDAMemory),format=NV12,width=320,height=240,\
     framerate=25/1,pixel-aspect-ratio=1/1,interlace-mode=progressive";
const SYSTEM_CAPS: &str = "video/x-raw,format=NV12,width=320,height=240,\
     framerate=25/1,pixel-aspect-ratio=1/1,interlace-mode=progressive";

/// A stand-in for `cudadownload` with the same pad templates: CUDA or system
/// memory in, GL or system memory out. The tests only need the element to
/// exist under that name and to accept CUDA caps on its sink pad.
mod stand_in {
    use gstreamer as gst;
    use gstreamer::glib;
    use gstreamer::prelude::*;
    use gstreamer::subclass::prelude::*;

    mod imp {
        use super::*;
        use std::sync::LazyLock;

        #[derive(Default)]
        pub struct CudaDownloadStandIn;

        #[glib::object_subclass]
        impl ObjectSubclass for CudaDownloadStandIn {
            const NAME: &'static str = "StromTestCudaDownloadStandIn";
            type Type = super::CudaDownloadStandIn;
            type ParentType = gst::Element;
        }

        impl ObjectImpl for CudaDownloadStandIn {
            fn constructed(&self) {
                self.parent_constructed();
                let obj = self.obj();
                for name in ["sink", "src"] {
                    let templ = obj.element_class().pad_template(name).unwrap();
                    obj.add_pad(&gst::Pad::from_template(&templ)).unwrap();
                }
            }
        }

        impl GstObjectImpl for CudaDownloadStandIn {}

        impl ElementImpl for CudaDownloadStandIn {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "cudadownload stand-in",
                        "Filter/Video",
                        "Test stand-in for the nvcodec cudadownload element",
                        "Strom tests",
                    )
                });
                Some(&META)
            }

            fn pad_templates() -> &'static [gst::PadTemplate] {
                static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                    let sink: gst::Caps = "video/x-raw(memory:CUDAMemory); video/x-raw"
                        .parse()
                        .unwrap();
                    let src: gst::Caps =
                        "video/x-raw(memory:GLMemory); video/x-raw".parse().unwrap();
                    vec![
                        gst::PadTemplate::new(
                            "sink",
                            gst::PadDirection::Sink,
                            gst::PadPresence::Always,
                            &sink,
                        )
                        .unwrap(),
                        gst::PadTemplate::new(
                            "src",
                            gst::PadDirection::Src,
                            gst::PadPresence::Always,
                            &src,
                        )
                        .unwrap(),
                    ]
                });
                TEMPLATES.as_ref()
            }
        }
    }

    glib::wrapper! {
        pub struct CudaDownloadStandIn(ObjectSubclass<imp::CudaDownloadStandIn>)
            @extends gst::Element, gst::Object;
    }

    /// Make [`super::CUDA_ADAPTER_FACTORY`] available for as long as the guard
    /// lives. On an NVIDIA host the real element is left in place and used.
    pub struct Registered(Option<gst::PluginFeature>);

    pub fn register() -> Registered {
        if gst::ElementFactory::find(super::CUDA_ADAPTER_FACTORY).is_some() {
            return Registered(None);
        }
        gst::Element::register(
            None,
            super::CUDA_ADAPTER_FACTORY,
            gst::Rank::NONE,
            CudaDownloadStandIn::static_type(),
        )
        .expect("register the cudadownload stand-in");
        Registered(
            gst::ElementFactory::find(super::CUDA_ADAPTER_FACTORY)
                .map(|f| f.upcast::<gst::PluginFeature>()),
        )
    }

    impl Drop for Registered {
        fn drop(&mut self) {
            if let Some(feature) = self.0.take() {
                gst::Registry::get().remove_feature(&feature);
            }
        }
    }
}

fn block(id: &str, definition: &str, properties: Vec<(&str, PV)>) -> strom_types::BlockInstance {
    let properties: HashMap<String, PV> = properties
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let computed_external_pads = strom::blocks::builtin::get_builder(definition)
        .and_then(|builder| builder.get_external_pads(&properties));
    strom_types::BlockInstance {
        id: id.to_string(),
        block_definition_id: definition.to_string(),
        name: None,
        properties,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads,
    }
}

/// A Media Player (decode mode, empty playlist) feeding `mixer_input` of a GPU
/// Vision Mixer.
fn player_into_mixer(name: &str, mixer_input: &str) -> Flow {
    let mut flow = Flow::new(name.to_string());
    flow.blocks.push(block(
        MIXER,
        "builtin.vision_mixer",
        vec![
            ("compositor_preference", PV::String("gpu".to_string())),
            ("num_inputs", PV::UInt(2)),
            ("num_dsk_inputs", PV::String("1".to_string())),
            ("pgm_resolution", PV::String("320x240".to_string())),
            ("multiview_resolution", PV::String("320x240".to_string())),
        ],
    ));
    flow.blocks.push(block(
        PLAYER,
        "builtin.media_player",
        vec![("decode", PV::Bool(true))],
    ));
    flow.links.push(strom_types::Link {
        from: format!("{}:video_out", PLAYER),
        to: format!("{}:{}", MIXER, mixer_input),
    });
    flow
}

fn build_manager(flow: &Flow) -> (PipelineManager, NamedTempFile) {
    gst::init().unwrap();
    strom::gpu::detect_gpu_capabilities();
    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let manager = PipelineManager::new(
        flow,
        EventBroadcaster::with_capacity(10),
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
    .expect("GPU vision mixer flow should build");
    (manager, temp_file)
}

fn element(pipeline: &gst::Pipeline, name: &str) -> gst::Element {
    pipeline
        .by_name(name)
        .unwrap_or_else(|| panic!("{} exists", name))
}

/// The element feeding `name`'s sink pad.
fn feeder_of(pipeline: &gst::Pipeline, name: &str) -> Option<gst::Element> {
    element(pipeline, name)
        .static_pad("sink")?
        .peer()?
        .parent_element()
}

fn factory_name(element: &gst::Element) -> String {
    element
        .factory()
        .map(|f| f.name().to_string())
        .unwrap_or_default()
}

/// What the Media Player's output did with one frame.
struct Outcome {
    /// The caps its `video_out` settled on, if it negotiated with the mixer.
    negotiated: Option<gst::Caps>,
    /// Every error on the bus, as (source name, message, debug).
    errors: Vec<(String, String, String)>,
}

/// Start only the Media Player's main-pipeline elements and push one frame
/// with `caps` from its `appsrc`, the way its bridge does with each decoded
/// sample. The mixer stays in NULL. Waits until the Media Player's output has
/// negotiated or the bus carries an error, at most 5 s.
fn push_from_player(pipeline: &gst::Pipeline, caps: &str) -> Outcome {
    let names = ["appsrc_video", "queue_video", "video_out"].map(|n| format!("{}:{}", PLAYER, n));
    for name in names.iter().rev() {
        let e = element(pipeline, name);
        e.set_locked_state(true);
        e.set_state(gst::State::Playing)
            .unwrap_or_else(|e| panic!("{} could not start: {:?}", name, e));
    }

    let caps: gst::Caps = caps.parse().unwrap();
    let mut buffer = gst::Buffer::with_size(320 * 240 * 3 / 2).unwrap();
    buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    let sample = gst::Sample::builder().buffer(&buffer).caps(&caps).build();
    let appsrc = element(pipeline, &names[0])
        .downcast::<gstreamer_app::AppSrc>()
        .unwrap();
    appsrc
        .push_sample(&sample)
        .expect("appsrc takes the sample");

    let video_out_src = element(pipeline, &names[2]).static_pad("src").unwrap();
    let bus = pipeline.bus().unwrap();
    let mut errors = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut settle_until = None;
    loop {
        while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(err) = msg.view() {
                errors.push((
                    msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                    err.error().to_string(),
                    err.debug().map(|d| d.to_string()).unwrap_or_default(),
                ));
            }
        }
        if video_out_src.current_caps().is_some() && settle_until.is_none() {
            break;
        }
        // After the first error give the others (the Media Player's own
        // not-negotiated) a moment to land too.
        if !errors.is_empty() && settle_until.is_none() {
            settle_until = Some(Instant::now() + Duration::from_millis(200));
        }
        let now = Instant::now();
        if now >= deadline || settle_until.is_some_and(|t| now >= t) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let negotiated = video_out_src.current_caps();

    for name in &names {
        let e = element(pipeline, name);
        let _ = e.set_state(gst::State::Null);
        e.set_locked_state(false);
    }
    Outcome { negotiated, errors }
}

/// The defect: CUDA memory from the Media Player into a mixer input. With the
/// front, `cudadownload` sits between the input queue and `glupload`, and the
/// Media Player's output negotiated; without it, nothing was inserted and the
/// Media Player failed with not-negotiated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cuda_memory_from_the_media_player_gets_cudadownload_before_glupload() {
    let _lock = REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    common::require_elements(GL_ELEMENTS);
    let _adapter = stand_in::register();

    for input in ["video_in_0", "dsk_in_0"] {
        let suffix = if input == "dsk_in_0" { "dsk_0" } else { "0" };
        let queue = format!("{}:queue_{}", MIXER, suffix);
        let glupload = format!("{}:glupload_{}", MIXER, suffix);

        let (manager, _registry) = build_manager(&player_into_mixer("cuda_into_vm", input));
        let pipeline = manager.pipeline();
        assert_eq!(
            feeder_of(pipeline, &glupload).map(|e| e.name().to_string()),
            Some(queue.clone()),
            "{}: before any caps, {} should feed {} directly",
            input,
            queue,
            glupload
        );

        let outcome = push_from_player(pipeline, CUDA_CAPS);

        let adapter = feeder_of(pipeline, &glupload).unwrap_or_else(|| {
            panic!("{}: {} lost its feeder", input, glupload);
        });
        assert_eq!(
            factory_name(&adapter),
            CUDA_ADAPTER_FACTORY,
            "{}: CUDA memory from the Media Player reached {} without a {} in \
             front of it, so it cannot negotiate",
            input,
            glupload,
            CUDA_ADAPTER_FACTORY
        );
        assert_eq!(
            feeder_of(pipeline, &adapter.name()).map(|e| e.name().to_string()),
            Some(queue.clone()),
            "{}: the inserted {} is not fed by the input queue",
            input,
            CUDA_ADAPTER_FACTORY
        );

        assert!(
            outcome.errors.is_empty(),
            "{}: the flow failed: {:?}",
            input,
            outcome.errors
        );
        assert!(
            outcome
                .negotiated
                .as_ref()
                .and_then(|c| c.features(0))
                .is_some_and(|f| f.contains("memory:CUDAMemory")),
            "{}: the Media Player's output did not negotiate CUDA memory into the mixer \
             (got {:?})",
            input,
            outcome.negotiated
        );
    }
}

/// The front costs nothing where it is not needed: system memory goes straight
/// into `glupload`, which uploads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_memory_from_the_media_player_goes_straight_into_glupload() {
    let _lock = REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    common::require_elements(GL_ELEMENTS);
    let _adapter = stand_in::register();

    let (manager, _registry) = build_manager(&player_into_mixer("system_into_vm", "video_in_0"));
    let pipeline = manager.pipeline();
    let outcome = push_from_player(pipeline, SYSTEM_CAPS);

    assert!(
        outcome.negotiated.is_some() && outcome.errors.is_empty(),
        "system memory did not negotiate into the mixer: {:?}",
        outcome.errors
    );
    assert_eq!(
        feeder_of(pipeline, &format!("{}:glupload_0", MIXER)).map(|e| e.name().to_string()),
        Some(format!("{}:queue_0", MIXER)),
        "a system-memory input was given an adapter it does not need"
    );
}

/// Without `cudadownload` (no nvcodec plugin), CUDA memory must fail the flow
/// with a message that names what is missing, posted by the mixer input —
/// not only the Media Player's `Internal data stream error`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cuda_memory_without_cudadownload_fails_the_flow_with_a_reason() {
    let _lock = REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    common::require_elements(GL_ELEMENTS);
    if gst::ElementFactory::find(CUDA_ADAPTER_FACTORY).is_some() {
        // Only an NVIDIA build has the real element; CI never does.
        eprintln!(
            "SKIP: {} is installed, so the missing-adapter path cannot be reached here",
            CUDA_ADAPTER_FACTORY
        );
        return;
    }

    let (manager, _registry) = build_manager(&player_into_mixer("cuda_no_adapter", "video_in_0"));
    let pipeline = manager.pipeline();
    let queue = format!("{}:queue_0", MIXER);
    let errors = push_from_player(pipeline, CUDA_CAPS).errors;
    let ours = errors.iter().find(|(source, _, _)| *source == queue);
    let Some((_, message, debug)) = ours else {
        panic!(
            "no error from {}: CUDA memory without {} fails the flow without saying \
             why. Errors on the bus: {:?}",
            queue, CUDA_ADAPTER_FACTORY, errors
        );
    };
    assert!(
        message.contains(CUDA_ADAPTER_FACTORY)
            && message.contains("CUDA memory")
            && message.contains("video_in_0"),
        "the error does not say which input lacks what: {}",
        message
    );
    assert!(
        debug.contains("nvcodec"),
        "the error does not say how to fix it: {}",
        debug
    );
}
