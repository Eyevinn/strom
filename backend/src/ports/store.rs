//! Where port reservations are remembered across a restart.
//!
//! One JSON file in the data directory, rewritten whole on every change. A
//! Strom holds a few dozen reservations at most, so a database would be
//! ceremony — and specifically **not** the flow storage backend: port numbers
//! are host-local, and a shared PostgreSQL serving two Strom nodes would
//! conflate two different hosts' port spaces and hand the same numbers to
//! both.
//!
//! Without persistence a restart would drop every owner's reservation and the
//! next request could hand out different numbers than the clients are already
//! configured to dial.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use strom_types::ports::PortReservation;
use tokio::fs;

/// On-disk format.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    reservations: Vec<PortReservation>,
}

/// Reads and writes `port_reservations.json`.
#[derive(Debug, Clone)]
pub struct PortReservationStore {
    path: PathBuf,
}

impl PortReservationStore {
    /// A store over the reservations file in `data_dir`.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: data_dir.as_ref().join("port_reservations.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every persisted reservation, lapsed ones included — what they still
    /// hold is decided by the pool, against the current flow list.
    ///
    /// A missing or empty file is an empty set, not an error.
    pub async fn load(&self) -> Result<Vec<PortReservation>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&self.path)
            .await
            .with_context(|| format!("reading {}", self.path.display()))?;
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let file: FileFormat = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", self.path.display()))?;
        Ok(file.reservations)
    }

    /// Replace the file. Written to a temporary name and renamed, so a crash
    /// mid-write leaves the previous file intact.
    pub async fn save(&self, reservations: &[PortReservation]) -> Result<()> {
        let file = FileFormat {
            version: 1,
            reservations: reservations.to_vec(),
        };
        let json = serde_json::to_string_pretty(&file)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, json)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .await
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}
