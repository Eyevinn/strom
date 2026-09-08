//! RTMP output block.
//!
//! Publishes a programme to an RTMP server, muxed as FLV.
//!
//! Chain, built dynamically once the input caps are known:
//! - video: `h264parse` → `flvmux`
//! - audio, raw in: `audioconvert` → `audioresample` → `avenc_aac` → `aacparse` → `flvmux`
//! - audio, AAC in: `aacparse` → `flvmux`
//!
//! and `flvmux` → `rtmp2sink`.
//!
//! # Why the chains are built on caps rather than statically
//!
//! Two reasons, and the second is the one that bites.
//!
//! FLV carries a small closed set of codecs, so the muxer needs H.264 and AAC
//! whatever arrives. The audio side therefore has to encode raw input, because
//! **no block produces AAC as its output**: `avenc_aac` and `opusenc` exist
//! only inside `builtin.mpegtssrt_output`, `builtin.whip_output` and
//! `builtin.efpsrt_output` as internal chains, and there is no audio encoder
//! block the way `builtin.videoenc` is one for video.
//!
//! So a block that demanded encoded AAC on its pad could be fed only by an
//! input block in passthrough mode, where the encoded stream comes from
//! upstream rather than from Strom: `builtin.mpegtssrt_input`, or
//! `builtin.media_player`, whose passthrough pipeline is
//! `urisourcebin(parse-streams=true)` and so emits AAC from an AAC-bearing
//! file. AAC is the criterion rather than encoded audio, which is why
//! `builtin.efpsrt_input` is not a third: its passthrough carries Opus, and FLV
//! cannot. Neither carries a mixed programme, which is the point: `builtin.mixer`
//! outputs raw audio, so encoding inside this block is what makes it usable
//! downstream of mixing at all.
//!
//! # Why the pads are reserved before the pipeline starts, not from the probes
//!
//! An aggregator's sink pad that never carries data means `flvmux` never
//! aggregates and nothing reaches the sink, so a pad must not exist for an input
//! that will not be fed. The obvious conclusion is to request each pad from its
//! caps probe, once the input has proved it has data. **That is wrong for FLV,
//! and it fails silently.**
//!
//! `flvmux` writes the FLV header and its `onMetaData` script tag at its first
//! aggregation. A pad requested after that is granted, and its buffers are muxed
//! into the body, but the stream is never declared in the header. Measured on
//! GStreamer 1.28.6 with this block's exact topology: pads requested from the
//! probes gave `hasAudio=False hasVideo=True` with 128 audio tags in the body,
//! and pads reserved before `PLAYING` gave `hasAudio=True hasVideo=True`. A
//! player that configures its decoders from the header, which is the normal
//! thing to do, plays no sound from the first case, and it reads as an encoder or
//! network fault rather than a graph mistake.
//!
//! `builtin.mpegtssrt_output` can request late because `mpegtsmux` re-emits PAT
//! and PMT continuously, so a late pad still gets announced. FLV's header is
//! written once, so this block cannot copy that.
//!
//! So the pads are reserved in `ctx.register_element_setup`, the window after
//! construction has linked every block and before the pipeline leaves NULL,
//! which is the only point where connectivity is known and the muxer has not
//! started. `recorder.rs` documents the same race for `splitmuxsink` and reaches
//! the same conclusion. Unconnected inputs get no pad, which preserves
//! video-only and audio-only flows, and a connected input whose codec is then
//! refused hands its reserved pad back, so a refusal cannot stall the other
//! stream. The probes still choose and insert the parser or encoder chain; they
//! just link into a pad that already exists.
//!
//! # Transport security: rtmps, and what is deliberately not configurable
//!
//! `rtmps://` in the location is all that is needed. `rtmp2sink` sets its own
//! `scheme` from the URL, and its `tls-validation-flags` default to
//! `validate-all`, so the server's certificate chain and identity are both
//! checked. **This block exposes no way to weaken that**, on purpose: the only
//! thing such a switch buys is accepting a self-signed certificate, and the
//! cost is a property whose whole function is to turn off the check that makes
//! `rtmps` worth using. A deployment that needs an internal CA should trust it
//! at the host, which is where trust decisions belong.
//!
//! Both halves were measured on 2026-09-09 against a real TLS endpoint,
//! `stunnel` in front of an RTMP listener with a certificate from a private CA:
//! with validation left at its default the connection is refused with
//! "Unacceptable TLS certificate", and with a trusted certificate the same chain
//! delivered 3.0 s of H.264 and AAC through the tunnel. So the refusal is real
//! rather than a default nothing enforces.
//!
//! **A location that is not `rtmp://` or `rtmps://` is refused at build time.**
//! `rtmp2sink` accepts any string and re-serialises whatever it could parse, so
//! `"not-a-url"` silently becomes `"rtmp:/"`: a plaintext connection to nowhere,
//! with no error. When a stream key in a URL is the only thing protecting a
//! programme feed, a typo that quietly drops encryption is the failure worth
//! refusing, so `parse_rtmp_location` names what is wrong instead.
//!
//! **No secret in an RTMP URL reaches a log, and there are three of them.**
//! `user:pass@`, the stream key in the last path segment, and any token in a
//! query string. The stream key is the one worth naming: for most servers it is
//! the whole authorisation, which is exactly why the section above calls it the
//! only thing protecting the feed, so logging it would contradict that. All
//! three are masked by `redact_location`, which everything logged here goes
//! through; the host and the application survive so a line can still be
//! attributed to an output.
//!
//! `rtmp2sink` lifts `user:pass@` into its own `username` and `password`
//! properties, so reading the property back is safe. The string the operator
//! supplied is not, and that is the one a block naturally logs. **Note what is
//! not solved by any of this: the URL is stored with the flow**, in flow JSON
//! and in the UI, because `location` is an ordinary string property. Log
//! redaction is not encryption at rest.
//!
//! A plaintext `rtmp://` target that is not loopback also gets one warning at
//! build, because the key and the media both cross the network readable.
//!
//! # Video is expected to arrive encoded
//!
//! Deliberately asymmetric with audio, and for a reason rather than by
//! omission: `builtin.videoenc` already exposes encoded H.264 on an
//! `encoded_out` pad, so the operator has a block for it, and encoding video
//! inside an output block would hide a codec choice that belongs in the graph.
//! Raw video is refused with a message naming that block.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};

