//! Encoder selection: which registered factory a priority list resolves to.
//!
//! The registry is read once into [`FactoryInfo`] descriptors (metadata only,
//! nothing is instantiated), and [`choose_encoder`] decides over them. That
//! keeps the decision testable on every platform, with descriptors standing in
//! for factories the host does not have.

use super::mediafoundation::is_media_foundation_plugin;
use crate::gst::software_decoder::klass_is_hardware;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::fmt;
use tracing::info;

/// One entry of an encoder priority list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Candidate {
    /// The element factory of that name.
    Named(&'static str),
    /// Every video encoder of that plugin for the codec, highest rank first.
    /// For plugins that register one feature per device under names that are
    /// not known in advance (`mediafoundation`).
    Plugin(&'static str),
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => write!(f, "{}", name),
            Self::Plugin(plugin) => write!(f, "{} encoders", plugin),
        }
    }
}

/// `names` as [`Candidate::Named`] entries.
pub(super) fn named(names: &[&'static str]) -> Vec<Candidate> {
    names.iter().copied().map(Candidate::Named).collect()
}

/// What the encoder must produce: the codec's media type and the profile the
/// block pins on its capsfilter, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EncoderOutput {
    pub(super) media_type: &'static str,
    pub(super) profile: Option<&'static str>,
}

/// One structure of a factory's source pad template.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct SrcFormat {
    pub(super) media_type: String,
    /// The `profile` values the template lists; `None` when it has no
    /// profile field (any profile may come out).
    pub(super) profiles: Option<Vec<String>>,
}

impl SrcFormat {
    fn from_structure(s: &gst::StructureRef) -> Self {
        let profiles = s.value("profile").ok().and_then(|v| {
            if let Ok(p) = v.get::<String>() {
                Some(vec![p])
            } else if let Ok(list) = v.get::<gst::List>() {
                Some(list.iter().filter_map(|v| v.get::<String>().ok()).collect())
            } else {
                None
            }
        });
        Self {
            media_type: s.name().to_string(),
            profiles,
        }
    }
}

/// What selection needs to know about one element factory.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct FactoryInfo {
    pub(super) name: String,
    /// The plugin that registered the feature.
    pub(super) plugin: Option<String>,
    pub(super) klass: String,
    pub(super) rank: gst::Rank,
    /// The source pad templates' structures.
    pub(super) src_formats: Vec<SrcFormat>,
}

impl FactoryInfo {
    /// Read a factory's registry metadata. Pad templates come from the
    /// registry cache; the element is not created.
    pub(super) fn from_factory(factory: &gst::ElementFactory) -> Self {
        let src_formats = factory
            .static_pad_templates()
            .iter()
            .filter(|t| t.direction() == gst::PadDirection::Src)
            .flat_map(|t| {
                t.caps()
                    .iter()
                    .map(SrcFormat::from_structure)
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
            src_formats,
        }
    }

    pub(super) fn is_hardware(&self) -> bool {
        klass_is_hardware(&self.klass)
    }

    fn is_media_foundation(&self) -> bool {
        is_media_foundation_plugin(self.plugin.as_deref())
    }

    fn is_video_encoder_for(&self, media_type: &str) -> bool {
        let parts: Vec<&str> = self.klass.split('/').collect();
        parts.contains(&"Encoder")
            && parts.contains(&"Video")
            && self.src_formats.iter().any(|f| f.media_type == media_type)
    }

    /// Whether the source template can produce `wanted`. A template with no
    /// structures (ANY) or no profile field counts as able to.
    pub(super) fn can_produce(&self, wanted: &EncoderOutput) -> bool {
        self.src_formats.is_empty()
            || self.src_formats.iter().any(|f| {
                f.media_type == wanted.media_type
                    && match (wanted.profile, &f.profiles) {
                        (None, _) | (_, None) => true,
                        (Some(profile), Some(listed)) => listed.iter().any(|p| p == profile),
                    }
            })
    }
}

/// Read the registry entries a priority list can resolve to: each named
/// candidate, and every feature of each [`Candidate::Plugin`].
pub(super) fn registry_snapshot(candidates: &[Candidate]) -> Vec<FactoryInfo> {
    let registry = gst::Registry::get();
    let mut factories = Vec::new();
    for candidate in candidates {
        match candidate {
            Candidate::Named(name) => factories
                .extend(gst::ElementFactory::find(name).map(|f| FactoryInfo::from_factory(&f))),
            Candidate::Plugin(plugin) => factories.extend(
                registry
                    .features_by_plugin(plugin)
                    .into_iter()
                    .filter_map(|f| f.downcast::<gst::ElementFactory>().ok())
                    .map(|f| FactoryInfo::from_factory(&f)),
            ),
        }
    }
    factories
}

/// The factories one candidate stands for, in the order to try them.
///
/// [`Candidate::Plugin`] is every video encoder of that plugin whose source
/// template has `media_type`, highest rank first. The `mediafoundation`
/// plugin registers one feature per device (the first as `mf{codec}enc` at
/// the plugin's rank, later ones one rank lower), so the names are not known
/// in advance.
fn expand_candidate<'a>(
    candidate: &Candidate,
    media_type: &str,
    factories: &'a [FactoryInfo],
) -> Vec<&'a FactoryInfo> {
    match candidate {
        Candidate::Named(name) => factories.iter().filter(|f| f.name == *name).collect(),
        Candidate::Plugin(plugin) => {
            let mut found: Vec<&FactoryInfo> = factories
                .iter()
                .filter(|f| {
                    f.plugin.as_deref() == Some(plugin) && f.is_video_encoder_for(media_type)
                })
                .collect();
            // Stable: equal ranks keep registry order.
            found.sort_by_key(|f| std::cmp::Reverse(f.rank));
            found
        }
    }
}

/// The first usable factory for the priority list `candidates`.
///
/// Skips a factory:
/// - whose rank is NONE (disabled, e.g. via `GST_PLUGIN_FEATURE_RANK`);
/// - from `mediafoundation` without the Hardware klass: the plugin registers
///   Microsoft's software MFT under the same naming and rank as the hardware
///   ones, and x264/x265/libvpx are the better software encoders;
/// - whose source template cannot produce the pinned profile: the capsfilter
///   would refuse its output (not-negotiated) instead of a later candidate
///   encoding it. An MFT, for one, lists only the profiles its device has.
pub(super) fn choose_encoder(
    candidates: &[Candidate],
    wanted: &EncoderOutput,
    factories: &[FactoryInfo],
) -> Option<String> {
    for candidate in candidates {
        let options = expand_candidate(candidate, wanted.media_type, factories);
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
            if !factory.can_produce(wanted) {
                info!(
                    "Skipping {}: its source template cannot produce {} profile={}",
                    factory.name,
                    wanted.media_type,
                    wanted.profile.unwrap_or("")
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
