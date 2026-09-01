//! Diagnostic pad probes for WHEP input blocks.
//!
//! Activated by setting `STROM_WHEP_PROBE=1`. When enabled, BUFFER pad probes
//! are installed at key points in the WHEP pipeline chain (from NiceSrc through
//! to LiveAdder) to count packets passing each element. A periodic reporter
//! writes changed counters to a file for post-mortem analysis.
//!
//! The probe callbacks are on the hottest GStreamer path (per-buffer). They use
//! only `AtomicU64::fetch_add` — no mutex, no allocation, no string formatting.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Element name prefixes we want to probe inside webrtcbin/rtpbin.
const PROBED_PREFIXES: &[&str] = &[
    "nicesrc",
    "nicesink",
    "dtlssrtpdec",
    "dtlssrtpdemux",
    "srtpdec",
    "rtprtxreceive",
    "rtpreddec",
    "rtpstorage",
    "rtpssrcdemux",
    "rtpjitterbuffer",
    "rtpptdemux",
    "rtpulpfecdec",
    "rtpopusdepay",
    "opusdec",
    "rtpsession",
    "rtpfunnel",
];

/// Reporter interval.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// A single probe counter.
struct ProbeEntry {
    name: String,
    counter: Arc<AtomicU64>,
}

/// Registry of all probe counters for one WHEP input block.
///
/// Registration (adding counters) takes a mutex — this happens infrequently
/// when elements/pads are created. The probe callbacks never touch the mutex;
/// they only do `AtomicU64::fetch_add`.
pub struct WhepProbeRegistry {
    entries: Mutex<Vec<ProbeEntry>>,
    running: Arc<AtomicBool>,
    flow_id: String,
}

impl WhepProbeRegistry {
    fn new(flow_id: String) -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            running: Arc::new(AtomicBool::new(true)),
            flow_id,
        }
    }

    /// Register a new counter and return the Arc to use in the probe closure.
    fn register(&self, name: String) -> Arc<AtomicU64> {
        let counter = Arc::new(AtomicU64::new(0));
        let mut entries = self.entries.lock().unwrap();
        // Avoid duplicates
        if entries.iter().any(|e| e.name == name) {
            debug!(name = %name, "WHEP probe already registered, skipping");
            return entries
                .iter()
                .find(|e| e.name == name)
                .unwrap()
                .counter
                .clone();
        }
        debug!(name = %name, "WHEP probe registered");
        entries.push(ProbeEntry {
            name,
            counter: counter.clone(),
        });
        counter
    }

    /// Snapshot all counters (name → value).
    fn snapshot(&self) -> Vec<(String, u64)> {
        let entries = self.entries.lock().unwrap();
        entries
            .iter()
            .map(|e| (e.name.clone(), e.counter.load(Ordering::Relaxed)))
            .collect()
    }
}

impl Drop for WhepProbeRegistry {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// Check if WHEP probing is enabled via environment variable.
pub fn whep_probe_enabled() -> bool {
    std::env::var("STROM_WHEP_PROBE").is_ok_and(|v| v == "1")
}

/// Install a BUFFER probe on a pad, incrementing the given counter.
fn install_buffer_probe(pad: &gst::Pad, counter: Arc<AtomicU64>) {
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
}

/// Build a human-readable probe name from element + pad.
fn probe_name(element: &gst::Element, pad: &gst::Pad) -> String {
    format!("{}:{}", element.name(), pad.name())
}

/// Returns true if the element name matches one of our probed prefixes.
fn should_probe(element_name: &str) -> bool {
    PROBED_PREFIXES
        .iter()
        .any(|prefix| element_name.starts_with(prefix))
}

/// Install probes on all existing src pads of an element, and set up
/// pad-added handler for dynamic pads (e.g. rtpssrcdemux).
fn install_probes_on_element(registry: &Arc<WhepProbeRegistry>, element: &gst::Element) {
    let element_name = element.name().to_string();

    // Probe existing src pads
    for pad in element.src_pads() {
        let name = format!("{}:{}", element_name, pad.name());
        let counter = registry.register(name);
        install_buffer_probe(&pad, counter);
    }

    // Watch for dynamically created pads (crucial for rtpssrcdemux)
    let registry_weak = Arc::downgrade(registry);
    let element_weak = element.downgrade();
    element.connect_pad_added(move |_element, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        let Some(registry) = registry_weak.upgrade() else {
            return;
        };
        let Some(element) = element_weak.upgrade() else {
            return;
        };
        let name = probe_name(&element, pad);
        let counter = registry.register(name);
        install_buffer_probe(pad, counter);
    });
}