/// The RTMP sink element. One element, no fallback.
///
/// `rtmp2sink` rather than `rtmpsink`: both take the URL in a `location`
/// property, but `rtmpsink` is built against librtmp and is absent from some
/// builds of `gst-plugins-bad`, while `rtmp2sink` has no external dependency and
/// is what upstream points at now.
const RTMP_SINK_FACTORY: &str = "rtmp2sink";

/// The package that ships the RTMP sink on this platform.
///
/// Same shape as `ice_package_hint`, so the operator-facing message names what
/// to install rather than what is missing.
pub const fn rtmp_package_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "brew install gst-plugins-bad"
    }
    #[cfg(target_os = "windows")]
    {
        "the GStreamer MSI installer's full package set"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "gstreamer1.0-plugins-bad (Debian/Ubuntu), gstreamer1-plugins-bad-free (Fedora) \
         or gst-plugins-bad (Arch)"
    }
}

/// The message shown when the RTMP sink is unavailable.
///
/// Kept separate from the check so its wording is testable on a host where the
/// element is present.
pub fn rtmp_missing_message() -> String {
    format!(
        "RTMP Output needs the GStreamer {} element, which this installation does \
         not have. Install it with: {}",
        RTMP_SINK_FACTORY,
        rtmp_package_hint()
    )
}

/// The package that ships `avenc_aac`, for the operator-facing log line.
///
/// Worth its own message because it is the one element in the chain that a host
/// can lack while still passing `require_rtmp_sink`: `avenc_aac` comes from
/// gst-libav, packaged separately from gst-plugins-bad. It is also built inside a
/// pad probe rather than at build time, so a missing one surfaces as a log line
/// and a published stream with no audio, and an opaque GStreamer error would send
/// an operator looking in the wrong place.
pub const fn libav_package_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "brew install gst-libav"
    }
    #[cfg(target_os = "windows")]
    {
        "the GStreamer MSI installer's full package set"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "gstreamer1.0-libav (Debian/Ubuntu), gstreamer1-libav (Fedora) \
         or gst-libav (Arch)"
    }
}

/// What a `location` turned out to be, once checked.
#[derive(Debug, PartialEq, Eq)]
pub struct RtmpLocation {
    /// `true` for `rtmps://`, so the transport is TLS.
    pub tls: bool,
    /// The host, used only to decide whether a plaintext target is loopback.
    pub host: String,
    /// The URL with any credentials removed, safe to log.
    pub redacted: String,
    /// The exact string to give the sink: trimmed, and the one that was checked.
    /// Handing the sink the untrimmed original instead let a stray space reach
    /// `rtmp2sink`, which mangled it to `rtmp:/` while the log still claimed TLS.
    pub location: String,
}

