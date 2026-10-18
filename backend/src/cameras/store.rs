//! Where known phone cameras are remembered across a restart.
//!
//! One JSON file in the data directory (`cameras.json`), rewritten whole on
//! every change, on the pattern of the port reservations file. Only identity
//! is kept: the phone's info and its device key. A camera that was admitted
//! once (with an invite) is known after a restart and reconnects by proving
//! its key, without a new invite. A camera stored without a key (an app
//! before device keys) needs a new invite. Tally, status and control values
//! are live data and start empty.
//!
//! The hub calls [`DeviceStore::save`] synchronously while it holds its own
//! lock, so a save blocks the hub for one small write. It happens only when a
//! phone is new, renamed or forgotten.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tallycam_hub::{DeviceStore, StoredDevice};
use tracing::warn;

/// On-disk format.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    cameras: Vec<StoredDevice>,
}

/// Reads and writes `cameras.json`.
pub struct CameraStore {
    path: PathBuf,
    /// The file's contents, in display order.
    cameras: Mutex<Vec<StoredDevice>>,
}

impl CameraStore {
    /// The store over `cameras.json` in `data_dir`. A missing file is an
    /// empty store; an unreadable one is logged and left alone until the
    /// first save replaces it (a camera list is not worth failing startup).
    pub fn open(data_dir: impl AsRef<Path>) -> Self {
        let path = data_dir.as_ref().join("cameras.json");
        let cameras = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<FileFormat>(&text) {
                Ok(file) => file.cameras,
                Err(e) => {
                    warn!("Ignoring camera store {}: {}", path.display(), e);
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                warn!("Cannot read camera store {}: {}", path.display(), e);
                Vec::new()
            }
        };
        Self {
            path,
            cameras: Mutex::new(cameras),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Written to a temporary name and renamed, so a crash mid-write leaves
    /// the previous file intact.
    fn write(&self, cameras: &[StoredDevice]) -> std::io::Result<()> {
        let file = FileFormat {
            version: 1,
            cameras: cameras.to_vec(),
        };
        let json = serde_json::to_string_pretty(&file)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }
}

impl DeviceStore for CameraStore {
    fn load(&self) -> Vec<StoredDevice> {
        self.cameras
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn save(&self, device: &StoredDevice) {
        let mut cameras = self.cameras.lock().unwrap_or_else(|e| e.into_inner());
        match cameras.iter_mut().find(|d| d.info.id == device.info.id) {
            Some(existing) if existing == device => return,
            Some(existing) => *existing = device.clone(),
            None => cameras.push(device.clone()),
        }
        if let Err(e) = self.write(&cameras) {
            warn!("Cannot write camera store {}: {}", self.path.display(), e);
        }
    }

    fn remove(&self, id: &str) {
        let mut cameras = self.cameras.lock().unwrap_or_else(|e| e.into_inner());
        let before = cameras.len();
        cameras.retain(|d| d.info.id != id);
        if cameras.len() == before {
            return;
        }
        if let Err(e) = self.write(&cameras) {
            warn!("Cannot write camera store {}: {}", self.path.display(), e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> StoredDevice {
        StoredDevice {
            info: serde_json::from_value(serde_json::json!({"id": id, "name": name})).unwrap(),
            public_key: None,
            tags: Vec::new(),
            last_seen_s: None,
        }
    }

    #[test]
    fn round_trips_in_order_and_replaces_changed_info() {
        let dir = tempfile::tempdir().unwrap();
        let store = CameraStore::open(dir.path());
        assert!(store.load().is_empty());
        store.save(&device("b", "Cam B"));
        store.save(&device("a", "Cam A"));
        store.save(&device("b", "Stage left"));

        let reopened = CameraStore::open(dir.path());
        let names: Vec<_> = reopened.load().into_iter().map(|d| d.info.name).collect();
        assert_eq!(names, ["Stage left", "Cam A"]);
        let text = std::fs::read_to_string(reopened.path()).unwrap();
        assert!(text.contains("\"version\": 1"));
    }

    #[test]
    fn keeps_the_device_key_and_forgets_removed_cameras() {
        let dir = tempfile::tempdir().unwrap();
        let store = CameraStore::open(dir.path());
        let mut keyed = device("k", "Keyed");
        keyed.public_key = Some("BAE=".into());
        store.save(&keyed);
        store.save(&device("old", "Before device keys"));
        store.remove("old");
        store.remove("unknown");

        let reopened = CameraStore::open(dir.path());
        assert_eq!(reopened.load(), [keyed]);
        // A file written before device keys still loads (no key).
        std::fs::write(
            reopened.path(),
            r#"{"version":1,"cameras":[{"info":{"id":"a","name":"A"}}]}"#,
        )
        .unwrap();
        let old = CameraStore::open(dir.path()).load();
        assert_eq!(old[0].public_key, None);
    }

    #[test]
    fn unreadable_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cameras.json"), "not json").unwrap();
        assert!(CameraStore::open(dir.path()).load().is_empty());
    }
}
