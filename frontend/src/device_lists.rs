//! Device lists for the property editor's device pickers, and the phone
//! camera picker.

use egui::Ui;
use strom_types::discovery::{DeviceCategory, DeviceResponse};
use strom_types::PropertyValue;

use crate::properties::DevicePickerActions;

/// The device lists a `Device { category }` property picks from.
#[derive(Clone, Copy)]
pub(crate) struct DeviceLists<'a> {
    pub video: &'a [DeviceResponse],
    pub audio: &'a [DeviceResponse],
    /// Phone cameras (TallyCam) Strom knows.
    pub cameras: &'a [DeviceResponse],
    /// A fetch is in flight for one of the lists.
    pub loading: bool,
}

impl<'a> DeviceLists<'a> {
    /// The list for `category`, and how to name its kind in the picker.
    /// Categories without a list show an empty picker with its refresh button.
    pub fn for_category(&self, category: DeviceCategory) -> (&'a [DeviceResponse], &'static str) {
        match category {
            DeviceCategory::VideoSource => (self.video, "video source"),
            DeviceCategory::AudioSource => (self.audio, "audio source"),
            DeviceCategory::Camera => (self.cameras, "camera"),
            DeviceCategory::AudioSink | DeviceCategory::NetworkSource | DeviceCategory::Other => {
                (&[], "device")
            }
        }
    }
}

/// Whether a camera entry is connected now.
fn online(d: &DeviceResponse) -> bool {
    d.properties.get("online").map(String::as_str) == Some("true")
}

/// A camera as the picker shows it: its display name, marked when offline.
pub(crate) fn camera_label(d: &DeviceResponse) -> String {
    if online(d) {
        d.name.clone()
    } else {
        format!("{} · offline", d.name)
    }
}

/// Picker for a camera property: the value is the device id, the picker shows
/// display names. A camera Strom does not know (forgotten, or not loaded yet)
/// shows its id.
pub(crate) fn show_camera_editor(
    ui: &mut Ui,
    value: &mut PropertyValue,
    cameras: &[DeviceResponse],
    loading: bool,
    actions: &mut DevicePickerActions,
) -> bool {
    const NONE: &str = "None (plain SRT input)";
    let PropertyValue::String(s) = value else {
        return false;
    };
    let mut changed = false;

    ui.horizontal(|ui| {
        let current = s.trim().to_string();
        let selected_display = if current.is_empty() {
            NONE.to_string()
        } else {
            cameras
                .iter()
                .find(|d| d.id == current)
                .map(camera_label)
                .unwrap_or_else(|| format!("{current} (unknown camera)"))
        };

        let combo = egui::ComboBox::from_id_salt(ui.next_auto_id())
            .selected_text(selected_display)
            .show_ui(ui, |ui| {
                if ui.selectable_label(current.is_empty(), NONE).clicked() {
                    s.clear();
                    changed = true;
                }
                if cameras.is_empty() {
                    let text = if loading {
                        "Loading…"
                    } else {
                        "(no cameras yet)"
                    };
                    ui.add_enabled(false, egui::Label::new(text));
                }
                for d in cameras {
                    let response = ui
                        .selectable_label(current == d.id, camera_label(d))
                        .on_hover_text(format!("Device id: {}", d.id));
                    if response.clicked() {
                        *s = d.id.clone();
                        changed = true;
                    }
                }
            });
        if !current.is_empty() {
            combo
                .response
                .on_hover_text(format!("Device id: {current}"));
        }

        let refresh = ui
            .add_enabled(
                !loading,
                egui::Button::new(egui_phosphor::regular::ARROWS_CLOCKWISE).small(),
            )
            .on_hover_text(if loading {
                "Loading cameras…"
            } else {
                "Reload cameras"
            });
        if refresh.clicked() {
            actions.refresh_requested = true;
        }
        if manage_cameras_button(ui) {
            actions.manage_cameras_requested = true;
        }
    });
    if cameras.is_empty() && !loading {
        ui.horizontal(|ui| {
            ui.weak("No cameras yet — add one");
            if manage_cameras_button(ui) {
                actions.manage_cameras_requested = true;
            }
        });
    }

    changed
}

/// The button that opens the camera page, where phones are added and named.
fn manage_cameras_button(ui: &mut Ui) -> bool {
    ui.small_button("Manage cameras…")
        .on_hover_text("Open the camera page in a new tab: add a phone, name it, control it")
        .clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn camera(online: bool) -> DeviceResponse {
        DeviceResponse {
            id: "55dd27030a5466fcc9d72a6e7ecb29eb".to_string(),
            name: "iPhone 16 · 55dd".to_string(),
            device_class: "Video/Source".to_string(),
            category: DeviceCategory::Camera,
            provider: "tallycam".to_string(),
            properties: HashMap::from([("online".to_string(), online.to_string())]),
            first_seen_secs_ago: 0,
            last_seen_secs_ago: 0,
        }
    }

    #[test]
    fn an_offline_camera_is_marked() {
        assert_eq!(camera_label(&camera(true)), "iPhone 16 · 55dd");
        assert_eq!(camera_label(&camera(false)), "iPhone 16 · 55dd · offline");
    }

    #[test]
    fn cameras_have_their_own_list() {
        let cameras = [camera(true)];
        let lists = DeviceLists {
            video: &[],
            audio: &[],
            cameras: &cameras,
            loading: false,
        };
        let (list, kind) = lists.for_category(DeviceCategory::Camera);
        assert_eq!(list.len(), 1);
        assert_eq!(kind, "camera");
    }
}