/// Strip everything secret from a URL so it can be logged.
///
/// Two secrets live in an RTMP URL, not one. The obvious one is `user:pass@`.
/// The other is the **stream key**, the last path segment, which for most RTMP
/// servers is the entire authorisation: this module's own docs call it the only
/// thing protecting the programme feed, so logging it verbatim would contradict
/// them. A query string can carry a token for the same purpose. All three are
/// masked; the host and the application are kept, because an operator still has
/// to be able to tell which output a log line belongs to.
///
/// **Fails closed, and that is the whole point.** An earlier version isolated the
/// authority as "everything up to the first `/`" and returned the input unchanged
/// when it found no credentials there. A password containing an unencoded `/`,
/// which any base64 secret hits about a quarter of the time per character, then
/// put the whole credential into an `info!` line. So userinfo is taken to end at
/// the **last** `@` wherever it sits, including in a string this module cannot
/// otherwise parse, because the reason it cannot parse may be the credential
/// itself. Over-redaction is the acceptable direction: a stream name containing
/// `@` loses some of the log line, and a password never does.
pub fn redact_location(raw: &str) -> String {
    // 1. userinfo, by the last '@' anywhere.
    let without_creds = match raw.rfind('@') {
        Some(at) => {
            let after_scheme = raw.find("://").map(|i| i + 3).unwrap_or(0);
            if after_scheme > at {
                // An '@' inside the scheme is not a URL this block should echo.
                return "<redacted>".to_string();
            }
            format!("{}REDACTED{}", &raw[..after_scheme], &raw[at..])
        }
        None => raw.to_string(),
    };
    // 2. a query string, which may carry a token.
    let (path_part, had_query) = match without_creds.split_once('?') {
        Some((head, _)) => (head.to_string(), true),
        None => (without_creds, false),
    };
    // 3. the last path segment, the stream key. Only when there is a path to
    //    speak of, so a bare host is left alone.
    let after_scheme = path_part.find("://").map(|i| i + 3).unwrap_or(0);
    let masked = match path_part[after_scheme..].rfind('/') {
        Some(rel) if rel > 0 => {
            let cut = after_scheme + rel + 1;
            format!("{}STREAMKEY", &path_part[..cut])
        }
        _ => path_part,
    };
    if had_query {
        format!("{}?QUERY", masked)
    } else {
        masked
    }
}

/// Check a `location` and say what it is, or refuse it.
///
/// **Why this refuses rather than passing the string through.** `rtmp2sink`
/// accepts any string and re-serialises what it could parse, so `"not-a-url"`
/// becomes `"rtmp:/"` and a typo in the scheme silently becomes an unencrypted
/// connection to nowhere. For a stream key that is the only thing protecting a
/// programme feed, silently degrading to plaintext is the failure worth
/// preventing, so an unusable or unknown scheme is refused at build time with a
/// message naming what is wrong.
///
/// TLS itself needs no configuration here: `rtmps://` in the URL is enough,
/// `rtmp2sink` sets its own `scheme` from it, and its `tls-validation-flags`
/// default to `validate-all`. This block deliberately does not expose a way to
/// weaken that, see the module docs.
pub fn parse_rtmp_location(raw: &str) -> Result<RtmpLocation, String> {
    let trimmed = raw.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return Err(format!(
            "RTMP Output needs a location starting rtmp:// or rtmps://, but got {:?}. \
             Without a scheme the sink cannot tell an encrypted target from a plain one",
            redact_location(trimmed)
        ));
    };
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "rtmp" => false,
        "rtmps" => true,
        other => {
            return Err(format!(
                "RTMP Output cannot publish over {:?}. Use rtmp:// or, to encrypt the \
                 connection, rtmps://",
                other
            ))
        }
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host_port = match authority.rsplit_once('@') {
        Some((_creds, h)) => h,
        None => authority,
    };
    // Strip a port, and the brackets of a literal IPv6 address.
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or("").to_string(),
        None => host_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_port)
            .to_string(),
    };
    if host.is_empty() {
        return Err(format!(
            "RTMP Output needs a host in its location, but got {:?}",
            redact_location(trimmed)
        ));
    }
    Ok(RtmpLocation {
        tls,
        host,
        redacted: redact_location(trimmed),
        location: trimmed.to_string(),
    })
}

/// Whether a host is the local machine, so plaintext to it leaves nothing.
fn is_loopback(host: &str) -> bool {
    // Parsed rather than matched on a prefix: `127.0.0.1.evil.example` resolves
    // to somewhere else entirely and used to suppress the plaintext warning.
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    host.eq_ignore_ascii_case("localhost")
}

/// Confirm the sink accepted the location as the transport we think it is.
///
/// **Ground truth rather than prediction, and this is the safer half of the
/// check.** `rtmp2sink` re-serialises whatever it could parse and silently keeps
/// a degenerate result: a password containing an unencoded `/`, or a stray space
/// around the URL, both leave it holding `rtmp:/` with no host, no credentials
/// and the scheme fallen back to plaintext. `parse_rtmp_location` cannot see
/// that, because it is a guess about someone else's parser. Reading the sink back
/// is not a guess.
///
/// So a mismatch is refused rather than logged: the alternative is a flow that
/// looks configured for TLS, reports itself as such, and publishes nowhere.
/// Nothing here echoes the raw location, only the redacted form.
fn confirm_sink_accepted(
    sink: &gst::Element,
    parsed: &RtmpLocation,
) -> Result<(), BlockBuildError> {
    let readback = sink.property::<String>("location");
    if !readback.contains(&parsed.host) {
        return Err(BlockBuildError::ElementCreation(format!(
            "RTMP Output could not use the location {}: {} reduced it to {:?}, which has no \
             host. If it carries credentials, percent-encode any '/' in the password as %2F",
            parsed.redacted, RTMP_SINK_FACTORY, readback
        )));
    }
    // 0 = rtmp, 1 = rtmps, per `gst-inspect-1.0 rtmp2sink`.
    let scheme_is_tls = sink
        .property_value("scheme")
        .transform::<i32>()
        .ok()
        .and_then(|v| v.get::<i32>().ok())
        .map(|v| v == 1)
        .unwrap_or(false);
    if scheme_is_tls != parsed.tls {
        return Err(BlockBuildError::ElementCreation(format!(
            "RTMP Output asked for {} with {}, but {} settled on {}. Refusing rather than \
             publishing over a transport nobody asked for",
            if parsed.tls { "rtmps" } else { "rtmp" },
            parsed.redacted,
            RTMP_SINK_FACTORY,
            if scheme_is_tls { "rtmps" } else { "rtmp" },
        )));
    }
    Ok(())
}

