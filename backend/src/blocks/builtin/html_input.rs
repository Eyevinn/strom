//! HTML input block — a web page as a video (and audio) source.
//!
//! Chromium renders the page offscreen through `cefsrc`, which hands the
//! pipeline BGRA frames with the page's audio riding along as buffer metadata.
//! `cefdemux` splits that audio back out, so it is only built when the block
//! actually exposes an audio pad.
//!
//! ```text
//! cefsrc -> capsfilter -> cefdemux -> video -> queue -> videoconvert -> video_output -> [video_out]
//!                                  -> audio -> queue -> audioconvert -> audioresample -> audio_output -> [audio_out]
//! ```
//!
//! Both branches of the split get a queue: they leave `cefdemux` on one
//! streaming thread, so without them a sink that blocks on one branch blocks
//! the other with it.
//!
//! Video only skips `cefdemux` entirely:
//!
//! ```text
//! cefsrc -> capsfilter -> videoconvert -> video_output -> [video_out]
//! ```
//!
//! The capsfilter is what decides the render size: `cefsrc` sizes its browser
//! from the caps negotiated downstream, so width, height and framerate are
//! properties of the block rather than of anything later in the graph.
//!
//! # Why this exists as a block
//!
//! HTML sources have been raw `cefsrc`/`cefdemux` elements in imported
//! pipelines. That works, but it leaves the operator setting caps by hand, and
//! it leaves Strom with no name for "this HTML source" — which is what the
//! DevTools endpoint needs in order to hand out a link for one page rather
//! than for every page in the instance.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::StreamMode, block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{info, warn};

/// The page shown by a block nobody has configured yet.
const DEFAULT_URL: &str = "https://github.com/Eyevinn/strom/pull/879";
const DEFAULT_WIDTH: u64 = 1920;
const DEFAULT_HEIGHT: u64 = 1080;
const DEFAULT_FRAMERATE: u64 = 30;

/// This block's definition id.
pub const BLOCK_ID: &str = "builtin.html_input";

/// The property that decides whether a link may be minted for this block.
///
/// It maps to `_block` because nothing in the pipeline reads it: the API reads
/// it from the stored block when an operator asks for a link. Storing it is
/// therefore the whole write, which is why it can change on a running flow.
pub const REMOTE_CONTROL_PROPERTY: &str = "remote_control";

/// The property holding the page to render.
pub const URL_PROPERTY: &str = "url";

/// The property that keeps an HTML source off the server's own network.
///
/// On by default, because the default is a server: the page is whatever the
/// operator - or on a shared Strom, a customer - pointed it at, and it renders
/// on air, so a page on `127.0.0.1` or `169.254.169.254` would put the
/// server's own services on screen. Off is for a Strom on the operator's own
/// machine that renders its own local pages. Anything that lets a customer
/// edit a flow must never expose it.
pub const STRICT_NETWORK_PROPERTY: &str = "strict_network";

/// The `cefsrc` property, from Strom's gstcefsrc build, that gives a browser its
/// own cookies, storage and cache instead of the process-wide ones.
pub const ISOLATED_CONTEXT_PROPERTY: &str = "isolated-context";

/// The `cefsrc` property naming the directory an isolated context persists in.
const CONTEXT_CACHE_PATH_PROPERTY: &str = "context-cache-path";

/// The `cefsrc` property that keeps session cookies in a persisted context.
/// Most logins are session cookies, so without it a flow restart logs out.
const PERSIST_SESSION_COOKIES_PROPERTY: &str = "persist-session-cookies";

/// The property naming a browser profile several HTML sources may share.
///
/// Empty gives the block a profile of its own. Blocks given the same name share
/// cookies and storage - a login made in one is seen by the others. Strom knows
/// nothing about who owns a flow, so a caller serving several customers has to
/// make the names its own, for instance by prefixing them with a tenant id.
pub const BROWSER_PROFILE_PROPERTY: &str = "browser_profile";

/// Where a block's browser profile lives, under the CEF cache directory.
///
/// Each profile is a directory of its own directly under the cache root -
/// Chromium only accepts a profile there, and silently keeps one in memory
/// anywhere deeper. It lasts as long as the cache directory does: across flow
/// restarts, and in the Docker image until the container is replaced. A block with no
/// profile name gets one derived from its flow and block ids; a named profile
/// lives under a different prefix, so no name can land on a block's own.
/// Every byte outside `[A-Za-z0-9_-]` is escaped, so distinct names never map
/// to the same directory and none can climb out of the cache root.
pub fn profile_dir(
    cache_root: &std::path::Path,
    properties: &HashMap<String, PropertyValue>,
) -> std::path::PathBuf {
    let text = |key: &str| match properties.get(key) {
        Some(PropertyValue::String(s)) => s.trim().to_string(),
        _ => String::new(),
    };
    let name = text(BROWSER_PROFILE_PROPERTY);
    let leaf = if name.is_empty() {
        format!(
            "strom-block-{}-{}",
            escape_profile_name(&text("_flow_id")),
            escape_profile_name(&text("_block_id"))
        )
    } else {
        format!("strom-named-{}", escape_profile_name(&name))
    };
    cache_root.join(leaf)
}

