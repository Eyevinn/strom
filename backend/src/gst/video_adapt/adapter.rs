//! The adapters [`super::decide`] calls for, and what they are made of: the
//! element factories, their templates, and the elements built from them.

use gstreamer as gst;

/// The caps feature that marks a buffer as living in GL memory.
pub const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

/// The caps feature that marks a buffer as living in CUDA memory.
pub const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";

/// The element that converts raw video in system memory between pixel formats.
pub const VIDEO_CONVERT_FACTORY: &str = "videoconvert";

/// The element that downloads GL memory to system memory.
pub const GL_DOWNLOAD_FACTORY: &str = "gldownload";

/// The element that takes CUDA memory out to GL memory (CUDA-GL interop) or
/// to system memory, whichever its consumer takes. Ships with the GStreamer
/// nvcodec plugin, so it exists on NVIDIA builds only.
pub const CUDA_DOWNLOAD_FACTORY: &str = "cudadownload";

/// The caps feature that marks a buffer as living in Direct3D 12 memory.
pub const D3D12_MEMORY_FEATURE: &str = "memory:D3D12Memory";

/// The caps feature that marks a buffer as living in Direct3D 11 memory.
pub const D3D11_MEMORY_FEATURE: &str = "memory:D3D11Memory";

/// The element that downloads D3D12 memory to system memory. Ships with the
/// GStreamer d3d12 plugin, so it exists on Windows builds only.
pub const D3D12_DOWNLOAD_FACTORY: &str = "d3d12download";

/// The element that downloads D3D11 memory to system memory. Ships with the
/// GStreamer d3d11 plugin, so it exists on Windows builds only.
pub const D3D11_DOWNLOAD_FACTORY: &str = "d3d11download";

/// An element to put in front of the consumer, in link order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    /// `gldownload`: GL memory to system memory.
    GlDownload,
    /// `cudadownload`: CUDA memory to GL or system memory.
    CudaDownload,
    /// `d3d12download`: D3D12 memory to system memory.
    D3d12Download,
    /// `d3d11download`: D3D11 memory to system memory.
    D3d11Download,
    /// `videoconvert`: a system-memory pixel format the consumer cannot take
    /// to one it can.
    VideoConvert,
    /// `videoscale`: scales raw video in system memory.
    VideoScale,
    /// `glcolorconvert ! glcolorscale ! glcolorconvert`: scales and converts
    /// in GL memory. `glcolorscale` takes RGBA only, so GL converts to it and
    /// back.
    GlScale,
    /// `cudaconvertscale`: scales and converts in CUDA memory.
    CudaScale,
    /// `d3d12convert`: scales and converts in D3D12 memory.
    D3d12Scale,
    /// `d3d11convert`: scales and converts in D3D11 memory.
    D3d11Scale,
    /// A `capsfilter` that pins system memory after a download:
    /// [`system_memory_pin`]. `gldownload` and `cudadownload` also output GPU
    /// memory, which would otherwise pass through to a consumer that lists
    /// it.
    SystemMemoryPin,
}