/// Refuse to build when the RTMP sink is unavailable.
fn require_rtmp_sink() -> Result<(), BlockBuildError> {
    if gst::ElementFactory::find(RTMP_SINK_FACTORY).is_some() {
        return Ok(());
    }
    Err(BlockBuildError::MissingPlugin(rtmp_missing_message()))
}

/// A `flvmux` sink pad reserved before the pipeline started, handed to the caps
/// probe that will link a chain into it.
type PadSlot = Arc<Mutex<Option<gst::Pad>>>;

/// Take the pad reserved for this media type, or request one now and say so.
///
/// **Why the pad is reserved up front rather than requested here.** `flvmux`
/// writes the FLV header and its `onMetaData` script tag at its first
/// aggregation. A pad requested after that is granted and its buffers *are*
/// muxed into the body, but the stream is never declared in the header, so a
/// server or player that configures its decoders from `TypeFlags` or
/// `onMetaData`, which is the normal thing to do, plays nothing from it.
/// Measured on GStreamer 1.28.6 with this block's exact topology: requesting
/// both pads from the caps probes gave `hasAudio=False hasVideo=True` with 128
/// audio tags sitting in the body, and reserving them before `PLAYING` gave
/// `hasAudio=True hasVideo=True`.
///
/// `recorder.rs` documents the same race for `splitmuxsink` and rejects the lazy
/// route for the same reason, pointing at this hook. `builtin.mpegtssrt_output`
/// can request late because `mpegtsmux` re-emits PAT and PMT continuously, so a
/// late pad still gets announced. FLV's header is written once.
fn take_mux_pad(
    slot: &PadSlot,
    mux: &gst::Element,
    name: &str,
    instance: &str,
) -> Result<gst::Pad, String> {
    if let Ok(mut guard) = slot.lock() {
        if let Some(pad) = guard.take() {
            return Ok(pad);
        }
    }
    // Reaching here means the input was not linked when the setup hook ran, yet
    // data arrived anyway. Better to publish an under-declared stream than none,
    // but say which it is, because the symptom is silent.
    warn!(
        "RTMP {}: no '{}' pad was reserved before the pipeline started, requesting \
         one now. The FLV header will not declare this stream, so a player that \
         reads it may ignore the media",
        instance, name
    );
    request_mux_pad(mux, name)
}

/// RTMP Output block builder.
pub struct RtmpOutputBuilder;