/// Where a raw `cefsrc` element's browser profile lives, under the CEF cache
/// directory. Under its own prefix, so it can land on neither a block's profile
/// nor a named one.
pub fn element_profile_dir(
    cache_root: &std::path::Path,
    flow_id: &str,
    element_id: &str,
) -> std::path::PathBuf {
    cache_root.join(format!(
        "strom-element-{}-{}",
        escape_profile_name(flow_id),
        escape_profile_name(element_id)
    ))
}

/// Whether this gstcefsrc can give a browser a context of its own. Only
/// Strom's patched build can; upstream creates every browser in the global
/// context, so every HTML source in the process shares one cookie jar.
pub fn plugin_isolates() -> bool {
    gst::ElementFactory::find("cefsrc")
        .and_then(|f| f.load().ok())
        .and_then(|f| gst::glib::object::ObjectClass::from_type(f.element_type()))
        .and_then(|class| class.find_property(ISOLATED_CONTEXT_PROPERTY))
        .is_some()
}

/// Give a `cefsrc` a browser context of its own, persisted in the directory
/// `dir_for` picks under the CEF cache root.
///
/// Every cefsrc in the process shares one browser context by default: one
/// cookie jar and one local storage for every HTML source on this Strom,
/// whichever flow - and on a shared Strom, whichever customer - it belongs to.
/// A plugin with isolated-context gives each its own. Older plugins lack the
/// property and keep sharing, which `who` is named in a warning about.
pub fn isolate_browser(
    cefsrc: &gst::Element,
    who: &str,
    dir_for: impl FnOnce(&std::path::Path) -> std::path::PathBuf,
) {
    if cefsrc.find_property(ISOLATED_CONTEXT_PROPERTY).is_none() {
        warn!(
            "{}: this gstcefsrc has no {} property, so the page shares cookies and storage \
             with every other HTML source in this Strom",
            who, ISOLATED_CONTEXT_PROPERTY
        );
        return;
    }
    cefsrc.set_property(ISOLATED_CONTEXT_PROPERTY, true);
    // Chromium only persists a context inside its root cache path; without one
    // the context stays in memory, isolated all the same.
    let Some(root) = std::env::var_os("GST_CEF_CACHE_LOCATION") else {
        warn!(
            "{}: no CEF cache directory, so its browser profile is kept in memory and a \
             login does not survive a flow restart",
            who
        );
        return;
    };
    let dir = dir_for(std::path::Path::new(&root));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(
            "{}: could not create browser profile {}: {} - the page keeps its session in \
             memory only",
            who,
            dir.display(),
            e
        );
        return;
    }
    cefsrc.set_property(CONTEXT_CACHE_PATH_PROPERTY, dir.to_string_lossy().as_ref());
    if cefsrc
        .find_property(PERSIST_SESSION_COOKIES_PROPERTY)
        .is_some()
    {
        cefsrc.set_property(PERSIST_SESSION_COOKIES_PROPERTY, true);
    }
}

fn escape_profile_name(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                (b as char).to_string()
            } else {
                format!("~{:02x}", b)
            }
        })
        .collect()
}

/// Largest viewport this block will negotiate, per side.
///
/// The caps field is a signed 32-bit integer, so an unbounded `u64` would wrap
/// into a negative width and fail negotiation with an error that says nothing
/// about the number the operator typed. 16384 is past any real page and well
/// inside Chromium's own texture limits.
const MAX_DIMENSION: u64 = 16384;

/// Largest framerate this block will negotiate. Chromium caps rendering far
/// below this; the bound exists so the value survives the cast to `i32`.
const MAX_FRAMERATE: u64 = 1000;

/// HTML input block builder.
pub struct HtmlInputBuilder;

fn stream_mode(properties: &HashMap<String, PropertyValue>) -> StreamMode {
    properties
        .get("stream_mode")
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(StreamMode::parse(s)),
            _ => None,
        })
        .unwrap_or(StreamMode::Video)
}

