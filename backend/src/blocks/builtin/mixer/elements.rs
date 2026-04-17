use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::OnceLock;
use tracing::{error, warn};

use super::properties::db_to_linear;
use super::EQ_BAND_TYPE_BELL;

/// Cached result of checking whether audiomixer supports the force-live property.
static AUDIOMIXER_HAS_FORCE_LIVE: OnceLock<bool> = OnceLock::new();

/// Create a configured audiomixer element with force-live, latency, and start-time-selection.
pub(super) fn make_audiomixer(
    name: &str,
    force_live: bool,
    latency_ms: u64,
    min_upstream_latency_ms: u64,
) -> Result<gst::Element, BlockBuildError> {
    // Check if force-live is available (construct-only, must be set at build time)
    let has_force_live = *AUDIOMIXER_HAS_FORCE_LIVE.get_or_init(|| {
        gst::ElementFactory::make("audiomixer")
            .build()
            .map(|probe| probe.find_property("force-live").is_some())
            .unwrap_or(false)
    });

    let mut builder = gst::ElementFactory::make("audiomixer").name(name);
    if has_force_live {
        builder = builder.property("force-live", force_live);
    }
    let mixer = builder
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audiomixer {}: {}", name, e)))?;

    // start-time-selection=zero: match compositor behaviour so A/V stay in sync
    // when audio and video pass through separate aggregators.  Also avoids a
    // GStreamer 1.26 race where the srcpad task can run before any buffer arrives,
    // causing the aggregator to pick the absolute monotonic clock time as start
    // time and wait for an impossibly far deadline.
    mixer.set_property_from_str("start-time-selection", "zero");

    // latency: aggregator timeout in nanoseconds
    let latency_ns = latency_ms * 1_000_000;
    mixer.set_property("latency", latency_ns * gst::ClockTime::NSECOND);

    // min-upstream-latency: reported to upstream elements
    if mixer.find_property("min-upstream-latency").is_some() {
        let min_upstream_ns = min_upstream_latency_ms * 1_000_000;
        mixer.set_property(
            "min-upstream-latency",
            min_upstream_ns * gst::ClockTime::NSECOND,
        );
    }

    // ignore-inactive-pads: skip pads that aren't receiving data
    if mixer.find_property("ignore-inactive-pads").is_some() {
        mixer.set_property("ignore-inactive-pads", true);
    }

    Ok(mixer)
}

/// Create a gate element, falling back to identity passthrough if unavailable.
pub(super) fn make_gate_element(
    name: &str,
    enabled: bool,
    threshold_db: f64,
    attack_ms: f64,
    release_ms: f64,
    backend: &str,
) -> Result<gst::Element, BlockBuildError> {
    if backend == "rust" {
        if let Ok(gate) = gst::ElementFactory::make("lsp-rs-gate").name(name).build() {
            gate.set_property("enabled", enabled);
            gate.set_property("open-threshold", threshold_db as f32);
            gate.set_property("close-threshold", threshold_db as f32);
            gate.set_property("attack", attack_ms as f32);
            gate.set_property("release", release_ms as f32);
            return Ok(gate);
        }
        error!("lsp-rs-gate not available for {}, using passthrough", name);
    } else if let Ok(gate) = gst::ElementFactory::make("lsp-plug-in-plugins-lv2-gate-stereo")
        .name(name)
        .build()
    {
        if gate.find_property("enabled").is_some() {
            gate.set_property("enabled", enabled);
        }
        if gate.find_property("gt").is_some() {
            gate.set_property("gt", db_to_linear(threshold_db) as f32);
        }
        if gate.find_property("at").is_some() {
            gate.set_property("at", attack_ms as f32);
        }
        if gate.find_property("rt").is_some() {
            gate.set_property("rt", release_ms as f32);
        }
        return Ok(gate);
    } else {
        error!("LV2 gate not available for {}, using passthrough", name);
    }
    gst::ElementFactory::make("identity")
        .name(name)
        .property("silent", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("gate fallback {}: {}", name, e)))
}

/// Create a compressor element, falling back to identity passthrough if unavailable.
#[allow(clippy::too_many_arguments)]
pub(super) fn make_compressor_element(
    name: &str,
    enabled: bool,
    threshold_db: f64,
    ratio: f64,
    attack_ms: f64,
    release_ms: f64,
    makeup_db: f64,
    backend: &str,
) -> Result<gst::Element, BlockBuildError> {
    if backend == "rust" {
        if let Ok(comp) = gst::ElementFactory::make("lsp-rs-compressor")
            .name(name)
            .build()
        {
            comp.set_property("enabled", enabled);
            comp.set_property("threshold", db_to_linear(threshold_db) as f32);
            comp.set_property("ratio", ratio as f32);
            comp.set_property("attack", attack_ms as f32);
            comp.set_property("release", release_ms as f32);
            comp.set_property("makeup-gain", db_to_linear(makeup_db) as f32);
            return Ok(comp);
        }
        error!(
            "lsp-rs-compressor not available for {}, using passthrough",
            name
        );
    } else if let Ok(comp) = gst::ElementFactory::make("lsp-plug-in-plugins-lv2-compressor-stereo")
        .name(name)
        .build()
    {
        if comp.find_property("enabled").is_some() {
            comp.set_property("enabled", enabled);
        }
        if comp.find_property("al").is_some() {
            comp.set_property("al", db_to_linear(threshold_db) as f32);
        }
        if comp.find_property("cr").is_some() {
            comp.set_property("cr", ratio as f32);
        }
        if comp.find_property("at").is_some() {
            comp.set_property("at", attack_ms as f32);
        }
        if comp.find_property("rt").is_some() {
            comp.set_property("rt", release_ms as f32);
        }
        if comp.find_property("mk").is_some() {
            comp.set_property("mk", db_to_linear(makeup_db) as f32);
        }
        return Ok(comp);
    } else {
        error!(
            "LV2 compressor not available for {}, using passthrough",
            name
        );
    }
    gst::ElementFactory::make("identity")
        .name(name)
        .property("silent", true)
        .build()
        .map_err(|e| {
            BlockBuildError::ElementCreation(format!("compressor fallback {}: {}", name, e))
        })
}