impl Adapter {
    /// The element factory the adapter is named after: the one it is made
    /// from, or for [`Adapter::GlScale`] the scaler among its three.
    pub fn factory(self) -> &'static str {
        match self {
            Adapter::VideoScale => "videoscale",
            Adapter::GlScale => "glcolorscale",
            Adapter::CudaScale => "cudaconvertscale",
            Adapter::D3d12Scale => "d3d12convert",
            Adapter::D3d11Scale => "d3d11convert",
            Adapter::SystemMemoryPin => "capsfilter",
            Adapter::GlDownload => GL_DOWNLOAD_FACTORY,
            Adapter::CudaDownload => CUDA_DOWNLOAD_FACTORY,
            Adapter::D3d12Download => D3D12_DOWNLOAD_FACTORY,
            Adapter::D3d11Download => D3D11_DOWNLOAD_FACTORY,
            Adapter::VideoConvert => VIDEO_CONVERT_FACTORY,
        }
    }

    /// The element factories the adapter is made from, in link order.
    pub fn factories(self) -> &'static [&'static str] {
        match self {
            Adapter::GlScale => &["glcolorconvert", "glcolorscale", "glcolorconvert"],
            Adapter::GlDownload => &[GL_DOWNLOAD_FACTORY],
            Adapter::CudaDownload => &[CUDA_DOWNLOAD_FACTORY],
            Adapter::D3d12Download => &[D3D12_DOWNLOAD_FACTORY],
            Adapter::D3d11Download => &[D3D11_DOWNLOAD_FACTORY],
            Adapter::VideoConvert => &[VIDEO_CONVERT_FACTORY],
            Adapter::VideoScale => &["videoscale"],
            Adapter::CudaScale => &["cudaconvertscale"],
            Adapter::D3d12Scale => &["d3d12convert"],
            Adapter::D3d11Scale => &["d3d11convert"],
            Adapter::SystemMemoryPin => &["capsfilter"],
        }
    }

    /// The downloads, each out of one GPU memory type.
    pub const DOWNLOADS: [Adapter; 4] = [
        Adapter::GlDownload,
        Adapter::CudaDownload,
        Adapter::D3d12Download,
        Adapter::D3d11Download,
    ];

    /// The download made from `factory`, if it is one.
    pub fn download_from(factory: &str) -> Option<Adapter> {
        Adapter::DOWNLOADS
            .into_iter()
            .find(|adapter| adapter.factory() == factory)
    }

    /// The caps feature of the memory a download takes. `None` for every
    /// adapter that is not a download.
    pub fn memory_feature(self) -> Option<&'static str> {
        match self {
            Adapter::GlDownload => Some(GL_MEMORY_FEATURE),
            Adapter::CudaDownload => Some(CUDA_MEMORY_FEATURE),
            Adapter::D3d12Download => Some(D3D12_MEMORY_FEATURE),
            Adapter::D3d11Download => Some(D3D11_MEMORY_FEATURE),
            Adapter::VideoConvert
            | Adapter::VideoScale
            | Adapter::GlScale
            | Adapter::CudaScale
            | Adapter::D3d12Scale
            | Adapter::D3d11Scale
            | Adapter::SystemMemoryPin => None,
        }
    }

    /// The GStreamer plugin the adapter's element ships with, for an error
    /// that says what to install.
    pub fn plugin(self) -> &'static str {
        match self {
            Adapter::GlDownload | Adapter::GlScale => "opengl",
            Adapter::CudaDownload | Adapter::CudaScale => "nvcodec",
            Adapter::D3d12Download | Adapter::D3d12Scale => "d3d12",
            Adapter::D3d11Download | Adapter::D3d11Scale => "d3d11",
            Adapter::VideoConvert | Adapter::VideoScale => "videoconvertscale",
            Adapter::SystemMemoryPin => "coreelements",
        }
    }

    /// Every adapter. `sink_template` caches one template per entry.
    pub const ALL: [Adapter; 11] = [
        Adapter::GlDownload,
        Adapter::CudaDownload,
        Adapter::D3d12Download,
        Adapter::D3d11Download,
        Adapter::VideoConvert,
        Adapter::VideoScale,
        Adapter::GlScale,
        Adapter::CudaScale,
        Adapter::D3d12Scale,
        Adapter::D3d11Scale,
        Adapter::SystemMemoryPin,
    ];

    /// What the adapter's first element takes on its sink pad: its sink
    /// template, and for a GPU scaler only its entries in the scaler's own
    /// memory. `None` when an element is not installed. Read once per
    /// adapter, and only once found, so a test element registered later is
    /// still seen.
    pub fn sink_template(self) -> Option<&'static gst::Caps> {
        static TEMPLATES: [std::sync::OnceLock<gst::Caps>; Adapter::ALL.len()] =
            [const { std::sync::OnceLock::new() }; Adapter::ALL.len()];
        // An adapter missing from `ALL` is a bug the test below catches; it
        // gets no slot rather than a panic in a pad probe.
        let slot = &TEMPLATES[Adapter::ALL.iter().position(|a| *a == self)?];
        if let Some(caps) = slot.get() {
            return Some(caps);
        }
        let caps = match Scaler::from_adapter(self) {
            Some(scaler) => scaler.read_template(gst::PadDirection::Sink)?,
            None => factory_template(self.factories()[0], gst::PadDirection::Sink)?,
        };
        Some(slot.get_or_init(|| caps))
    }

    /// True when the adapter's element takes `caps` on its sink pad. `false`
    /// when it is not installed.
    pub fn sink_takes(self, caps: &gst::CapsRef) -> bool {
        self.sink_template()
            .is_some_and(|template| caps.can_intersect(template))
    }

    /// True for the downloads out of a platform GPU memory (CUDA, D3D12,
    /// D3D11), whose elements a working install can lack.
    pub fn downloads_device_memory(self) -> bool {
        matches!(
            self,
            Adapter::CudaDownload | Adapter::D3d12Download | Adapter::D3d11Download
        )
    }
}

