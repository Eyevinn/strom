//! SDP transformation functions for WHIP ingest.
//!
//! Each function is a standalone, pure transformation that can be applied
//! (or skipped) independently. This makes it easy to test which transforms
//! are actually needed by commenting out individual calls in `whip_ingest.rs`.

use strom_types::whip::{
    DEFAULT_MAX_VIDEO_BITRATE_KBPS, DEFAULT_MIN_VIDEO_BITRATE_KBPS,
    DEFAULT_START_VIDEO_BITRATE_KBPS,
};
use tracing::info;

/// Add `goog-remb` RTCP feedback to video codecs in the SDP answer.
///
/// REMB (Receiver Estimated Maximum Bitrate) is a fallback bandwidth
/// estimation mechanism. The primary mechanism is transport-cc (TWCC),
/// but adding REMB gives Chrome an additional signal for bandwidth
/// estimation if TWCC feedback is delayed.
///
/// Uses targeted string insertion to preserve original SDP bytes and
/// line endings exactly as webrtcbin produced them.
pub(crate) fn add_goog_remb(sdp: &str) -> String {
    // Only operate within the video section
    let video_start = match sdp.find("m=video") {
        Some(pos) => pos,
        None => return sdp.to_string(),
    };

    let video_section = &sdp[video_start..];

    // Find "transport-cc" in the video section (as part of a=rtcp-fb line)
    let tc_needle = " transport-cc";
    let tc_rel = match video_section.find(tc_needle) {
        Some(pos) => pos,
        None => return sdp.to_string(),
    };

    // Walk backwards to find start of this a=rtcp-fb line
    let line_start_rel = video_section[..tc_rel]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let line_content = &video_section[line_start_rel..tc_rel + tc_needle.len()];

    // Extract PT number from the line
    let pt = match line_content.strip_prefix("a=rtcp-fb:") {
        Some(rest) => rest.split(' ').next().unwrap_or(""),
        None => return sdp.to_string(),
    };

    let remb_line = format!("a=rtcp-fb:{} goog-remb", pt);
    if sdp.contains(&remb_line) {
        return sdp.to_string();
    }

    // Find the end of the transport-cc line (the next \n)
    let abs_tc_end = video_start + tc_rel + tc_needle.len();
    let newline_pos = match sdp[abs_tc_end..].find('\n') {
        Some(pos) => abs_tc_end + pos + 1, // position after the \n
        None => return sdp.to_string(),
    };

    // Detect line ending style from the original SDP
    let line_ending = if newline_pos >= 2 && sdp.as_bytes().get(newline_pos - 2) == Some(&b'\r') {
        "\r\n"
    } else {
        "\n"
    };

    // Insert goog-remb line right after the transport-cc line
    let mut result = String::with_capacity(sdp.len() + remb_line.len() + 2);
    result.push_str(&sdp[..newline_pos]);
    result.push_str(&remb_line);
    result.push_str(line_ending);
    result.push_str(&sdp[newline_pos..]);

    info!("WHIP: Added {} to SDP answer", remb_line);
    result
}

