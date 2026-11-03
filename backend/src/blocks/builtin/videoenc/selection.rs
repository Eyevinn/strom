//! Encoder selection: which registered factory a priority list resolves to.
//!
//! The registry is read once into [`FactoryInfo`] descriptors (metadata only,
//! nothing is instantiated), and [`choose_encoder`] decides over them. That
//! keeps the decision testable on every platform, with descriptors standing in
//! for factories the host does not have.

use super::mediafoundation::{is_media_foundation_plugin, MEDIA_FOUNDATION_CANDIDATE};
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::info;

/// What selection needs to know about one element factory.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct FactoryInfo {
    pub(super) name: String,
    /// The plugin that registered the feature.
    pub(super) plugin: Option<String>,
    pub(super) klass: String,
    pub(super) rank: gst::Rank,
    /// Media types of the source pad templates (`video/x-h264`, ...).
    pub(super) src_media_types: Vec<String>,
}

impl FactoryInfo {
    /// Read a factory's registry metadata. Pad templates come from the
    /// registry cache; the element is not created.
    pub(super) fn from_factory(factory: &gst::ElementFactory) -> Self {
        let src_media_types = factory
            .static_pad_templates()
            .iter()
            .filter(|t| t.direction() == gst::PadDirection::Src)
            .flat_map(|t| {
                t.caps()
                    .iter()
                    .map(|s| s.name().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        Self {
            name: factory.name().to_string(),
            plugin: factory.plugin_name().map(|p| p.to_string()),
            klass: factory
                .metadata(gst::ELEMENT_METADATA_KLASS)
                .unwrap_or_default()
                .to_string(),
            rank: factory.rank(),
            src_media_types,
        }
    }

    /// GStreamer marks dedicated-hardware elements with `Hardware` in the klass.
    pub(super) fn is_hardware(&self) -> bool {
        self.klass.split('/').any(|part| part == "Hardware")
    }

    fn is_media_foundation(&self) -> bool {
        is_media_foundation_plugin(self.plugin.as_deref())
    }

    fn is_video_encoder_for(&self, media_type: &str) -> bool {
        let parts: Vec<&str> = self.klass.split('/').collect();
        parts.contains(&"Encoder")
            && parts.contains(&"Video")
            && self.src_media_types.iter().any(|m| m == media_type)
    }
}

/// Read the registry entries a priority list can resolve to: each named
/// candidate, and for [`MEDIA_FOUNDATION_CANDIDATE`] every feature of the
/// `mediafoundation` plugin.
pub(super) fn registry_snapshot(candidates: &[&str]) -> Vec<FactoryInfo> {
    let mut factories: Vec<FactoryInfo> = candidates
        .iter()
        .filter(|c| **c != MEDIA_FOUNDATION_CANDIDATE)
        .filter_map(|c| gst::ElementFactory::find(c))
        .map(|f| FactoryInfo::from_factory(&f))
        .collect();
    if candidates.contains(&MEDIA_FOUNDATION_CANDIDATE) {
        let registry = gst::Registry::get();
        factories.extend(
            registry
                .features_by_plugin(super::mediafoundation::MEDIA_FOUNDATION_PLUGIN)
                .into_iter()
                .filter_map(|f| f.downcast::<gst::ElementFactory>().ok())
                .map(|f| FactoryInfo::from_factory(&f)),
        );
    }
    factories
}

/// The factories one candidate stands for, in the order to try them.
///
/// A named candidate is that factory. [`MEDIA_FOUNDATION_CANDIDATE`] is every
/// video encoder of the `mediafoundation` plugin that produces `media_type`,
/// highest rank first. The plugin registers one feature per device (the first
/// as `mf{codec}enc` at the given rank, later ones one rank lower), so the
/// names are not known in advance.
fn expand_candidate<'a>(
    candidate: &str,
    media_type: &str,
    factories: &'a [FactoryInfo],
) -> Vec<&'a FactoryInfo> {
    if candidate == MEDIA_FOUNDATION_CANDIDATE {
        let mut found: Vec<&FactoryInfo> = factories
            .iter()
            .filter(|f| f.is_media_foundation() && f.is_video_encoder_for(media_type))
            .collect();
        // Stable: equal ranks keep registry order.
        found.sort_by_key(|f| std::cmp::Reverse(f.rank));
        found
    } else {
        factories.iter().filter(|f| f.name == candidate).collect()
    }
}

/// The first usable factory for the priority list `candidates`.
///
/// Skips a factory whose rank is NONE (disabled, e.g. via
/// `GST_PLUGIN_FEATURE_RANK`), and a Media Foundation factory without the
/// Hardware klass: the plugin registers Microsoft's software MFT under the
/// same naming and rank as the hardware ones, and x264/x265/libvpx are the
/// better software encoders.
pub(super) fn choose_encoder(
    candidates: &[&str],
    media_type: &str,
    factories: &[FactoryInfo],
) -> Option<String> {
    for candidate in candidates {
        let options = expand_candidate(candidate, media_type, factories);
        if options.is_empty() {
            info!("Encoder not available: {}", candidate);
            continue;
        }
        for factory in options {
            if factory.rank == gst::Rank::NONE {
                info!("Encoder disabled (rank=0): {}", factory.name);
                continue;
            }
            if factory.is_media_foundation() && !factory.is_hardware() {
                info!(
                    "Skipping {}: a software Media Foundation transform, not a hardware encoder",
                    factory.name
                );
                continue;
            }
            info!(
                "Found available encoder: {} (rank={:?})",
                factory.name, factory.rank
            );
            return Some(factory.name.clone());
        }
    }
    None
}