/// A GPU scaler and converter, working in one memory type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaler {
    Gl,
    Cuda,
    D3d12,
    D3d11,
}

impl Scaler {
    pub const ALL: [Scaler; 4] = [Scaler::Gl, Scaler::Cuda, Scaler::D3d12, Scaler::D3d11];

    /// The adapter that scales and converts in its memory.
    pub fn adapter(self) -> Adapter {
        match self {
            Scaler::Gl => Adapter::GlScale,
            Scaler::Cuda => Adapter::CudaScale,
            Scaler::D3d12 => Adapter::D3d12Scale,
            Scaler::D3d11 => Adapter::D3d11Scale,
        }
    }

    /// The scaler an adapter is, if it is one.
    pub fn from_adapter(adapter: Adapter) -> Option<Scaler> {
        Scaler::ALL.into_iter().find(|s| s.adapter() == adapter)
    }

    /// The caps feature of the memory it works in.
    pub fn memory_feature(self) -> &'static str {
        self.download()
            .memory_feature()
            .expect("every scaler's memory has a download")
    }

    /// The download out of its memory.
    pub fn download(self) -> Adapter {
        match self {
            Scaler::Gl => Adapter::GlDownload,
            Scaler::Cuda => Adapter::CudaDownload,
            Scaler::D3d12 => Adapter::D3d12Download,
            Scaler::D3d11 => Adapter::D3d11Download,
        }
    }

    /// The scaler for the memory `features` name, if there is one.
    pub fn for_features(features: &gst::CapsFeaturesRef) -> Option<Scaler> {
        Scaler::ALL
            .into_iter()
            .find(|s| features.contains(s.memory_feature()))
    }

    /// The pad template of its first (sink) or last (src) element, in its
    /// memory only. `None` when an element is not installed.
    pub fn template(self, direction: gst::PadDirection) -> Option<gst::Caps> {
        if direction == gst::PadDirection::Sink {
            return self.adapter().sink_template().cloned();
        }
        // Asked per decision on the GPU path: read once, and only once found,
        // so a test element registered later is still seen.
        static SRC: [std::sync::OnceLock<gst::Caps>; Scaler::ALL.len()] =
            [const { std::sync::OnceLock::new() }; Scaler::ALL.len()];
        let slot = &SRC[Scaler::ALL.iter().position(|s| *s == self)?];
        if let Some(caps) = slot.get() {
            return Some(caps.clone());
        }
        let caps = self.read_template(direction)?;
        Some(slot.get_or_init(|| caps).clone())
    }

    /// [`Scaler::template`], read from the registry.
    fn read_template(self, direction: gst::PadDirection) -> Option<gst::Caps> {
        let factories = self.adapter().factories();
        if !factories.iter().all(|f| factory_available(f)) {
            return None;
        }
        let factory = match direction {
            gst::PadDirection::Sink => factories[0],
            _ => factories[factories.len() - 1],
        };
        let template = factory_template(factory, direction)?;
        Some(in_memory(&template, Some(self.memory_feature())))
    }
}

/// Scaling and converting on the CPU: what [`super::Consumer::ScaleConvert`] calls
/// for in system memory, after a download first, and what a block that
/// scales starts with.
pub const CPU_SCALE: [Adapter; 2] = [Adapter::VideoScale, Adapter::VideoConvert];