/// Move x-google bitrate hints into the video codec's `a=fmtp:` line.
///
/// webrtcbin echoes x-google-{min,start,max}-bitrate from the offer but
/// places them as standalone `a=x-google-*:` SDP attributes instead of
/// keeping them in the `a=fmtp:` line. Chrome only processes these hints
/// when they appear inside the fmtp parameters, so without this fix the
/// browser starts at its default ~300 kbps instead of the intended 2 Mbps.
///
/// This function:
/// 1. Reads the values from standalone `a=x-google-*:` lines (or uses defaults)
/// 2. Appends them to the video codec fmtp line
/// 3. Removes the standalone lines
pub(crate) fn fix_video_bitrate_hints(sdp: &str, max_bitrate_kbps: Option<u32>) -> String {
    // Extract existing standalone x-google values
    let mut min_bitrate: Option<&str> = None;
    let mut start_bitrate: Option<&str> = None;
    let mut max_bitrate: Option<&str> = None;

    for line in sdp.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("a=x-google-min-bitrate:") {
            min_bitrate = Some(val.trim());
        } else if let Some(val) = trimmed.strip_prefix("a=x-google-start-bitrate:") {
            start_bitrate = Some(val.trim());
        } else if let Some(val) = trimmed.strip_prefix("a=x-google-max-bitrate:") {
            max_bitrate = Some(val.trim());
        }
    }

    // Use defaults if standalone lines weren't present.
    // max_bitrate_kbps overrides the default max (but not an explicit value from the SDP).
    let default_max = max_bitrate_kbps
        .map(|v| v.to_string())
        .unwrap_or_else(|| DEFAULT_MAX_VIDEO_BITRATE_KBPS.to_string());
    let default_min = DEFAULT_MIN_VIDEO_BITRATE_KBPS.to_string();
    let default_start = DEFAULT_START_VIDEO_BITRATE_KBPS.to_string();
    let max_val: u32 = max_bitrate
        .unwrap_or(&default_max)
        .parse()
        .unwrap_or(DEFAULT_MAX_VIDEO_BITRATE_KBPS);
    let min_val: u32 = min_bitrate
        .unwrap_or(&default_min)
        .parse()
        .unwrap_or(DEFAULT_MIN_VIDEO_BITRATE_KBPS)
        .min(max_val);
    let start_val: u32 = start_bitrate
        .unwrap_or(&default_start)
        .parse()
        .unwrap_or(DEFAULT_START_VIDEO_BITRATE_KBPS)
        .clamp(min_val, max_val);

    let hints = format!(
        ";x-google-min-bitrate={};x-google-start-bitrate={};x-google-max-bitrate={}",
        min_val, start_val, max_val
    );

    // Find the video section and its fmtp line
    let video_start = match sdp.find("m=video") {
        Some(pos) => pos,
        None => return sdp.to_string(),
    };

    // Find the fmtp line in the video section. We need the first a=fmtp:
    // line after m=video that isn't for a meta-codec (RTX/RED/ULPFEC).
    // Since we already stripped those from the offer, the answer should
    // only have the primary video codec's fmtp line.
    let video_section = &sdp[video_start..];
    let fmtp_needle = "a=fmtp:";
    let fmtp_rel = match video_section.find(fmtp_needle) {
        Some(pos) => pos,
        None => return sdp.to_string(),
    };

    // Find the end of this fmtp line
    let fmtp_abs = video_start + fmtp_rel;
    let fmtp_line_end = sdp[fmtp_abs..]
        .find('\n')
        .map(|p| fmtp_abs + p)
        .unwrap_or(sdp.len());

    // Check if hints are already present
    let fmtp_line = &sdp[fmtp_abs..fmtp_line_end];
    if fmtp_line.contains("x-google-start-bitrate") {
        // Already has hints, just remove standalone lines
        return remove_standalone_x_google(sdp);
    }

    // Insert hints at the end of the fmtp line (before any \r\n)
    let insert_pos = if fmtp_line_end > 0 && sdp.as_bytes().get(fmtp_line_end - 1) == Some(&b'\r') {
        fmtp_line_end - 1 // insert before \r
    } else {
        fmtp_line_end // insert before \n
    };

    let mut result = String::with_capacity(sdp.len() + hints.len());
    result.push_str(&sdp[..insert_pos]);
    result.push_str(&hints);
    result.push_str(&sdp[insert_pos..]);

    info!(
        "WHIP: Added x-google bitrate hints to fmtp line (min={}, start={}, max={})",
        min_val, start_val, max_val
    );

    // Remove standalone a=x-google-* lines
    remove_standalone_x_google(&result)
}

/// Remove standalone `a=x-google-*:` lines from SDP.
fn remove_standalone_x_google(sdp: &str) -> String {
    // Use targeted removal to preserve original SDP bytes
    let mut result = String::with_capacity(sdp.len());
    let mut pos = 0;

    while pos < sdp.len() {
        let remaining = &sdp[pos..];
        let line_end = remaining
            .find('\n')
            .map(|p| p + 1)
            .unwrap_or(remaining.len());
        let line = &remaining[..line_end];
        let trimmed = line.trim();

        if trimmed.starts_with("a=x-google-min-bitrate:")
            || trimmed.starts_with("a=x-google-start-bitrate:")
            || trimmed.starts_with("a=x-google-max-bitrate:")
        {
            // Skip this line
            pos += line_end;
            continue;
        }

        result.push_str(line);
        pos += line_end;
    }

    result
}

