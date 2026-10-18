//! Where known phone cameras are remembered across a restart.
//!
//! One JSON file in the data directory (`cameras.json`), rewritten whole on
//! every change, on the pattern of the port reservations file. Kept: each
//! camera's identity (the phone's info and its device key), its tags and
//! when it was last seen, in display order, and the hub's production
//! settings. A camera that was admitted once (with an invite) is known after
//! a restart and reconnects by proving its key, without a new invite. A
//! camera stored without a key (an app before device keys) needs a new
//! invite. Tally, status and control values are live data and start empty.
//!
//! The hub calls the [`DeviceStore`] methods synchronously while it holds its
//! own lock, so a save blocks the hub for one small write. It happens only
//! when a phone connects, goes offline, is renamed, tagged or forgotten, or
//! when the order or the production changes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tallycam_hub::protocol::Production;
use tallycam_hub::{DeviceStore, StoredDevice};
use tracing::warn;

/// On-disk format. Files written before `production` existed load without
/// one.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    /// In display order.
    cameras: Vec<StoredDevice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    production: Option<Production>,
}

/// Reads and writes `cameras.json`.
pub struct CameraStore {
    path: PathBuf,
    /// The file's contents.
    contents: Mutex<FileFormat>,
}

impl CameraStore {
    /// The store over `cameras.json` in `data_dir`. A missing file is an
    /// empty store; an unreadable one is logged and left alone until the
    /// first save replaces it (a camera list is not worth failing startup).
    pub fn open(data_dir: impl AsRef<Path>) -> Self {
        let path = data_dir.as_ref().join("cameras.json");
        let mut contents = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<FileFormat>(&text) {
                Ok(file) => file,
                Err(e) => {
                    warn!("Ignoring camera store {}: {}", path.display(), e);
                    FileFormat::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileFormat::default(),
            Err(e) => {
                warn!("Cannot read camera store {}: {}", path.display(), e);
                FileFormat::default()
            }
        };
        contents.version = 1;
        Self {
            path,
            contents: Mutex::new(contents),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FileFormat> {
        self.contents.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Written to a temporary name and renamed, so a crash mid-write leaves
    /// the previous file intact. A failure is logged: the hub cannot act on
    /// it, and the next change writes the whole file again.
    fn write(&self, contents: &FileFormat) {
        if let Err(e) = self.try_write(contents) {
            warn!("Cannot write camera store {}: {}", self.path.display(), e);
        }
    }

    fn try_write(&self, contents: &FileFormat) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(contents)?;
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
        self.lock().cameras.clone()
    }

    fn save(&self, device: &StoredDevice) {
        let mut contents = self.lock();
        match contents
            .cameras
            .iter_mut()
            .find(|d| d.info.id == device.info.id)
        {
            Some(existing) if existing == device => return,
            Some(existing) => *existing = device.clone(),
            None => contents.cameras.push(device.clone()),
        }
        self.write(&contents);
    }

    fn remove(&self, id: &str) {
        let mut contents = self.lock();
        let before = contents.cameras.len();
        contents.cameras.retain(|d| d.info.id != id);
        if contents.cameras.len() == before {
            return;
        }
        self.write(&contents);
    }

    fn load_production(&self) -> Option<Production> {
        self.lock().production.clone()
    }

    fn save_production(&self, production: &Production) {
        let mut contents = self.lock();
        if contents.production.as_ref() == Some(production) {
            return;
        }
        contents.production = Some(production.clone());
        self.write(&contents);
    }

    /// The listed cameras first, in that order, then the others in their old
    /// order. Unknown ids are ignored.
    fn save_order(&self, ids: &[String]) {
        let mut contents = self.lock();
        let mut listed = HashSet::new();
        let mut next: Vec<StoredDevice> = ids
            .iter()
            .filter(|id| listed.insert(id.as_str()))
            .filter_map(|id| contents.cameras.iter().find(|d| &d.info.id == id).cloned())
            .collect();
        next.extend(
            contents
                .cameras
                .iter()
                .filter(|d| !listed.contains(d.info.id.as_str()))
                .cloned(),
        );
        if next == contents.cameras {
            return;
        }
        contents.cameras = next;
        self.write(&contents);
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

    /// The production and the display order survive a restart, and so do
    /// a camera's tags and when it was last seen.
    #[test]
    fn keeps_the_production_order_tags_and_last_seen() {
        let dir = tempfile::tempdir().unwrap();
        let store = CameraStore::open(dir.path());
        assert_eq!(store.load_production(), None);
        for id in ["a", "b", "c"] {
            store.save(&device(id, id));
        }
        store.save_order(&["c".into(), "x".into(), "a".into(), "c".into()]);
        let mut tagged = device("b", "b");
        tagged.tags = vec!["Studio A".into()];
        tagged.last_seen_s = Some(1_700_000_000);
        store.save(&tagged);
        store.save(&device("d", "d"));
        let production: Production = serde_json::from_value(serde_json::json!({
            "mains_hz": 60, "shutter": "mains"
        }))
        .unwrap();
        store.save_production(&production);

        let reopened = CameraStore::open(dir.path());
        let loaded = reopened.load();
        let ids: Vec<_> = loaded.iter().map(|d| d.info.id.as_str()).collect();
        assert_eq!(ids, ["c", "a", "b", "d"]);
        assert_eq!(loaded[2], tagged);
        assert_eq!(reopened.load_production(), Some(production));

        // Through the hub: an `arrange` and a production change are kept.
        let hub = tallycam_hub::Hub::with_store(std::sync::Arc::new(reopened));
        hub.arrange(vec!["d".into()]);
        let mut production = hub.production();
        production.mains_hz = 50;
        hub.set_production(production.clone()).unwrap();
        let again = CameraStore::open(dir.path());
        let ids: Vec<_> = again.load().into_iter().map(|d| d.info.id).collect();
        assert_eq!(ids, ["d", "c", "a", "b"]);
        assert_eq!(again.load_production(), Some(production));
    }

    /// A file from before the production, tags and last seen were kept.
    #[test]
    fn an_old_file_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cameras.json"),
            r#"{"version":1,"cameras":[
                {"info":{"id":"a","name":"A"},"public_key":"BAE="},
                {"info":{"id":"b","name":"B"}}
            ]}"#,
        )
        .unwrap();
        let store = CameraStore::open(dir.path());
        let loaded = store.load();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].public_key.as_deref(), Some("BAE="));
        assert!(loaded[1].tags.is_empty() && loaded[1].last_seen_s.is_none());
        assert_eq!(store.load_production(), None);
    }

    #[test]
    fn unreadable_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cameras.json"), "not json").unwrap();
        assert!(CameraStore::open(dir.path()).load().is_empty());
    }
}
