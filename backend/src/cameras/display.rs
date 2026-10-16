//! How Strom names a camera, and the camera picker's device list.
//!
//! Strom shows a camera by its display name everywhere; the device id stays in
//! the background (it is what a block's `camera` property stores). The phone
//! reports a name in its hello, but an iOS 16+ app cannot read the user's
//! device name and reports only "iPhone" or "iPad". Such a generic name is
//! replaced by a default derived from the model and the device id, so two
//! phones of the same kind can be told apart.

use std::collections::HashMap;

use strom_types::discovery::{DeviceCategory, DeviceResponse};
use tallycam_hub::protocol::DeviceSnapshot;

/// Provider name of camera entries in the device list.
pub const CAMERA_PROVIDER: &str = "tallycam";

/// Names a phone reports that say nothing about which phone it is.
const GENERIC_NAMES: &[&str] = &["iphone", "ipad", "ipod", "ipod touch", "android", "phone"];

/// Marketing names of recent Apple model ids. Others show as the model id.
const MODEL_NAMES: &[(&str, &str)] = &[
    ("iPhone15,4", "iPhone 15"),
    ("iPhone15,5", "iPhone 15 Plus"),
    ("iPhone16,1", "iPhone 15 Pro"),
    ("iPhone16,2", "iPhone 15 Pro Max"),
    ("iPhone17,1", "iPhone 16 Pro"),
    ("iPhone17,2", "iPhone 16 Pro Max"),
    ("iPhone17,3", "iPhone 16"),
    ("iPhone17,4", "iPhone 16 Plus"),
    ("iPhone17,5", "iPhone 16e"),
    ("iPhone18,1", "iPhone 17 Pro"),
    ("iPhone18,2", "iPhone 17 Pro Max"),
    ("iPhone18,3", "iPhone 17"),
    ("iPhone18,4", "iPhone Air"),
];

/// The name Strom shows for a camera: the name the phone reports, or, when
/// that is empty or only a generic word like "iPhone", "<model> · <id start>"
/// (e.g. "iPhone 16 · 55dd").
pub fn display_name(id: &str, reported_name: &str, model: &str) -> String {
    let reported = reported_name.trim();
    let generic = reported.is_empty()
        || GENERIC_NAMES.contains(&reported.to_ascii_lowercase().as_str())
        || reported.eq_ignore_ascii_case(model.trim());
    if !generic {
        return reported.to_string();
    }
    let model = model.trim();
    let kind = MODEL_NAMES
        .iter()
        .find(|(id, _)| *id == model)
        .map(|(_, name)| *name)
        .or(Some(model).filter(|m| !m.is_empty()))
        .or(Some(reported).filter(|r| !r.is_empty()))
        .unwrap_or("Camera");
    let short: String = id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(4)
        .collect::<String>()
        .to_ascii_lowercase();
    if short.is_empty() {
        kind.to_string()
    } else {
        format!("{kind} · {short}")
    }
}

/// The display name of a hub device.
pub fn snapshot_name(d: &DeviceSnapshot) -> String {
    display_name(&d.id, &d.info.name, &d.info.model)
}

/// A known camera as an entry of `GET /api/discovery/devices?category=camera`:
/// the id is the device id (what the `camera` property stores), the name its
/// display name.
pub fn device_response(d: &DeviceSnapshot) -> DeviceResponse {
    let mut properties = HashMap::new();
    properties.insert("online".to_string(), d.online.to_string());
    for (key, value) in [
        ("model", &d.info.model),
        ("platform", &d.info.platform),
        ("app_version", &d.info.app_version),
        ("reported_name", &d.info.name),
    ] {
        if !value.is_empty() {
            properties.insert(key.to_string(), value.clone());
        }
    }
    if let Some(fingerprint) = &d.key_fingerprint {
        properties.insert("key_fingerprint".to_string(), fingerprint.clone());
    }
    DeviceResponse {
        id: d.id.clone(),
        name: snapshot_name(d),
        device_class: "Video/Source".to_string(),
        category: DeviceCategory::Camera,
        provider: CAMERA_PROVIDER.to_string(),
        properties,
        first_seen_secs_ago: 0,
        last_seen_secs_ago: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallycam_hub::protocol::DeviceInfo;

    const ID: &str = "55dd27030a5466fcc9d72a6e7ecb29eb";

    #[test]
    fn a_real_name_is_kept() {
        assert_eq!(
            display_name(ID, "  Camera 2 ", "iPhone17,3"),
            "Camera 2".to_string()
        );
    }

    #[test]
    fn a_generic_name_becomes_model_and_id_start() {
        assert_eq!(display_name(ID, "iPhone", "iPhone17,3"), "iPhone 16 · 55dd");
        assert_eq!(display_name(ID, "", "iPhone17,3"), "iPhone 16 · 55dd");
        assert_eq!(display_name(ID, "iPad", "iPad14,1"), "iPad14,1 · 55dd");
        assert_eq!(display_name(ID, "iPad14,1", "iPad14,1"), "iPad14,1 · 55dd");
    }

    #[test]
    fn without_a_model_the_reported_word_or_camera_is_used() {
        assert_eq!(display_name(ID, "iPhone", ""), "iPhone · 55dd");
        assert_eq!(display_name("phone-1", "", ""), "Camera · phon");
        assert_eq!(display_name("", "", ""), "Camera");
    }

    fn snapshot(name: &str, online: bool) -> DeviceSnapshot {
        DeviceSnapshot {
            id: ID.to_string(),
            info: DeviceInfo {
                id: ID.to_string(),
                name: name.to_string(),
                model: "iPhone17,3".to_string(),
                platform: "ios".to_string(),
                app_version: "1.0".to_string(),
                ..Default::default()
            },
            controls: Vec::new(),
            commands: Vec::new(),
            state: Default::default(),
            status: None,
            preview: None,
            tally: Default::default(),
            online,
            key_fingerprint: Some("abcdef".to_string()),
        }
    }

    #[test]
    fn a_camera_is_listed_by_display_name_with_its_device_id() {
        let d = device_response(&snapshot("iPhone", false));
        assert_eq!(d.id, ID);
        assert_eq!(d.name, "iPhone 16 · 55dd");
        assert_eq!(d.category, DeviceCategory::Camera);
        assert_eq!(d.provider, CAMERA_PROVIDER);
        assert_eq!(d.properties["online"], "false");
        assert_eq!(d.properties["model"], "iPhone17,3");
        assert_eq!(d.properties["reported_name"], "iPhone");
        assert_eq!(d.properties["key_fingerprint"], "abcdef");

        let d = device_response(&snapshot("Stage left", true));
        assert_eq!(d.name, "Stage left");
        assert_eq!(d.properties["online"], "true");
    }
}
