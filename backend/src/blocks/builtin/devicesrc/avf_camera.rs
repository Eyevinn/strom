//! Open an AVFoundation camera by its stable id, not by its place in a list
//! (#1278).
//!
//! Before GStreamer 1.30, `avfdeviceprovider` binds a camera through
//! `avfvideosrc`'s `device-index`: the camera's position in macOS' device list
//! when the provider probed. That provider never probes again while Strom runs,
//! and `avfvideosrc` reads the list again when it opens the camera. macOS does
//! not keep that list in one order, so a block bound to one camera could open
//! another (docs/UPSTREAM_NOTES.md). The camera's stable id, `avf.unique_id`,
//! is in the device properties. Here Strom probes the list again when the
//! block is built, looks the camera up by that id and sets the index it has
//! now. From 1.30 `avfvideosrc` has a `unique-id` property, which the provider
//! sets itself, and none of this is needed.
//!
//! The list can still reorder between the build and the open, so the block
//! also checks which camera opened, and fails the flow if it is not the bound
//! one.

use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::glib::translate::{FromGlibPtrContainer, ToGlibPtr};
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, info, warn};

/// Device property that holds AVFoundation's stable camera id.
const UNIQUE_ID_KEY: &str = "avf.unique_id";

/// A camera as macOS lists it right now.
#[derive(Debug, Clone, PartialEq)]
struct Camera {
    unique_id: String,
    index: i32,
    name: String,
}

/// The AVFoundation camera's stable id, if `device` is one.
pub(super) fn unique_id(device: &gst::Device) -> Option<String> {
    device
        .properties()?
        .get::<String>(UNIQUE_ID_KEY)
        .ok()
        .filter(|id| !id.is_empty())
}

/// Point `element`, made from `device`, at the camera `device` describes, as
/// macOS lists it now; then check, once the camera is open, that it is that
/// camera.
pub(super) fn bind(
    block: &str,
    device: &gst::Device,
    unique_id: &str,
    element: &gst::Element,
) -> Result<(), BlockBuildError> {
    let name = device.display_name().to_string();
    if element.has_property("unique-id") {
        // GStreamer 1.30+: the provider set the stable id, and avfvideosrc
        // opens by it.
        debug!("{}: camera '{}' opens by unique-id", block, name);
    } else if element.has_property_with_type("device-index", i32::static_type()) {
        let cameras = probe_cameras().map_err(|why| {
            BlockBuildError::ElementCreation(format!(
                "{}: cannot list cameras again to find '{}': {}",
                block, name, why
            ))
        })?;
        set_current_index(block, &name, unique_id, element, &cameras)?;
    }
    install_open_check(block, element, &name, unique_id);
    Ok(())
}

/// Set `element`'s `device-index` to where `cameras` lists `unique_id` now.
fn set_current_index(
    block: &str,
    name: &str,
    unique_id: &str,
    element: &gst::Element,
    cameras: &[Camera],
) -> Result<(), BlockBuildError> {
    let camera = cameras
        .iter()
        .find(|c| c.unique_id == unique_id)
        .ok_or_else(|| {
            BlockBuildError::InvalidConfiguration(format!(
                "{}: camera '{}' (unique id {}) is no longer connected",
                block, name, unique_id
            ))
        })?;
    if cameras
        .iter()
        .any(|c| c.unique_id != unique_id && c.name == camera.name)
    {
        warn!(
            "{}: another camera is also called '{}'; the check of the opened camera cannot tell them apart",
            block, camera.name
        );
    }
    let stale = element.property::<i32>("device-index");
    if stale != camera.index {
        info!(
            "{}: camera '{}' moved from index {} to {} in macOS' device list",
            block, name, stale, camera.index
        );
    }
    element.set_property("device-index", camera.index);
    Ok(())
}

/// The cameras `avfdeviceprovider` lists right now.
///
/// `gst_device_provider_get_devices` returns the list from the first probe
/// once the provider has been started, which Strom's long-running device
/// monitor does, so this calls the provider's probe directly. The AVF
/// provider has no start or stop, so probing it does not touch the provider
/// stop path that crashed on macOS (see `make_device_element`). It queries
/// every camera's formats, which is cheap next to opening one, and runs once
/// per camera block build.
fn probe_cameras() -> Result<Vec<Camera>, &'static str> {
    let factory = gst::DeviceProviderFactory::find("avfdeviceprovider")
        .ok_or("avfdeviceprovider not found")?;
    let provider = factory
        .get()
        .ok_or("avfdeviceprovider could not be created")?;
    let class: &gst::ffi::GstDeviceProviderClass = provider.class().as_ref();
    let probe = class.probe.ok_or("avfdeviceprovider has no probe")?;
    // SAFETY: `probe` is the provider class's own probe function, called on an
    // instance of that class. It returns a new list holding a reference to
    // each device; like `gst_device_provider_get_devices`, sink any that are
    // still floating, so `from_glib_full` takes over owned references.
    let devices: Vec<gst::Device> = unsafe {
        let ptr: *mut gst::ffi::GstDeviceProvider = provider.to_glib_none().0;
        let list = probe(ptr);
        let mut item = list;
        while !item.is_null() {
            let object = (*item).data as *mut gst::glib::gobject_ffi::GObject;
            if gst::glib::gobject_ffi::g_object_is_floating(object) != 0 {
                gst::glib::gobject_ffi::g_object_ref_sink(object);
            }
            item = (*item).next;
        }
        FromGlibPtrContainer::from_glib_full(list)
    };
    Ok(devices
        .iter()
        .filter_map(|d| {
            d.find_property("device-index")?;
            Some(Camera {
                unique_id: unique_id(d)?,
                index: d.property::<i32>("device-index"),
                name: d.display_name().to_string(),
            })
        })
        .collect())
}