/// Strip `urn:3gpp:video-orientation` (CVO) extmap from an SDP answer.
///
/// GStreamer's webrtcbin has no built-in handler for the CVO RTP header
/// extension. If the extension is negotiated, mobile browsers (Safari iOS,
/// Chrome Android) send unrotated video frames and signal orientation via
/// the RTP header extension — which GStreamer silently ignores, resulting
/// in video stuck in landscape.
///
/// By stripping the extension from the answer, the browser falls back to
/// rotating the video pixels in the encoder before sending, so the receiver
/// gets correctly oriented frames without needing CVO support.
pub(crate) fn strip_cvo_extension(sdp: &str) -> String {
    let mut result = String::with_capacity(sdp.len());
    let mut stripped = false;
    let mut pos = 0;

    while pos < sdp.len() {
        let remaining = &sdp[pos..];
        let line_end = remaining
            .find('\n')
            .map(|p| p + 1)
            .unwrap_or(remaining.len());
        let line = &remaining[..line_end];
        let trimmed = line.trim();

        if trimmed.contains("urn:3gpp:video-orientation") {
            stripped = true;
            pos += line_end;
            continue;
        }

        result.push_str(line);
        pos += line_end;
    }

    if stripped {
        info!("WHIP: Stripped urn:3gpp:video-orientation from SDP answer (forcing browser pixel rotation)");
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // add_goog_remb tests
    // ========================================================================

    #[test]
    fn add_goog_remb_inserts_after_transport_cc() {
        let sdp = "\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=rtcp-fb:96 transport-cc\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n";

        let result = add_goog_remb(sdp);

        assert!(result.contains("a=rtcp-fb:96 goog-remb\r\n"));
        // Should be right after transport-cc line
        let tc_pos = result.find("a=rtcp-fb:96 transport-cc").unwrap();
        let remb_pos = result.find("a=rtcp-fb:96 goog-remb").unwrap();
        let fmtp_pos = result.find("a=fmtp:96").unwrap();
        assert!(remb_pos > tc_pos);
        assert!(remb_pos < fmtp_pos);
    }

    #[test]
    fn add_goog_remb_noop_without_video() {
        let sdp = "\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtcp-fb:111 transport-cc\r\n";

        let result = add_goog_remb(sdp);
        assert_eq!(result, sdp);
    }

    #[test]
    fn add_goog_remb_noop_if_already_present() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=rtcp-fb:96 transport-cc\r\n\
a=rtcp-fb:96 goog-remb\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n";

        let result = add_goog_remb(sdp);
        assert_eq!(result, sdp);
    }

    #[test]
    fn add_goog_remb_preserves_crlf() {
        let sdp = "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtcp-fb:96 transport-cc\r\n\
a=fmtp:96 foo\r\n";

        let result = add_goog_remb(sdp);
        // Every line should end with \r\n
        for line in result.split('\n') {
            if !line.is_empty() {
                assert!(line.ends_with('\r'), "Line missing \\r: {:?}", line);
            }
        }
    }

    // ========================================================================
    // fix_video_bitrate_hints tests
    // ========================================================================

    #[test]
    fn fix_video_bitrate_hints_moves_standalone_to_fmtp() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n\
a=x-google-min-bitrate:1500\r\n\
a=x-google-start-bitrate:1500\r\n\
a=x-google-max-bitrate:3000\r\n";

        let result = fix_video_bitrate_hints(sdp, None);

        // Standalone lines should be removed
        assert!(!result.contains("a=x-google-min-bitrate:"));
        assert!(!result.contains("a=x-google-start-bitrate:"));
        assert!(!result.contains("a=x-google-max-bitrate:"));

        // Values should be in the fmtp line
        assert!(result.contains("x-google-min-bitrate=1500"));
        assert!(result.contains("x-google-start-bitrate=1500"));
        assert!(result.contains("x-google-max-bitrate=3000"));
        // Original fmtp content preserved
        assert!(result.contains("level-asymmetry-allowed=1"));
    }

    #[test]
    fn fix_video_bitrate_hints_applies_defaults_without_standalone() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n";

        let result = fix_video_bitrate_hints(sdp, None);

        assert!(result.contains(&format!(
            "x-google-min-bitrate={}",
            DEFAULT_MIN_VIDEO_BITRATE_KBPS
        )));
        assert!(result.contains(&format!(
            "x-google-start-bitrate={}",
            DEFAULT_START_VIDEO_BITRATE_KBPS
        )));
        assert!(result.contains(&format!(
            "x-google-max-bitrate={}",
            DEFAULT_MAX_VIDEO_BITRATE_KBPS
        )));
    }

    #[test]
    fn fix_video_bitrate_hints_noop_if_already_in_fmtp() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;x-google-start-bitrate=2000;x-google-min-bitrate=2000;x-google-max-bitrate=4000\r\n";

        let result = fix_video_bitrate_hints(sdp, None);
        assert_eq!(result, sdp);
    }

    #[test]
    fn fix_video_bitrate_hints_removes_standalone_when_fmtp_has_hints() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=fmtp:96 level-asymmetry-allowed=1;x-google-start-bitrate=2000\r\n\
a=x-google-min-bitrate:1000\r\n";

        let result = fix_video_bitrate_hints(sdp, None);

        // Standalone line removed
        assert!(!result.contains("a=x-google-min-bitrate:"));
        // Existing fmtp hints preserved
        assert!(result.contains("x-google-start-bitrate=2000"));
    }

    #[test]
    fn fix_video_bitrate_hints_noop_without_video() {
        let sdp = "\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 minptime=10\r\n";

        let result = fix_video_bitrate_hints(sdp, None);
        assert_eq!(result, sdp);
    }

    // ========================================================================
    // strip_cvo_extension tests
    // ========================================================================

    #[test]
    fn strip_cvo_extension_removes_video_orientation_extmap() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
a=extmap:4 urn:3gpp:video-orientation\r\n\
a=extmap:5 urn:ietf:params:rtp-hdrext:sdes:mid\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n";

        let result = strip_cvo_extension(sdp);

        assert!(!result.contains("3gpp:video-orientation"));
        assert!(result.contains("a=extmap:3"));
        assert!(result.contains("a=extmap:5"));
        assert!(result.contains("a=fmtp:96"));
    }

    #[test]
    fn strip_cvo_extension_noop_without_cvo() {
        let sdp = "\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
a=fmtp:96 level-asymmetry-allowed=1\r\n";

        let result = strip_cvo_extension(sdp);
        assert_eq!(result, sdp);
    }
}