/// Create a parametric EQ element, falling back to identity passthrough if unavailable.
pub(super) fn make_eq_element(
    name: &str,
    enabled: bool,
    bands: &[(f64, f64, f64); 4],
    backend: &str,
) -> Result<gst::Element, BlockBuildError> {
    if backend == "rust" {
        if let Ok(eq) = gst::ElementFactory::make("lsp-rs-equalizer")
            .name(name)
            .build()
        {
            eq.set_property("enabled", enabled);
            eq.set_property("num-bands", 4u32);
            for (band, (freq, gain_db, q)) in bands.iter().enumerate() {
                eq.set_property(&format!("band{}-type", band), EQ_BAND_TYPE_BELL);
                eq.set_property(&format!("band{}-frequency", band), *freq as f32);
                eq.set_property(&format!("band{}-gain", band), *gain_db as f32); // dB directly
                eq.set_property(&format!("band{}-q", band), *q as f32);
                eq.set_property(&format!("band{}-enabled", band), true);
            }
            return Ok(eq);
        }
        error!(
            "lsp-rs-equalizer not available for {}, using passthrough",
            name
        );
    } else if let Ok(eq) =
        gst::ElementFactory::make("lsp-plug-in-plugins-lv2-para-equalizer-x8-stereo")
            .name(name)
            .build()
    {
        if eq.find_property("enabled").is_some() {
            eq.set_property("enabled", enabled);
        }
        for (band, (freq, gain_db, q)) in bands.iter().enumerate() {
            let ft_prop = format!("ft-{}", band);
            let f_prop = format!("f-{}", band);
            let g_prop = format!("g-{}", band);
            let q_prop = format!("q-{}", band);
            if eq.find_property(&ft_prop).is_some() {
                eq.set_property_from_str(&ft_prop, "Bell");
            }
            if eq.find_property(&f_prop).is_some() {
                eq.set_property(&f_prop, *freq as f32);
            }
            if eq.find_property(&g_prop).is_some() {
                eq.set_property(&g_prop, db_to_linear(*gain_db) as f32);
            }
            if eq.find_property(&q_prop).is_some() {
                eq.set_property(&q_prop, *q as f32);
            }
        }
        return Ok(eq);
    } else {
        error!("LV2 EQ not available for {}, using passthrough", name);
    }
    gst::ElementFactory::make("identity")
        .name(name)
        .property("silent", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("eq fallback {}: {}", name, e)))
}

/// Create a limiter element, falling back to identity passthrough if unavailable.
pub(super) fn make_limiter_element(
    name: &str,
    enabled: bool,
    threshold_db: f64,
    backend: &str,
) -> Result<gst::Element, BlockBuildError> {
    if backend == "rust" {
        if let Ok(lim) = gst::ElementFactory::make("lsp-rs-limiter")
            .name(name)
            .build()
        {
            lim.set_property("enabled", enabled);
            lim.set_property("threshold", threshold_db as f32); // dB directly
            return Ok(lim);
        }
        error!(
            "lsp-rs-limiter not available for {}, using passthrough",
            name
        );
    } else if let Ok(lim) = gst::ElementFactory::make("lsp-plug-in-plugins-lv2-limiter-stereo")
        .name(name)
        .build()
    {
        if lim.find_property("enabled").is_some() {
            lim.set_property("enabled", enabled);
        }
        if lim.find_property("th").is_some() {
            lim.set_property("th", db_to_linear(threshold_db) as f32);
        }
        return Ok(lim);
    } else {
        error!("LV2 limiter not available for {}, using passthrough", name);
    }
    gst::ElementFactory::make("identity")
        .name(name)
        .property("silent", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("limiter fallback {}: {}", name, e)))
}

/// Create a high-pass filter element. Uses audiocheblimit from gst-plugins-good,
/// falls back to identity passthrough if unavailable.
pub(super) fn make_hpf_element(
    name: &str,
    enabled: bool,
    cutoff_hz: f64,
) -> Result<gst::Element, BlockBuildError> {
    if let Ok(hpf) = gst::ElementFactory::make("audiocheblimit")
        .name(name)
        .build()
    {
        // mode: 0=low-pass, 1=high-pass
        hpf.set_property_from_str("mode", "high-pass");
        hpf.set_property_from_str("poles", "4"); // 24dB/oct slope
        if enabled {
            hpf.set_property("cutoff", cutoff_hz as f32);
        }
        // cutoff=0 (default) enables GstBaseTransform passthrough mode
        return Ok(hpf);
    }
    // Try audiowsinclimit as alternative
    if let Ok(hpf) = gst::ElementFactory::make("audiowsinclimit")
        .name(name)
        .build()
    {
        hpf.set_property_from_str("mode", "high-pass");
        if enabled {
            hpf.set_property("cutoff", cutoff_hz as f32);
        }
        return Ok(hpf);
    }
    warn!("No HPF plugin available for {}, using passthrough", name);
    gst::ElementFactory::make("identity")
        .name(name)
        .property("silent", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("hpf fallback {}: {}", name, e)))
}