/// True when `features` put the buffer in system memory: no memory feature, or
/// the system-memory one, with or without metas next to it.
pub(crate) fn is_system_memory(features: &gst::CapsFeaturesRef) -> bool {
    features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)
        || !features.iter().any(|f| f.starts_with("memory:"))
}

/// A capsfilter named `<name_prefix>_system_memory` that pins what a download
/// outputs to system memory: `gldownload` and `cudadownload` also output GL
/// memory, which a consumer that lists GL memory would otherwise be handed.
pub fn system_memory_pin(name_prefix: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make("capsfilter")
        .name(format!("{}_system_memory", name_prefix))
        .property("caps", gst::Caps::new_empty_simple("video/x-raw"))
        .build()
        .map_err(|e| format!("capsfilter could not be created: {}", e))
}

/// True when `name` can be created from the registry.
pub fn factory_available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Create the elements for `adapters`, in link order, named after
/// `name_prefix`.
pub fn build_elements(
    adapters: &[Adapter],
    name_prefix: &str,
) -> Result<Vec<gst::Element>, String> {
    let mut elements: Vec<gst::Element> = Vec::new();
    let mut made: Vec<&str> = Vec::new();
    for adapter in adapters {
        if *adapter == Adapter::SystemMemoryPin {
            elements.push(system_memory_pin(name_prefix)?);
            continue;
        }
        for factory in adapter.factories() {
            // A factory that repeats in the chain (GL scaling converts on
            // both sides) gets a count, so names stay unique.
            let earlier = made.iter().filter(|f| *f == factory).count();
            made.push(factory);
            elements.push(match earlier {
                0 => make(factory, name_prefix)?,
                n => gst::ElementFactory::make(factory)
                    .name(format!("{}_{}{}", name_prefix, factory, n))
                    .build()
                    .map_err(|e| format!("{} could not be created: {}", factory, e))?,
            });
        }
    }
    Ok(elements)
}

/// The raw video structures of `caps` in the memory `feature` names, or in
/// system memory for `None`.
pub fn in_memory(caps: &gst::CapsRef, feature: Option<&str>) -> gst::Caps {
    let mut picked = gst::Caps::new_empty();
    {
        let picked = picked.get_mut().expect("new caps are writable");
        for (structure, features) in caps.iter_with_features() {
            let matches = if features.is_any() {
                false
            } else {
                match feature {
                    Some(feature) => features.contains(feature),
                    None => is_system_memory(features),
                }
            };
            if structure.name() == "video/x-raw" && matches {
                picked.append_structure_full(structure.to_owned(), Some(features.to_owned()));
            }
        }
    }
    picked
}

/// The pad template of `factory` in `direction`. `None` when it is not
/// installed.
pub fn factory_template(factory: &str, direction: gst::PadDirection) -> Option<gst::Caps> {
    gst::ElementFactory::find(factory)?
        .static_pad_templates()
        .into_iter()
        .find(|t| t.direction() == direction)
        .map(|t| t.caps())
}

/// An element of `factory` named `<name_prefix>_<factory>`.
fn make(factory: &str, name_prefix: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .name(format!("{}_{}", name_prefix, factory))
        .build()
        .map_err(|e| format!("{} could not be created: {}", factory, e))
}

/// What `glupload` takes in system memory: the system-memory raw video
/// entries of its sink template, with their features dropped so they match
/// producer caps written either way. `None` when `glupload` is not installed.
/// Read once: the registry does not change under a running process.
pub fn gl_upload_system_caps() -> Option<&'static gst::Caps> {
    static CAPS: std::sync::OnceLock<Option<gst::Caps>> = std::sync::OnceLock::new();
    CAPS.get_or_init(|| {
        let factory = gst::ElementFactory::find("glupload")?;
        let template = factory
            .static_pad_templates()
            .into_iter()
            .find(|t| t.direction() == gst::PadDirection::Sink)?
            .caps();
        let mut system = gst::Caps::new_empty();
        {
            let system = system.get_mut().expect("new caps are writable");
            for (structure, features) in template.iter_with_features() {
                if structure.name() == "video/x-raw" && is_system_memory(features) {
                    system.append_structure(structure.to_owned());
                }
            }
        }
        (!system.is_empty()).then_some(system)
    })
    .as_ref()
}