/// Set up deep-element-added on a bin to catch all internal elements
/// created by webrtcbin/rtpbin.
fn install_deep_element_probes(registry: &Arc<WhepProbeRegistry>, bin: &gst::Bin) {
    // Probe already-existing children
    for element in bin.iterate_recurse().into_iter().flatten() {
        let name = element.name().to_string();
        if should_probe(&name) {
            install_probes_on_element(registry, &element);
        }
    }

    // Catch future dynamically added elements
    let registry_clone = Arc::clone(registry);
    bin.connect("deep-element-added", false, move |values| {
        let element = values[2].get::<gst::Element>().unwrap();
        let element_name = element.name();
        if should_probe(&element_name) {
            install_probes_on_element(&registry_clone, &element);
        }
        None
    });
}

/// Spawn the file-based reporter task. Writes to `/tmp/strom-whep-probe-{flow_id}.log`.
fn spawn_reporter(registry: Arc<WhepProbeRegistry>) -> tokio::task::JoinHandle<()> {
    let running = registry.running.clone();
    let flow_id = registry.flow_id.clone();

    tokio::spawn(async move {
        let dir =
            strom_types::env::var_opt("STROM_WHEP_PROBE_DIR").unwrap_or_else(|| "/tmp".to_string());
        let path = format!("{}/strom-whep-probe-{}.log", dir, flow_id);
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to open WHEP probe log file {}: {}", path, e);
                return;
            }
        };
        let file = Arc::new(Mutex::new(file));
        info!("WHEP probe reporter writing to {}", path);

        let mut interval = tokio::time::interval(REPORT_INTERVAL);
        let mut prev_values: HashMap<String, u64> = HashMap::new();

        loop {
            interval.tick().await;
            if !running.load(Ordering::Relaxed) {
                break;
            }

            let snapshot = registry.snapshot();
            let mut changed: Vec<String> = Vec::new();

            for (name, value) in &snapshot {
                let prev = prev_values.get(name).copied().unwrap_or(0);
                if *value != prev {
                    changed.push(format!("{}={}", name, value));
                }
            }

            if !changed.is_empty() {
                let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
                let line = format!("[{}] {}\n", timestamp, changed.join(" "));
                if let Ok(mut f) = file.lock() {
                    let _ = f.write_all(line.as_bytes());
                    let _ = f.flush();
                }
            }

            prev_values = snapshot.into_iter().collect();
        }

        // Final snapshot on shutdown
        let snapshot = registry.snapshot();
        let non_zero: Vec<String> = snapshot
            .iter()
            .filter(|(_, v)| *v > 0)
            .map(|(name, value)| format!("{}={}", name, value))
            .collect();
        if !non_zero.is_empty() {
            let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
            let line = format!("[{}] FINAL {}\n", timestamp, non_zero.join(" "));
            if let Ok(mut f) = file.lock() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
        info!("WHEP probe reporter stopped for flow {}", flow_id);
    })
}

/// Set up WHEP probes on a whepsrc or whepclientsrc bin.
///
/// Call this from `build_whepsrc` / `build_whepclientsrc` after the bin is
/// created but before it starts. Returns an `Arc<WhepProbeRegistry>` that
/// should be passed through to `setup_stream_with_caps_detection` and
/// `setup_audio_decode_chain` for probing the downstream chain.
///
/// Returns `None` if probing is not enabled.
pub fn setup_whep_probes(bin: &gst::Element, flow_id: &str) -> Option<Arc<WhepProbeRegistry>> {
    if !whep_probe_enabled() {
        return None;
    }

    info!("WHEP probes enabled for flow {}", flow_id);

    let registry = Arc::new(WhepProbeRegistry::new(flow_id.to_string()));

    // Install deep-element-added on the whepsrc bin
    if let Ok(bin) = bin.clone().downcast::<gst::Bin>() {
        install_deep_element_probes(&registry, &bin);
    }

    // Spawn the reporter
    let _reporter_handle = spawn_reporter(Arc::clone(&registry));

    Some(registry)
}

/// Install a probe on a named element's src pad (for elements outside webrtcbin).
///
/// Use this for identity, audioconvert, audioresample, etc. that are created
/// directly in the WHEP block code.
pub fn probe_element_src(registry: &Arc<WhepProbeRegistry>, element: &gst::Element) {
    if let Some(pad) = element.static_pad("src") {
        let name = probe_name(element, &pad);
        let counter = registry.register(name);
        install_buffer_probe(&pad, counter);
    }
}

/// Install a probe on a specific pad (e.g. liveadder sink pad).
pub fn probe_pad(registry: &Arc<WhepProbeRegistry>, element: &gst::Element, pad: &gst::Pad) {
    let name = probe_name(element, pad);
    let counter = registry.register(name);
    install_buffer_probe(pad, counter);
}
