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
use tracing::{debug, info};

/// Device property that holds AVFoundation's stable camera id.
const UNIQUE_ID_KEY: &str = "avf.unique_id";

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
        let cameras = probe_cameras(device)?;
        let index = index_for(&cameras, unique_id).ok_or_else(|| {
            BlockBuildError::InvalidConfiguration(format!(
                "{}: camera '{}' (unique id {}) is no longer connected",
                block, name, unique_id
            ))
        })?;
        let stale = element.property::<i32>("device-index");
        if stale != index {
            info!(
                "{}: camera '{}' moved from index {} to {} in macOS' device list",
                block, name, stale, index
            );
        }
        element.set_property("device-index", index);
    }
    install_open_check(block, element, &name, unique_id);
    Ok(())
}

/// The cameras the provider of `device` lists right now, as
/// `(unique id, device-index)`.
///
/// `gst_device_provider_get_devices` returns the list from the first probe
/// once the provider has been started, which Strom's long-running device
/// monitor does, so this calls the provider's probe directly. The AVF
/// provider has no start or stop, so probing it does not touch the provider
/// stop path that crashed on macOS (see `make_device_element`).
fn probe_cameras(device: &gst::Device) -> Result<Vec<(Option<String>, i32)>, BlockBuildError> {
    let probe_failed = |why: &str| {
        BlockBuildError::ElementCreation(format!(
            "cannot list cameras again to find '{}': {}",
            device.display_name(),
            why
        ))
    };
    let factory = gst::DeviceProviderFactory::find("avfdeviceprovider")
        .ok_or_else(|| probe_failed("avfdeviceprovider not found"))?;
    let provider = factory
        .get()
        .ok_or_else(|| probe_failed("avfdeviceprovider could not be created"))?;
    let class: &gst::ffi::GstDeviceProviderClass = provider.class().as_ref();
    let probe = class
        .probe
        .ok_or_else(|| probe_failed("avfdeviceprovider has no probe"))?;
    // SAFETY: `probe` is the provider class's own probe function, called on an
    // instance of that class. It returns a new list that owns a reference to
    // each device (`gst_device_provider_get_devices` returns it as transfer
    // full), which `from_glib_full` takes over.
    let devices: Vec<gst::Device> = unsafe {
        let ptr: *mut gst::ffi::GstDeviceProvider = provider.to_glib_none().0;
        FromGlibPtrContainer::from_glib_full(probe(ptr))
    };
    Ok(devices
        .iter()
        .filter_map(|d| {
            let index = d
                .find_property("device-index")
                .map(|_| d.property::<i32>("device-index"))?;
            Some((unique_id(d), index))
        })
        .collect())
}

/// The index macOS lists the camera `unique_id` at, in `cameras` from
/// [`probe_cameras`].
fn index_for(cameras: &[(Option<String>, i32)], unique_id: &str) -> Option<i32> {
    cameras
        .iter()
        .find(|(id, _)| id.as_deref() == Some(unique_id))
        .map(|(_, index)| *index)
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
/// before it sends caps, so the check runs on the first CAPS event. Two
/// cameras with the same name pass it; only the index lookup in [`bind`]
/// tells them apart.
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
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        if event.type_() != gst::EventType::Caps {
            return gst::PadProbeReturn::Ok;
        }
        let Some(element) = pad.parent_element() else {
            return gst::PadProbeReturn::Remove;
        };
        let opened = element.property::<Option<String>>("device-name");
        match wrong_camera(&block, &expected, &unique_id, opened.as_deref()) {
            None => gst::PadProbeReturn::Remove,
            Some(message) => {
                gst::element_error!(element, gst::ResourceError::NotFound, ("{}", message));
                // Without caps nothing from the wrong camera goes downstream
                // while the flow stops.
                gst::PadProbeReturn::Drop
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_for_finds_the_camera_where_it_is_listed_now() {
        // The provider probed the USB camera at 1; macOS now lists it first.
        let now = vec![
            (Some("usb-logi".to_string()), 0),
            (Some("builtin".to_string()), 1),
        ];
        assert_eq!(index_for(&now, "usb-logi"), Some(0));
        assert_eq!(index_for(&now, "builtin"), Some(1));
    }

    #[test]
    fn index_for_misses_a_camera_that_is_gone() {
        let now = vec![(Some("builtin".to_string()), 0), (None, 1)];
        assert_eq!(index_for(&now, "usb-logi"), None);
    }

    #[test]
    fn wrong_camera_names_both_cameras() {
        let message = wrong_camera(
            "Local Input",
            "Logi 4K Stream Edition",
            "usb-logi",
            Some("MacBook Pro Camera"),
        )
        .expect("a different camera is refused");
        assert!(message.contains("MacBook Pro Camera"), "{message}");
        assert!(message.contains("Logi 4K Stream Edition"), "{message}");
    }

    #[test]
    fn wrong_camera_accepts_the_bound_camera() {
        assert_eq!(
            wrong_camera("Local Input", "Logi", "usb-logi", Some("Logi")),
            None
        );
        // Not open yet: nothing to compare.
        assert_eq!(wrong_camera("Local Input", "Logi", "usb-logi", None), None);
    }

    /// Calls the provider's probe through FFI, as a build does. A host
    /// without cameras (the CI runners) lists none, and only checks the call.
    #[test]
    #[cfg(target_os = "macos")]
    fn probe_cameras_lists_what_the_provider_lists() {
        gst::init().unwrap();
        let Some(factory) = gst::DeviceProviderFactory::find("avfdeviceprovider") else {
            return;
        };
        let provider = factory.get().expect("avfdeviceprovider");
        // Not started, so this probes too.
        let listed: Vec<gst::Device> = provider.devices().into_iter().collect();
        let Some(first) = listed.first() else {
            return;
        };
        let cameras = probe_cameras(first).expect("probe");
        assert_eq!(cameras.len(), listed.len());
        for device in &listed {
            let id = unique_id(device).expect("AVF cameras carry avf.unique_id");
            assert!(index_for(&cameras, &id).is_some(), "{id} not found again");
        }
    }
}