/// The page this block renders, with the same fallback the pipeline uses.
///
/// A block nobody has edited has no `url` property at all, and renders
/// [`DEFAULT_URL`]. Anything asking "what page is this block showing?" has to
/// give the same answer as `build`, so both go through here.
pub fn url(properties: &HashMap<String, PropertyValue>) -> String {
    properties
        .get(URL_PROPERTY)
        .and_then(|v| match v {
            PropertyValue::String(s) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_else(|| DEFAULT_URL.to_string())
}

/// The schemes an HTML source may render, and a remote control session may
/// navigate to.
///
/// An allowlist, not a list of what to refuse: `file:` is not the only way to
/// the filesystem (`view-source:file://` gets there too), and Chromium keeps
/// its own `chrome:`, `devtools:` and `chrome-extension:` pages behind schemes
/// of their own. Anything not named here is refused, including schemes that
/// do not exist yet.
pub const ALLOWED_SCHEMES: &[&str] = &["http", "https", "data"];

/// A URL this block may render, in the form it will be handed to Chromium.
///
/// A bare address such as `example.com` or `example.com:8080/page` is read as
/// `https://`, the way a browser's address bar would. Everything else must
/// name one of [`ALLOWED_SCHEMES`], and `http(s)` must name a host. The error
/// says what was refused in words an operator can act on.
pub fn normalize_url(raw: &str) -> Result<String, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("The URL is empty".to_string());
    }
    if url.chars().any(char::is_control) {
        return Err("The URL contains control characters".to_string());
    }

    // `name:` is a scheme unless what follows the colon is a port number, as
    // in `localhost:8080`.
    let scheme = url.split_once(':').and_then(|(prefix, rest)| {
        let mut chars = prefix.chars();
        let is_scheme = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        let is_port = rest.starts_with(|c: char| c.is_ascii_digit());
        (is_scheme && !is_port).then(|| (prefix.to_ascii_lowercase(), rest))
    });

    let Some((scheme, rest)) = scheme else {
        return normalize_url(&format!("https://{}", url));
    };
    if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
        return Err(format!(
            "{}: URLs are not allowed. An HTML source renders http, https and data URLs only.",
            scheme
        ));
    }
    if scheme != "data" {
        let host = rest.strip_prefix("//").unwrap_or("");
        if host.is_empty() || host.starts_with(['/', '?', '#']) {
            return Err(format!("{} names no host", url));
        }
    }
    Ok(format!("{}:{}", scheme, rest))
}

/// Whether this block is kept off the server's own network. Anything but an
/// explicit `false` is strict.
pub fn strict_network(properties: &HashMap<String, PropertyValue>) -> bool {
    !matches!(
        properties.get(STRICT_NETWORK_PROPERTY),
        Some(PropertyValue::Bool(false))
    )
}

/// Whether an address is the server's own or its network's: loopback,
/// private, link-local (which holds cloud metadata), carrier-grade NAT and
/// unspecified, in IPv4 or IPv6, including IPv4 mapped into IPv6.
fn is_internal_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // 100.64.0.0/10, shared address space
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_internal_ip(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7, unique local
                || (first & 0xfe00) == 0xfc00
                // fe80::/10, link-local
                || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Refuse an address on the server's own network, for a strict source.
///
/// This checks the address as written, the way Chromium will parse it:
/// `127.1`, `0x7f000001` and `[::ffff:127.0.0.1]` are all loopback. A name
/// that only resolves to an internal address gets past it, which is why the
/// browser needs its own network to be locked down as well; this closes the
/// door that needs no DNS at all.
pub fn check_destination(url: &str, strict: bool) -> Result<(), String> {
    if !strict {
        return Ok(());
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return Err(format!("{} is not a URL", url));
    };
    let internal = match parsed.host() {
        None => false,
        Some(url::Host::Ipv4(v4)) => is_internal_ip(v4.into()),
        Some(url::Host::Ipv6(v6)) => is_internal_ip(v6.into()),
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
    };
    if internal {
        return Err(format!(
            "{} is on this server or its local network, which an HTML source with Strict \
             Network Access does not reach",
            parsed.host_str().unwrap_or(url)
        ));
    }
    Ok(())
}

/// A URL this block may render: an allowed scheme, and for a strict block an
/// address off the server's own network.
pub fn checked_destination(raw: &str, strict: bool) -> Result<String, String> {
    let url = normalize_url(raw)?;
    check_destination(&url, strict)?;
    Ok(url)
}

/// The page this block renders, checked against [`ALLOWED_SCHEMES`] and, for a
/// strict block, against the server's own network.
pub fn checked_url(properties: &HashMap<String, PropertyValue>) -> Result<String, String> {
    checked_destination(&url(properties), strict_network(properties))
}

/// Load a new page into a running HTML source.
///
/// `cefsrc` loads a new `url` into a running browser, but its property does
/// not carry `GST_PARAM_MUTABLE_PLAYING`, so the generic live-write path
/// refuses it. Returns `true` when the write was this block's and has been
/// applied; the caller then skips the generic path. The value has already been
/// through [`normalize_url`] in `update_block_properties`.
///
/// Coupling note: the element id tail mirrors the `cefsrc` name in `build`.
pub fn try_apply_live_url(
    element: &gst::Element,
    element_id: &str,
    prop_name: &str,
    value: &PropertyValue,
) -> bool {
    if prop_name != URL_PROPERTY || !element_id.ends_with(":cefsrc") {
        return false;
    }
    let PropertyValue::String(url) = value else {
        return false;
    };
    element.set_property(URL_PROPERTY, url);
    info!("Loaded {} into HTML source {}", url, element_id);
    true
}

/// Whether this block's operator has allowed a remote control link for it.
pub fn remote_control_enabled(properties: &HashMap<String, PropertyValue>) -> bool {
    matches!(
        properties.get(REMOTE_CONTROL_PROPERTY),
        Some(PropertyValue::Bool(true))
    )
}

/// A viewport or framerate number, or the default when it is not one.
///
/// Out-of-range is treated the same way as zero or the wrong type: fall back
/// to the default rather than fail the flow. `max` keeps the value inside
/// `i32`, which is what the caps field is.
fn uint_property(
    properties: &HashMap<String, PropertyValue>,
    name: &str,
    fallback: u64,
    max: u64,
) -> u64 {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u),
            PropertyValue::Int(i) if *i > 0 => Some(*i as u64),
            _ => None,
        })
        .filter(|n| *n > 0 && *n <= max)
        .unwrap_or(fallback)
}