/// Why the camera `opened` is not the camera bound as `expected`, if it is not.
fn wrong_camera(
    block: &str,
    expected: &str,
    unique_id: &str,
    opened: Option<&str>,
) -> Option<String> {
    match opened {
        Some(opened) if opened != expected => Some(format!(
            "{}: opened camera '{}' instead of the bound camera '{}' (unique id {}); \
             macOS reordered its camera list, start the flow again",
            block, opened, expected, unique_id
        )),
        _ => None,
    }
}

/// Fail the flow if the camera `element` opened is not `expected`.
///
/// `avfvideosrc` opens the camera when it starts and sets `device-name` then,
/// before it sends caps, so the check runs on every CAPS event. Two cameras
/// with the same name pass it; only the index lookup in [`bind`] tells them
/// apart.
fn install_open_check(block: &str, element: &gst::Element, expected: &str, unique_id: &str) {
    if !element.has_property_with_type("device-name", String::static_type()) {
        return;
    }
    let Some(pad) = element.static_pad("src") else {
        return;
    };
    let block = block.to_string();
    let expected = expected.to_string();
    let unique_id = unique_id.to_string();
    let refused = AtomicBool::new(false);
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        if event.type_() != gst::EventType::Caps {
            return gst::PadProbeReturn::Ok;
        }
        if refused.load(Ordering::Relaxed) {
            return gst::PadProbeReturn::Drop;
        }
        let Some(element) = pad.parent_element() else {
            return gst::PadProbeReturn::Ok;
        };
        let opened = element.property::<Option<String>>("device-name");
        let Some(message) = wrong_camera(&block, &expected, &unique_id, opened.as_deref()) else {
            return gst::PadProbeReturn::Ok;
        };
        refused.store(true, Ordering::Relaxed);
        gst::element_error!(element, gst::ResourceError::NotFound, ("{}", message));
        // Nothing from the wrong camera goes downstream while the flow stops,
        // and downstream sees no buffers without caps, which would add an
        // "Internal data stream error" behind this one. This buffer probe
        // exists only on a camera already refused.
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            |_, _| gst::PadProbeReturn::Drop,
        );
        gst::PadProbeReturn::Drop
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// Stands in for `avfvideosrc`, which CI has no camera for: a bin with
    /// its `device-name` and `device-index` properties and a `src` pad.
    mod stand_in {
        use gstreamer as gst;
        use gstreamer::glib;
        use gstreamer::prelude::*;
        use gstreamer::subclass::prelude::*;
        use std::sync::atomic::{AtomicI32, Ordering};
        use std::sync::{LazyLock, Mutex};

        mod imp {
            use super::*;

            #[derive(Default)]
            pub struct CameraStandIn {
                pub name: Mutex<Option<String>>,
                pub index: AtomicI32,
            }

            #[glib::object_subclass]
            impl ObjectSubclass for CameraStandIn {
                const NAME: &'static str = "StromTestAvfCameraStandIn";
                type Type = super::CameraStandIn;
                type ParentType = gst::Bin;
            }

            impl ObjectImpl for CameraStandIn {
                fn properties() -> &'static [glib::ParamSpec] {
                    static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                        vec![
                            glib::ParamSpecString::builder("device-name").build(),
                            glib::ParamSpecInt::builder("device-index")
                                .minimum(-1)
                                .default_value(-1)
                                .build(),
                        ]
                    });
                    PROPS.as_ref()
                }

                fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
                    match pspec.name() {
                        "device-name" => *self.name.lock().unwrap() = value.get().unwrap(),
                        _ => self.index.store(value.get().unwrap(), Ordering::Relaxed),
                    }
                }

                fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
                    match pspec.name() {
                        "device-name" => self.name.lock().unwrap().to_value(),
                        _ => self.index.load(Ordering::Relaxed).to_value(),
                    }
                }
            }

            impl GstObjectImpl for CameraStandIn {}
            impl ElementImpl for CameraStandIn {}
            impl BinImpl for CameraStandIn {}
        }

        glib::wrapper! {
            pub struct CameraStandIn(ObjectSubclass<imp::CameraStandIn>)
                @extends gst::Bin, gst::Element, gst::Object,
                @implements gst::ChildProxy;
        }

        /// A camera that opened as `opened` at `index` and sends `buffers`
        /// frames.
        pub fn camera(opened: &str, index: i32, buffers: i32) -> gst::Element {
            gst::init().unwrap();
            let bin: CameraStandIn = glib::Object::builder()
                .property("device-name", opened)
                .property("device-index", index)
                .build();
            let src = gst::ElementFactory::make("videotestsrc")
                .property("num-buffers", buffers)
                .build()
                .unwrap();
            bin.add(&src).unwrap();
            let ghost = gst::GhostPad::builder_with_target(&src.static_pad("src").unwrap())
                .unwrap()
                .name("src")
                .build();
            bin.add_pad(&ghost).unwrap();
            bin.upcast()
        }
    }

    fn cameras() -> Vec<Camera> {
        // The provider probed the USB camera at 1; macOS now lists it first.
        vec![
            Camera {
                unique_id: "usb-logi".into(),
                index: 0,
                name: "Logi".into(),
            },
            Camera {
                unique_id: "builtin".into(),
                index: 1,
                name: "MacBook Pro Camera".into(),
            },
        ]
    }

    #[test]
    fn set_current_index_moves_the_element_to_where_the_camera_is_now() {
        let element = stand_in::camera("", 1, 0);
        set_current_index("Local Input", "Logi", "usb-logi", &element, &cameras()).unwrap();
        assert_eq!(element.property::<i32>("device-index"), 0);
    }

    #[test]
    fn set_current_index_refuses_a_camera_that_is_gone() {
        let element = stand_in::camera("", 1, 0);
        let err = set_current_index("Local Input", "Cam", "gone", &element, &cameras())
            .expect_err("a missing camera fails the build");
        assert!(err.to_string().contains("no longer connected"), "{err}");
        assert_eq!(element.property::<i32>("device-index"), 1);
    }

    /// Runs `camera` into a counting sink with the open check for "Logi";
    /// returns the error the flow failed with, if any, and the frames that
    /// reached the sink.
    fn run_open_check(camera: gst::Element) -> (Option<String>, usize) {
        install_open_check("Local Input", &camera, "Logi", "usb-logi");
        let sink = gst::ElementFactory::make("fakesink")
            .property("signal-handoffs", true)
            .property("sync", false)
            .build()
            .unwrap();
        let frames = Arc::new(AtomicUsize::new(0));
        let counter = frames.clone();
        sink.connect("handoff", false, move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            None
        });
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([&camera, &sink]).unwrap();
        camera.link(&sink).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        let mut error = None;
        while let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(10)) {
            match msg.view() {
                gst::MessageView::Error(e) => {
                    error = Some(e.error().to_string());
                    break;
                }
                gst::MessageView::Eos(_) => break,
                _ => {}
            }
        }
        pipeline.set_state(gst::State::Null).unwrap();
        (error, frames.load(Ordering::Relaxed))
    }

    #[test]
    fn open_check_fails_the_flow_on_another_camera() {
        let (error, frames) = run_open_check(stand_in::camera("MacBook Pro Camera", 1, 5));
        let error = error.expect("the wrong camera fails the flow");
        assert!(error.contains("MacBook Pro Camera"), "{error}");
        assert!(error.contains("Logi"), "{error}");
        assert_eq!(frames, 0, "no frame from the wrong camera reaches the sink");
    }

    #[test]
    fn open_check_passes_the_bound_camera() {
        let (error, frames) = run_open_check(stand_in::camera("Logi", 0, 5));
        assert_eq!(error, None);
        assert_eq!(frames, 5);
    }

    #[test]
    fn wrong_camera_accepts_a_camera_not_open_yet() {
        assert_eq!(wrong_camera("Local Input", "Logi", "usb-logi", None), None);
    }

    /// Calls the provider's probe through FFI, as a build does. A Mac without
    /// cameras (the CI runners) lists none, and still makes the call.
    #[test]
    #[cfg(target_os = "macos")]
    fn probe_cameras_lists_what_the_provider_lists() {
        gst::init().unwrap();
        let cameras = probe_cameras().expect("avfdeviceprovider probes on macOS");
        let listed: Vec<gst::Device> = gst::DeviceProviderFactory::find("avfdeviceprovider")
            .unwrap()
            .get()
            .unwrap()
            .devices()
            .into_iter()
            .collect();
        assert_eq!(cameras.len(), listed.len());
        for device in &listed {
            let id = unique_id(device).expect("AVF cameras carry avf.unique_id");
            assert!(
                cameras.iter().any(|c| c.unique_id == id),
                "{id} not found again"
            );
        }
    }
}