impl BlockBuilder for RtmpOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building RTMP Output block instance: {}", instance_id);
        require_rtmp_sink()?;

        let location = properties
            .get("location")
            .and_then(|v| match v {
                PropertyValue::String(s) if !s.trim().is_empty() => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| DEFAULT_RTMP_LOCATION.to_string());

        let sync = properties
            .get("sync")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        // FLV muxer. streamable=true writes no seek table and no duration: the
        // non-streamable form rewrites the header at end of file, and a live
        // stream has no end.
        let mux_id = format!("{}:rtmp_flvmux", instance_id);
        let mux = gst::ElementFactory::make("flvmux")
            .name(&mux_id)
            .property("streamable", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("flvmux: {}", e)))?;

        let sink_id = format!("{}:rtmp_sink", instance_id);
        let sink = gst::ElementFactory::make(RTMP_SINK_FACTORY)
            .name(&sink_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("{}: {}", RTMP_SINK_FACTORY, e))
            })?;
        // Refuse an unusable location before anything is built, and never log the
        // raw one: it is where credentials live if the operator put them there.
        let parsed = parse_rtmp_location(&location).map_err(BlockBuildError::ElementCreation)?;
        // parsed.location, not the raw one: the raw may carry whitespace the sink
        // cannot parse, and it is the string that may carry credentials.
        sink.set_property("location", &parsed.location);
        sink.set_property("sync", sync);
        // async=false: don't block pipeline preroll waiting for the first
        // buffer. Without a reachable RTMP server the sink would otherwise hold
        // PAUSED->PLAYING indefinitely. Matches the SRT, WHEP, WHIP and AES67
        // sinks; block-built elements bypass add_element, so nothing sets this
        // for us.
        sink.set_property("async", false);
        sink.set_property("qos", true);
        confirm_sink_accepted(&sink, &parsed)?;

        if parsed.tls {
            info!(
                "RTMP Output configured: location={}, sync={}, sink={}, transport=rtmps \
                 (TLS, certificates validated)",
                parsed.redacted, sync, RTMP_SINK_FACTORY
            );
        } else {
            info!(
                "RTMP Output configured: location={}, sync={}, sink={}, transport=rtmp \
                 (plaintext)",
                parsed.redacted, sync, RTMP_SINK_FACTORY
            );
            if !is_loopback(&parsed.host) {
                // Said once, at build, rather than per buffer. The stream key in
                // the URL is usually the only thing protecting the programme feed,
                // and over plaintext it crosses the network in the clear along
                // with the media.
                warn!(
                    "RTMP {}: publishing to {} over plaintext rtmp. The stream key and \
                     the media are both readable in transit; use rtmps:// if the \
                     server supports it",
                    instance_id, parsed.host
                );
            }
        }

        let mux_weak = mux.downgrade();
        let video_pad_slot: PadSlot = Arc::new(Mutex::new(None));
        let audio_pad_slot: PadSlot = Arc::new(Mutex::new(None));

        // Video input. An identity, with the parser inserted and linked once the
        // caps arrive; see the module docs for why nothing is linked statically.
        let video_input_id = format!("{}:rtmp_video_input", instance_id);
        let video_input = gst::ElementFactory::make("identity")
            .name(&video_input_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("video identity: {}", e)))?;

        if let Some(src_pad) = video_input.static_pad("src") {
            let mux_weak_clone = mux_weak.clone();
            let video_slot = video_pad_slot.clone();
            let instance = instance_id.to_string();
            let inserted = Arc::new(AtomicBool::new(false));
            src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                let Some(caps) = caps_from_probe(info) else {
                    return gst::PadProbeReturn::Ok;
                };
                if inserted.swap(true, Ordering::SeqCst) {
                    return gst::PadProbeReturn::Ok;
                }
                let Some((bin, mux)) = bin_and_mux(&mux_weak_clone, &instance) else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(structure) = caps.structure(0) else {
                    error!("RTMP {}: no structure in video caps", instance);
                    return gst::PadProbeReturn::Ok;
                };
                let caps_name = structure.name().to_string();
                debug!("RTMP {}: video caps detected: {}", instance, caps_name);

                let result = video_plan(&caps_name)
                    .and_then(|()| build_video_chain(&bin, &mux, &video_slot, pad, &instance));
                if let Err(e) = result {
                    error!("RTMP {}: {}", instance, e);
                    release_reserved_pad(&video_slot, &mux, "video", &instance);
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Audio input. Raw is encoded here, AAC is parsed only; see the module
        // docs for why the encode lives inside the block.
        let audio_input_id = format!("{}:rtmp_audio_input", instance_id);
        let audio_input = gst::ElementFactory::make("identity")
            .name(&audio_input_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audio identity: {}", e)))?;

        if let Some(src_pad) = audio_input.static_pad("src") {
            let mux_weak_clone = mux_weak.clone();
            let audio_slot = audio_pad_slot.clone();
            let instance = instance_id.to_string();
            let inserted = Arc::new(AtomicBool::new(false));
            src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                let Some(caps) = caps_from_probe(info) else {
                    return gst::PadProbeReturn::Ok;
                };
                if inserted.swap(true, Ordering::SeqCst) {
                    return gst::PadProbeReturn::Ok;
                }
                let Some((bin, mux)) = bin_and_mux(&mux_weak_clone, &instance) else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(structure) = caps.structure(0) else {
                    error!("RTMP {}: no structure in audio caps", instance);
                    return gst::PadProbeReturn::Ok;
                };
                let caps_name = structure.name().to_string();
                debug!("RTMP {}: audio caps detected: {}", instance, caps_name);

                let mpegversion = structure.get::<i32>("mpegversion").unwrap_or(4);
                let layer = structure.get::<i32>("layer").unwrap_or(3);
                let result =
                    audio_plan(&caps_name, mpegversion, layer).and_then(|plan| match plan {
                        AudioPlan::Encode => {
                            build_raw_audio_chain(&bin, &mux, &audio_slot, pad, &instance)
                        }
                        AudioPlan::Parse => {
                            build_aac_audio_chain(&bin, &mux, &audio_slot, pad, &instance)
                        }
                    });
                if let Err(e) = result {
                    error!("RTMP {}: {}", instance, e);
                    release_reserved_pad(&audio_slot, &mux, "audio", &instance);
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Reserve the mux pads before the pipeline starts, for the inputs that are
        // actually connected.
        //
        // This hook runs after construction has linked every block and before the
        // pipeline leaves NULL, which is the only window where both facts hold:
        // connectivity is known, and `flvmux` has not yet written the FLV header.
        // See `take_mux_pad` for what goes wrong if a pad is requested later, and
        // `recorder.rs` for the same reasoning applied to `splitmuxsink`.
        //
        // An unconnected input gets no pad, which is what keeps a video-only or
        // audio-only flow working: an aggregator sink pad that never carries data
        // stops the muxer aggregating altogether.
        let mux_for_setup = mux.downgrade();
        let video_input_weak = video_input.downgrade();
        let audio_input_weak = audio_input.downgrade();
        let video_slot_for_setup = video_pad_slot.clone();
        let audio_slot_for_setup = audio_pad_slot.clone();
        let instance_for_setup = instance_id.to_string();
        ctx.register_element_setup(Box::new(move |_flow_id, _events| {
            let Some(mux) = mux_for_setup.upgrade() else {
                return;
            };
            for (input, slot, name) in [
                (&video_input_weak, &video_slot_for_setup, "video"),
                (&audio_input_weak, &audio_slot_for_setup, "audio"),
            ] {
                if !input_is_connected(input) {
                    debug!(
                        "RTMP {}: {} input is not connected, reserving no pad",
                        instance_for_setup, name
                    );
                    continue;
                }
                match mux.request_pad_simple(name) {
                    Some(pad) => {
                        if let Ok(mut guard) = slot.lock() {
                            *guard = Some(pad);
                        }
                        debug!(
                            "RTMP {}: reserved the '{}' pad before the pipeline started",
                            instance_for_setup, name
                        );
                    }
                    None => error!(
                        "RTMP {}: flvmux refused a '{}' pad",
                        instance_for_setup, name
                    ),
                }
            }
        }));

        // Only the mux to sink link is static. Both mux sink pads are reserved by
        // the setup hook above and linked from inside the probes, once their input
        // has proved it has data.
        let internal_links = vec![(
            ElementPadRef::pad(&mux_id, "src"),
            ElementPadRef::pad(&sink_id, "sink"),
        )];

        Ok(BlockBuildResult {
            elements: vec![
                (video_input_id, video_input),
                (audio_input_id, audio_input),
                (mux_id, mux),
                (sink_id, sink),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// What this block does with an audio stream, once its caps are known.
#[derive(Debug, PartialEq, Eq)]
pub enum AudioPlan {
    /// Raw input: encode to AAC inside the block. See the module docs for why.
    Encode,
    /// Already AAC: parse only, so the encoder is not run twice.
    Parse,
}

/// Decide what to do with video caps, or refuse with the operator-facing reason.
///
/// Split out from the pad probe so the decision can be tested without a
/// pipeline. The wiring it leads to still needs a running flow.
pub fn video_plan(caps_name: &str) -> Result<(), String> {
    match caps_name {
        "video/x-h264" => Ok(()),
        "video/x-raw" => Err(
            "RTMP Output needs H.264 video, but its video input is raw. \
                              Place a builtin.videoenc block before it and link its \
                              encoded_out pad"
                .to_string(),
        ),
        other => Err(format!(
            "RTMP Output needs H.264 video, but its video input carries {}. \
             FLV cannot carry that codec",
            other
        )),
    }
}

/// Decide what to do with audio caps, or refuse with the operator-facing reason.
///
/// `mpegversion` and `layer` are only read for `audio/mpeg`, and both are
/// optional on the wire, so callers pass the value to assume when absent rather than an
/// `Option`. FLV does carry MPEG-1 layer 3 at 5512, 8000, 11025, 22050 and
/// 44100 Hz, per `flvmux`'s own sink caps, but this block does not implement
/// that path: it would need a rate check and a resample, and every producer we
/// care about emits AAC or raw.
pub fn audio_plan(caps_name: &str, mpegversion: i32, layer: i32) -> Result<AudioPlan, String> {
    match caps_name {
        "audio/x-raw" => Ok(AudioPlan::Encode),
        "audio/mpeg" if mpegversion == 2 || mpegversion == 4 => Ok(AudioPlan::Parse),
        "audio/mpeg" if mpegversion == 1 && layer == 3 => Err(
            "RTMP Output needs AAC or raw audio, but its audio input carries MP3. \
                 FLV can carry MP3 and this block does not implement that path"
                .to_string(),
        ),
        "audio/mpeg" => Err(format!(
            "RTMP Output needs AAC or raw audio, but its audio input carries MPEG-{} \
             audio layer {}, which FLV cannot carry",
            mpegversion, layer
        )),
        other => Err(format!(
            "RTMP Output needs AAC or raw audio, but its audio input carries {}",
            other
        )),
    }
}

/// The caps carried by a downstream CAPS event, or `None` for any other event.
fn caps_from_probe(info: &gst::PadProbeInfo) -> Option<gst::Caps> {
    let Some(gst::PadProbeData::Event(event)) = &info.data else {
        return None;
    };
    if event.type_() != gst::EventType::Caps {
        return None;
    }
    match event.view() {
        gst::EventView::Caps(caps_event) => Some(caps_event.caps().to_owned()),
        _ => None,
    }
}

/// The pipeline bin and the muxer, or `None` once either has gone away.
fn bin_and_mux(
    mux_weak: &gst::glib::WeakRef<gst::Element>,
    instance_id: &str,
) -> Option<(gst::Bin, gst::Element)> {
    let mux = mux_weak.upgrade()?;
    let Some(parent) = mux.parent() else {
        error!("RTMP {}: mux has no parent", instance_id);
        return None;
    };
    match parent.downcast::<gst::Bin>() {
        Ok(bin) => Some((bin, mux)),
        Err(_) => {
            error!("RTMP {}: mux parent is not a Bin", instance_id);
            None
        }
    }
}

/// Give back a pad reserved for a stream that turned out unusable.
///
/// Reserving up front is what gets the FLV header right, but it means a
/// connected input whose codec is then refused holds a pad that will never carry
/// data, and `flvmux` is an aggregator, so that would stop it aggregating and
/// take the other stream down too. A refusal therefore has to hand the pad back.
/// That is what keeps "raw video is refused" from also killing the audio.
fn release_reserved_pad(slot: &PadSlot, mux: &gst::Element, name: &str, instance: &str) {
    let reserved = slot.lock().ok().and_then(|mut guard| guard.take());
    if let Some(pad) = reserved {
        mux.release_request_pad(&pad);
        debug!(
            "RTMP {}: released the reserved '{}' pad after a refusal, so the muxer \
             can still aggregate the other stream",
            instance, name
        );
    }
}

/// Whether an input identity has something linked to its sink pad.
///
/// Runs after construction's linking pass, so this is exact for links resolved
/// there. Same helper and same caveat as `recorder.rs`: a link deferred to
/// `pending_links` would resolve after this and read as unconnected.
fn input_is_connected(input: &gst::glib::WeakRef<gst::Element>) -> bool {
    input
        .upgrade()
        .and_then(|e| e.static_pad("sink"))
        .map(|p| p.is_linked())
        .unwrap_or(false)
}

/// Request one of flvmux's fixed-name sink pads.
///
/// `flvmux` names them `video` and `audio` rather than following a `%u`
/// template, so they are requested by name.
fn request_mux_pad(mux: &gst::Element, name: &str) -> Result<gst::Pad, String> {
    mux.request_pad_simple(name)
        .ok_or_else(|| format!("flvmux refused a '{}' pad", name))
}

/// Video: `h264parse` into the muxer.
///
/// `config-interval=-1` repeats SPS and PPS on every keyframe, which is what
/// lets a viewer who joins mid-stream decode at all. Without it the stream looks
/// broken to every late joiner and the fault reads as a network problem.
fn build_video_chain(
    bin: &gst::Bin,
    mux: &gst::Element,
    pad_slot: &PadSlot,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
) -> Result<(), String> {
    let parser_name = format!("{}:rtmp_h264parse", instance_id);
    let parser = gst::ElementFactory::make("h264parse")
        .name(&parser_name)
        .property("config-interval", -1i32)
        .build()
        .map_err(|e| format!("h264parse: {}", e))?;

    bin.add(&parser).map_err(|e| format!("add parser: {}", e))?;
    parser
        .sync_state_with_parent()
        .map_err(|e| format!("sync parser: {}", e))?;

    let parser_sink = parser.static_pad("sink").ok_or("parser has no sink pad")?;
    let parser_src = parser.static_pad("src").ok_or("parser has no src pad")?;
    let mux_sink = take_mux_pad(pad_slot, mux, "video", instance_id)?;

    // Every path that gives up below must hand the pad back. `flvmux` is a
    // `GstAggregator`, so a requested sink pad that never carries data stops it
    // aggregating and nothing reaches the sink, which takes the other media type
    // down with it. Same invariant `recorder.rs` states for `splitmuxsink`.
    let link = || -> Result<(), String> {
        identity_src_pad
            .link(&parser_sink)
            .map_err(|e| format!("link identity -> h264parse: {:?}", e))?;
        parser_src
            .link(&mux_sink)
            .map_err(|e| format!("link h264parse -> flvmux: {:?}", e))?;
        Ok(())
    };
    if let Err(e) = link() {
        mux.release_request_pad(&mux_sink);
        return Err(e);
    }

    info!(
        "RTMP {}: video chain linked: identity -> h264parse -> flvmux ({})",
        instance_id,
        mux_sink.name()
    );
    Ok(())
}

/// Audio, raw in: convert, resample, encode to AAC, parse, into the muxer.
fn build_raw_audio_chain(
    bin: &gst::Bin,
    mux: &gst::Element,
    pad_slot: &PadSlot,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
) -> Result<(), String> {
    let convert_name = format!("{}:rtmp_audio_convert", instance_id);
    let resample_name = format!("{}:rtmp_audio_resample", instance_id);
    let encoder_name = format!("{}:rtmp_audio_encoder", instance_id);
    let parser_name = format!("{}:rtmp_aacparse", instance_id);

    let convert = gst::ElementFactory::make("audioconvert")
        .name(&convert_name)
        .build()
        .map_err(|e| format!("audioconvert: {}", e))?;
    let resample = gst::ElementFactory::make("audioresample")
        .name(&resample_name)
        .build()
        .map_err(|e| format!("audioresample: {}", e))?;
    let encoder = gst::ElementFactory::make("avenc_aac")
        .name(&encoder_name)
        .build()
        .map_err(|e| {
            format!(
                "avenc_aac could not be created ({}). If it is missing, install: {}",
                e,
                libav_package_hint()
            )
        })?;
    let parser = gst::ElementFactory::make("aacparse")
        .name(&parser_name)
        .build()
        .map_err(|e| format!("aacparse: {}", e))?;

    bin.add_many([&convert, &resample, &encoder, &parser])
        .map_err(|e| format!("add elements: {}", e))?;
    for element in [&convert, &resample, &encoder, &parser] {
        element
            .sync_state_with_parent()
            .map_err(|e| format!("sync {}: {}", element.name(), e))?;
    }

    let convert_sink = convert
        .static_pad("sink")
        .ok_or("audioconvert has no sink pad")?;
    let parser_src = parser.static_pad("src").ok_or("parser has no src pad")?;
    let mux_sink = take_mux_pad(pad_slot, mux, "audio", instance_id)?;

    // Every path that gives up below must hand the pad back. `flvmux` is a
    // `GstAggregator`, so a requested sink pad that never carries data stops it
    // aggregating and nothing reaches the sink, which takes the other media type
    // down with it. Same invariant `recorder.rs` states for `splitmuxsink`.
    let link = || -> Result<(), String> {
        identity_src_pad
            .link(&convert_sink)
            .map_err(|e| format!("link identity -> audioconvert: {:?}", e))?;
        convert
            .link(&resample)
            .map_err(|e| format!("link audioconvert -> audioresample: {}", e))?;
        resample
            .link(&encoder)
            .map_err(|e| format!("link audioresample -> avenc_aac: {}", e))?;
        encoder
            .link(&parser)
            .map_err(|e| format!("link avenc_aac -> aacparse: {}", e))?;
        parser_src
            .link(&mux_sink)
            .map_err(|e| format!("link aacparse -> flvmux: {:?}", e))?;
        Ok(())
    };
    if let Err(e) = link() {
        mux.release_request_pad(&mux_sink);
        return Err(e);
    }

    info!(
        "RTMP {}: audio chain linked: identity -> audioconvert -> audioresample -> \
         avenc_aac -> aacparse -> flvmux ({})",
        instance_id,
        mux_sink.name()
    );
    Ok(())
}

/// Audio, AAC in: parse only, into the muxer.
fn build_aac_audio_chain(
    bin: &gst::Bin,
    mux: &gst::Element,
    pad_slot: &PadSlot,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
) -> Result<(), String> {
    let parser_name = format!("{}:rtmp_aacparse", instance_id);
    let parser = gst::ElementFactory::make("aacparse")
        .name(&parser_name)
        .build()
        .map_err(|e| format!("aacparse: {}", e))?;

    bin.add(&parser).map_err(|e| format!("add parser: {}", e))?;
    parser
        .sync_state_with_parent()
        .map_err(|e| format!("sync parser: {}", e))?;

    let parser_sink = parser.static_pad("sink").ok_or("parser has no sink pad")?;
    let parser_src = parser.static_pad("src").ok_or("parser has no src pad")?;
    let mux_sink = take_mux_pad(pad_slot, mux, "audio", instance_id)?;

    // Every path that gives up below must hand the pad back. `flvmux` is a
    // `GstAggregator`, so a requested sink pad that never carries data stops it
    // aggregating and nothing reaches the sink, which takes the other media type
    // down with it. Same invariant `recorder.rs` states for `splitmuxsink`.
    let link = || -> Result<(), String> {
        identity_src_pad
            .link(&parser_sink)
            .map_err(|e| format!("link identity -> aacparse: {:?}", e))?;
        parser_src
            .link(&mux_sink)
            .map_err(|e| format!("link aacparse -> flvmux: {:?}", e))?;
        Ok(())
    };
    if let Err(e) = link() {
        mux.release_request_pad(&mux_sink);
        return Err(e);
    }

    info!(
        "RTMP {}: audio chain linked: identity -> aacparse -> flvmux ({})",
        instance_id,
        mux_sink.name()
    );
    Ok(())
}

/// Get metadata for RTMP blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![rtmp_output_definition()]
}

/// Get RTMP Output block definition (metadata only).
fn rtmp_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.rtmp_output".to_string(),
        name: "RTMP Output".to_string(),
        description: "Publish a programme to an RTMP server, muxed as FLV. Takes H.264 \
                      video, so place a Video Encoder before it; audio may be raw or AAC \
                      and is encoded here when raw."
            .to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "location".to_string(),
                label: "RTMP URL".to_string(),
                description: "Where to publish, for example rtmp://host:1935/live/streamkey. \
                              Use rtmps:// to encrypt the connection; the server's certificate \
                              is then validated and a self-signed one is refused. Credentials \
                              may be given as rtmps://user:password@host/live/streamkey; they are \
                              kept out of the logs, but this whole URL is stored with the flow, \
                              so treat it as a secret at rest."
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(DEFAULT_RTMP_LOCATION.to_string())),
                mapping: PropertyMapping {
                    element_id: "rtmp_sink".to_string(),
                    property_name: "location".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "sync".to_string(),
                label: "Synchronise to clock".to_string(),
                description: "Pace output against the pipeline clock. Turn off when the input \
                              carries a remote encoder's timestamps, which otherwise makes the \
                              sink believe it is behind and drop frames."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "rtmp_sink".to_string(),
                    property_name: "sync".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("V".to_string()),
                    name: "video_in".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "rtmp_video_input".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    label: Some("A".to_string()),
                    name: "audio_in".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "rtmp_audio_input".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📡".to_string()),
            width: Some(1.5),
            height: Some(2.0),
            ..Default::default()
        }),
    }
}