impl BlockBuilder for HtmlInputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = stream_mode(properties);
        let mut outputs = Vec::new();

        if mode.has_video() {
            outputs.push(ExternalPad {
                label: if mode.has_audio() {
                    Some("V".to_string())
                } else {
                    None
                },
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "video_output".to_string(),
                internal_pad_name: "src".to_string(),
            });
        }
        if mode.has_audio() {
            outputs.push(ExternalPad {
                label: if mode.has_video() {
                    Some("A".to_string())
                } else {
                    None
                },
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audio_output".to_string(),
                internal_pad_name: "src".to_string(),
            });
        }

        Some(ExternalPads {
            inputs: vec![],
            outputs,
        })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        let mode = stream_mode(properties);
        let url = checked_url(properties).map_err(BlockBuildError::InvalidProperty)?;
        let width = uint_property(properties, "width", DEFAULT_WIDTH, MAX_DIMENSION);
        let height = uint_property(properties, "height", DEFAULT_HEIGHT, MAX_DIMENSION);
        let framerate = uint_property(properties, "framerate", DEFAULT_FRAMERATE, MAX_FRAMERATE);

        info!(
            "Building HTML Input block instance: {} ({}x{}@{} mode={})",
            instance_id,
            width,
            height,
            framerate,
            mode.as_str()
        );

        let make = |factory: &str| -> Result<gst::Element, BlockBuildError> {
            gst::ElementFactory::make(factory).build().map_err(|e| {
                // cefsrc and cefdemux are the two that are realistically
                // absent: they come from gstcefsrc, which only the strom-full
                // image carries. Say so rather than leaving the operator with
                // a bare element-creation failure.
                if matches!(factory, "cefsrc" | "cefdemux") {
                    BlockBuildError::MissingPlugin(format!(
                        "HTML sources need the gstcefsrc plugin for `{}`, which ships in the \
                         strom-full image ({})",
                        factory, e
                    ))
                } else {
                    BlockBuildError::ElementCreation(format!(
                        "Failed to create {} for the HTML input block: {}",
                        factory, e
                    ))
                }
            })
        };

        let cefsrc = make("cefsrc")?;
        cefsrc.set_property("url", &url);

        isolate_browser(
            &cefsrc,
            &format!("HTML Input block {}", instance_id),
            |root| profile_dir(root, properties),
        );

        // cefsrc renders at whatever size is negotiated downstream, so this
        // capsfilter is the page's viewport. BGRA is what cefsrc produces and
        // what cefdemux accepts; converting happens after the split.
        let capsfilter = make("capsfilter")?;
        capsfilter.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "BGRA")
                .field("width", width as i32)
                .field("height", height as i32)
                .field("framerate", gst::Fraction::new(framerate as i32, 1))
                .build(),
        );

        let mut elements: Vec<(String, gst::Element)> = vec![
            (format!("{}:cefsrc", instance_id), cefsrc),
            (format!("{}:capsfilter", instance_id), capsfilter),
        ];
        let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = vec![(
            ElementPadRef::pad(format!("{}:cefsrc", instance_id), "src"),
            ElementPadRef::pad(format!("{}:capsfilter", instance_id), "sink"),
        )];

        // The audio only exists on the far side of cefdemux, so a video-only
        // block has no reason to build it.
        let video_source = if mode.has_audio() {
            let cefdemux = make("cefdemux")?;
            elements.push((format!("{}:cefdemux", instance_id), cefdemux));
            internal_links.push((
                ElementPadRef::pad(format!("{}:capsfilter", instance_id), "src"),
                ElementPadRef::pad(format!("{}:cefdemux", instance_id), "sink"),
            ));

            // Both branches leave cefdemux on its one streaming thread, so
            // each needs a queue of its own: without them a downstream sink
            // that blocks on one branch blocks the other with it. This is the
            // shape gstcefsrc's own documented pipeline uses. Default
            // properties - there is no latency requirement here that would
            // justify overriding them.
            let audioqueue = make("queue")?;
            let audioconvert = make("audioconvert")?;
            let audioresample = make("audioresample")?;
            let audio_output = make("identity")?;
            elements.push((format!("{}:audioqueue", instance_id), audioqueue));
            elements.push((format!("{}:audioconvert", instance_id), audioconvert));
            elements.push((format!("{}:audioresample", instance_id), audioresample));
            elements.push((format!("{}:audio_output", instance_id), audio_output));
            internal_links.push((
                ElementPadRef::pad(format!("{}:cefdemux", instance_id), "audio"),
                ElementPadRef::pad(format!("{}:audioqueue", instance_id), "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(format!("{}:audioqueue", instance_id), "src"),
                ElementPadRef::pad(format!("{}:audioconvert", instance_id), "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(format!("{}:audioconvert", instance_id), "src"),
                ElementPadRef::pad(format!("{}:audioresample", instance_id), "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(format!("{}:audioresample", instance_id), "src"),
                ElementPadRef::pad(format!("{}:audio_output", instance_id), "sink"),
            ));

            // The video side of the split needs the same treatment.
            let videoqueue = make("queue")?;
            elements.push((format!("{}:videoqueue", instance_id), videoqueue));
            internal_links.push((
                ElementPadRef::pad(format!("{}:cefdemux", instance_id), "video"),
                ElementPadRef::pad(format!("{}:videoqueue", instance_id), "sink"),
            ));

            (format!("{}:videoqueue", instance_id), "src")
        } else {
            (format!("{}:capsfilter", instance_id), "src")
        };

        if mode.has_video() {
            let videoconvert = make("videoconvert")?;
            let video_output = make("identity")?;
            elements.push((format!("{}:videoconvert", instance_id), videoconvert));
            elements.push((format!("{}:video_output", instance_id), video_output));
            internal_links.push((
                ElementPadRef::pad(video_source.0.clone(), video_source.1),
                ElementPadRef::pad(format!("{}:videoconvert", instance_id), "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(format!("{}:videoconvert", instance_id), "src"),
                ElementPadRef::pad(format!("{}:video_output", instance_id), "sink"),
            ));
        } else {
            // Audio-only still renders the page — cefdemux needs the video to
            // take the audio out of, so it has to go somewhere.
            let fakesink = make("fakesink")?;
            fakesink.set_property("sync", false);
            fakesink.set_property("async", false);
            elements.push((format!("{}:video_sink", instance_id), fakesink));
            internal_links.push((
                ElementPadRef::pad(video_source.0.clone(), video_source.1),
                ElementPadRef::pad(format!("{}:video_sink", instance_id), "sink"),
            ));
        }

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Get metadata for HTML input blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![html_input_definition()]
}

fn html_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: BLOCK_ID.to_string(),
        name: "HTML Input".to_string(),
        description: "Renders a web page as a video source through Chromium, with the page's \
                      audio as an optional second output. Needs the gstcefsrc plugin, which \
                      ships in the strom-full image."
            .to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "url".to_string(),
                label: "URL".to_string(),
                description: "Page to render: an http://, https:// or data: URL. A bare address is read as https://."
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(DEFAULT_URL.to_string())),
                mapping: PropertyMapping {
                    element_id: "cefsrc".to_string(),
                    property_name: "url".to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
            ExposedProperty {
                name: "width".to_string(),
                label: "Width".to_string(),
                description: "Viewport width in pixels. The page is rendered at this size."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_WIDTH)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "width".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "height".to_string(),
                label: "Height".to_string(),
                description: "Viewport height in pixels. The page is rendered at this size."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_HEIGHT)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "height".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "framerate".to_string(),
                label: "Framerate".to_string(),
                description: "Frames per second the page is rendered at. Chromium caps this at 60."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_FRAMERATE)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "framerate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "stream_mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "Which outputs the block exposes. Pages are usually silent, so \
                              video only is the default."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "video".to_string(),
                            label: Some("Video".to_string()),
                        },
                        EnumValue {
                            value: "audio_video".to_string(),
                            label: Some("Audio + Video".to_string()),
                        },
                        EnumValue {
                            value: "audio".to_string(),
                            label: Some("Audio".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("video".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stream_mode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: BROWSER_PROFILE_PROPERTY.to_string(),
                label: "Browser Profile".to_string(),
                description: "Cookies and storage this page keeps, and so what it stays \
                              logged in to. Empty gives this source a profile of its own. \
                              Sources given the same name share one."
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: BROWSER_PROFILE_PROPERTY.to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: REMOTE_CONTROL_PROPERTY.to_string(),
                label: "Remote Control".to_string(),
                description: "Allow an operator to be handed a link that shows this page \
                              and passes their clicks and keystrokes to it - to log in, clear \
                              a consent dialog, click a tab. The instance also needs a CEF \
                              debug port configured, and authentication. Whoever holds the \
                              link can see and change what this source is putting on air \
                              until the link expires or is revoked. With cef.full_devtools \
                              on, a link is instead full control of the browser and reaches \
                              every HTML source in the instance."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: REMOTE_CONTROL_PROPERTY.to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
            ExposedProperty {
                name: STRICT_NETWORK_PROPERTY.to_string(),
                label: "Strict Network Access".to_string(),
                description: "SECURITY: leave this on for any server deployment. On, the page \
                              cannot be pointed at this server or its local network - \
                              localhost, 127.0.0.1, private addresses, or the cloud metadata \
                              service at 169.254.169.254 - whether through its URL, remote \
                              control or set-as-start-page. Off lets a page reach everything \
                              this server can, and renders it on air. Turn it off only on \
                              your own machine, for your own local pages. A system that lets \
                              customers edit flows must never expose this setting."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: STRICT_NETWORK_PROPERTY.to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![ExternalPad {
                label: None,
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "video_output".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🌐".to_string()),
            width: Some(2.5),
            height: Some(2.0),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn video_only_is_the_default_shape() {
        let pads = HtmlInputBuilder.get_external_pads(&props(&[])).unwrap();
        assert!(pads.inputs.is_empty());
        assert_eq!(pads.outputs.len(), 1);
        assert_eq!(pads.outputs[0].name, "video_out");
        assert_eq!(pads.outputs[0].media_type, MediaType::Video);
        // A lone output needs no label to tell it apart from a sibling.
        assert!(pads.outputs[0].label.is_none());
    }

    #[test]
    fn audio_video_exposes_both_pads_labelled() {
        let pads = HtmlInputBuilder
            .get_external_pads(&props(&[(
                "stream_mode",
                PropertyValue::String("audio_video".to_string()),
            )]))
            .unwrap();
        let names: Vec<_> = pads.outputs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["video_out", "audio_out"]);
        assert_eq!(pads.outputs[0].label.as_deref(), Some("V"));
        assert_eq!(pads.outputs[1].label.as_deref(), Some("A"));
    }

    #[test]
    fn audio_only_exposes_audio_alone() {
        let pads = HtmlInputBuilder
            .get_external_pads(&props(&[(
                "stream_mode",
                PropertyValue::String("audio".to_string()),
            )]))
            .unwrap();
        assert_eq!(pads.outputs.len(), 1);
        assert_eq!(pads.outputs[0].name, "audio_out");
        assert!(pads.outputs[0].label.is_none());
    }

    #[test]
    fn sizes_fall_back_rather_than_negotiating_zero() {
        // A zero or negative viewport cannot be negotiated, and a block that
        // fails to start says less than one that renders at the default.
        let p = props(&[
            ("width", PropertyValue::UInt(0)),
            ("height", PropertyValue::Int(-1)),
            ("framerate", PropertyValue::String("thirty".to_string())),
        ]);
        assert_eq!(
            uint_property(&p, "width", DEFAULT_WIDTH, MAX_DIMENSION),
            DEFAULT_WIDTH
        );
        assert_eq!(
            uint_property(&p, "height", DEFAULT_HEIGHT, MAX_DIMENSION),
            DEFAULT_HEIGHT
        );
        assert_eq!(
            uint_property(&p, "framerate", DEFAULT_FRAMERATE, MAX_FRAMERATE),
            DEFAULT_FRAMERATE
        );
    }

    #[test]
    fn an_int_viewport_is_accepted_as_written() {
        let p = props(&[("width", PropertyValue::Int(1280))]);
        assert_eq!(
            uint_property(&p, "width", DEFAULT_WIDTH, MAX_DIMENSION),
            1280
        );
    }

    #[test]
    fn a_viewport_too_large_for_the_caps_field_falls_back() {
        // The caps field is an i32. Left unbounded, 3_000_000_000 casts to a
        // negative width and the flow fails to start on a negotiation error
        // that says nothing about the number that was typed.
        let p = props(&[
            ("width", PropertyValue::UInt(3_000_000_000)),
            ("height", PropertyValue::UInt(u64::from(u32::MAX))),
            ("framerate", PropertyValue::UInt(1 << 40)),
        ]);
        let width = uint_property(&p, "width", DEFAULT_WIDTH, MAX_DIMENSION);
        let height = uint_property(&p, "height", DEFAULT_HEIGHT, MAX_DIMENSION);
        let framerate = uint_property(&p, "framerate", DEFAULT_FRAMERATE, MAX_FRAMERATE);
        assert_eq!(width, DEFAULT_WIDTH);
        assert_eq!(height, DEFAULT_HEIGHT);
        assert_eq!(framerate, DEFAULT_FRAMERATE);
        // What the capsfilter is actually handed stays positive.
        assert!(width as i32 > 0 && height as i32 > 0 && framerate as i32 > 0);
    }

    #[test]
    fn the_largest_accepted_viewport_survives_the_cast() {
        let p = props(&[
            ("width", PropertyValue::UInt(MAX_DIMENSION)),
            ("height", PropertyValue::UInt(MAX_DIMENSION)),
        ]);
        assert_eq!(
            uint_property(&p, "width", DEFAULT_WIDTH, MAX_DIMENSION) as i32,
            MAX_DIMENSION as i32
        );
        assert_eq!(
            uint_property(&p, "height", DEFAULT_HEIGHT, MAX_DIMENSION) as i32,
            MAX_DIMENSION as i32
        );
    }

    #[test]
    fn an_unedited_block_renders_the_default_url() {
        // The pipeline falls back to DEFAULT_URL, so anything asking which
        // page a block shows has to give the same answer - a block dropped on
        // the canvas and left alone has no `url` property at all.
        assert_eq!(url(&props(&[])), DEFAULT_URL);
        assert_eq!(
            url(&props(&[("url", PropertyValue::String("   ".to_string()))])),
            DEFAULT_URL
        );
        assert_eq!(
            url(&props(&[(
                "url",
                PropertyValue::String("https://example.org/a".to_string())
            )])),
            "https://example.org/a"
        );
    }

    #[test]
    fn only_http_https_and_data_are_rendered() {
        for ok in [
            "https://example.com",
            "http://example.com/a?b#c",
            "HTTPS://example.com",
            "data:text/html,<h1>hi</h1>",
        ] {
            assert!(normalize_url(ok).is_ok(), "{} should be allowed", ok);
        }
        for refused in [
            "file:///etc/passwd",
            "FILE:///etc/passwd",
            "view-source:file:///etc/passwd",
            "chrome://settings",
            "chrome-extension://abc/page.html",
            "devtools://devtools/bundled/inspector.html",
            "javascript:alert(1)",
            "blob:https://example.com/uuid",
            "filesystem:https://example.com/temporary/x",
            "about:blank",
            "https:///etc/passwd",
            "https://",
            "",
            "   ",
            "https://example.com/\u{0}",
        ] {
            assert!(
                normalize_url(refused).is_err(),
                "{:?} must be refused",
                refused
            );
        }
    }

    #[test]
    fn a_bare_address_is_read_as_https() {
        assert_eq!(normalize_url("example.com").unwrap(), "https://example.com");
        assert_eq!(
            normalize_url(" localhost:8080/page ").unwrap(),
            "https://localhost:8080/page"
        );
        assert_eq!(
            normalize_url("example.com:443").unwrap(),
            "https://example.com:443"
        );
    }

    #[test]
    fn a_refused_url_fails_the_build_with_the_reason() {
        let properties = props(&[(
            "url",
            PropertyValue::String("file:///etc/passwd".to_string()),
        )]);
        let refused = checked_url(&properties).unwrap_err();
        assert!(refused.contains("file:"), "got {}", refused);
    }

    #[test]
    fn every_block_gets_a_profile_of_its_own() {
        let root = std::path::Path::new("/cache");
        let block = |flow: &str, id: &str| {
            profile_dir(
                root,
                &props(&[
                    ("_flow_id", PropertyValue::String(flow.to_string())),
                    ("_block_id", PropertyValue::String(id.to_string())),
                ]),
            )
        };
        // Block ids are only unique within a flow, so the flow is part of it.
        assert_ne!(block("flow-a", "html"), block("flow-b", "html"));
        assert_ne!(block("flow-a", "html"), block("flow-a", "html2"));
        assert_eq!(block("flow-a", "html"), block("flow-a", "html"));
        assert_eq!(
            block("flow-a", "html").parent(),
            Some(std::path::Path::new("/cache"))
        );
    }

    #[test]
    fn a_named_profile_is_shared_and_kept_apart_from_every_other() {
        let root = std::path::Path::new("/cache");
        let named = |name: &str| {
            profile_dir(
                root,
                &props(&[
                    ("_flow_id", PropertyValue::String("f".to_string())),
                    ("_block_id", PropertyValue::String("b".to_string())),
                    (
                        BROWSER_PROFILE_PROPERTY,
                        PropertyValue::String(name.to_string()),
                    ),
                ]),
            )
        };
        assert_eq!(named("tenant-1/login"), named("tenant-1/login"));
        // Escaping keeps names that differ only in punctuation apart.
        assert_ne!(named("tenant-1/login"), named("tenant-1_login"));
        assert_ne!(named("a~2fb"), named("a/b"));
        // A name cannot land on a block's own profile, or climb out.
        assert_ne!(
            named("strom-block-f-b"),
            profile_dir(
                root,
                &props(&[
                    ("_flow_id", PropertyValue::String("f".to_string())),
                    ("_block_id", PropertyValue::String("b".to_string())),
                ])
            )
        );
        let escaped = named("../../etc");
        assert_eq!(escaped.parent(), Some(std::path::Path::new("/cache")));
    }

    #[test]
    fn a_strict_source_does_not_reach_the_server_or_its_network() {
        for url in [
            "http://127.0.0.1:9222/json/list",
            "http://localhost:8080/api/flows",
            "http://LOCALHOST./",
            "http://app.localhost/",
            "http://127.1/",
            "http://0x7f000001/",
            "http://2130706433/",
            "http://0.0.0.0:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.5/",
            "http://172.16.1.1/",
            "http://192.168.1.1/",
            "http://100.64.0.1/",
            "http://[::1]:9222/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
        ] {
            assert!(
                checked_destination(url, true).is_err(),
                "{} must be refused for a strict source",
                url
            );
            assert!(
                checked_destination(url, false).is_ok(),
                "{} is allowed once strict is off",
                url
            );
        }
        for url in [
            "https://example.com/",
            "http://192.0.2.10/",
            "https://8.8.8.8/",
            "data:text/html,<h1>hi</h1>",
            "localhost.example.com",
        ] {
            assert!(
                checked_destination(url, true).is_ok(),
                "{} is not on the server's network",
                url
            );
        }
    }

    #[test]
    fn a_block_is_strict_unless_switched_off() {
        assert!(strict_network(&props(&[])));
        assert!(strict_network(&props(&[(
            STRICT_NETWORK_PROPERTY,
            PropertyValue::String("false".to_string())
        )])));
        assert!(!strict_network(&props(&[(
            STRICT_NETWORK_PROPERTY,
            PropertyValue::Bool(false)
        )])));
        let local = props(&[(
            URL_PROPERTY,
            PropertyValue::String("http://127.0.0.1:8080/".to_string()),
        )]);
        assert!(checked_url(&local).is_err());
        let mut loose = local.clone();
        loose.insert(
            STRICT_NETWORK_PROPERTY.to_string(),
            PropertyValue::Bool(false),
        );
        assert!(checked_url(&loose).is_ok());
    }

    #[test]
    fn a_raw_cefsrc_gets_a_profile_no_block_or_name_can_land_on() {
        let root = std::path::Path::new("/cache");
        let element = element_profile_dir(root, "f", "b");
        // Keyed by flow as well: element ids are only unique within one.
        assert_ne!(element, element_profile_dir(root, "g", "b"));
        assert_eq!(element.parent(), Some(root));
        let block = profile_dir(
            root,
            &props(&[
                ("_flow_id", PropertyValue::String("f".to_string())),
                ("_block_id", PropertyValue::String("b".to_string())),
            ]),
        );
        let named = profile_dir(
            root,
            &props(&[(
                BROWSER_PROFILE_PROPERTY,
                PropertyValue::String("strom-element-f-b".to_string()),
            )]),
        );
        assert_ne!(element, block);
        assert_ne!(element, named);
        assert_eq!(
            element_profile_dir(root, "f", "../x").parent(),
            Some(root),
            "an element id cannot climb out of the cache root"
        );
    }

    #[test]
    fn remote_control_is_off_unless_it_is_the_bool_true() {
        assert!(!remote_control_enabled(&props(&[])));
        assert!(!remote_control_enabled(&props(&[(
            REMOTE_CONTROL_PROPERTY,
            PropertyValue::Bool(false)
        )])));
        // A string is not a switch, however true it reads.
        assert!(!remote_control_enabled(&props(&[(
            REMOTE_CONTROL_PROPERTY,
            PropertyValue::String("true".to_string())
        )])));
        assert!(remote_control_enabled(&props(&[(
            REMOTE_CONTROL_PROPERTY,
            PropertyValue::Bool(true)
        )])));
    }
}
