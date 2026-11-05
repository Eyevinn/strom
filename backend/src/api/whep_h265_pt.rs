//! Keep a browser's low H.265 payload types working through WHEP Output.
//!
//! Chrome and Safari offer H.265 on payload types below 96 (Chrome 49 and 51,
//! with RTX on 50 and 52). `rtph265pay` only produces `payload=[96,127]`, so
//! webrtcsink cannot link its payloader for such a payload type and drops the
//! codec. The WHEP proxy therefore moves those payload types into [96, 127]
//! in the offer before webrtcsink sees it ([`renumber_offer`]).
//!
//! The browser only knows the payload types it offered, so the answer and the
//! RTP it receives must use them too. [`restore_answer`] maps the answer back,
//! and the offer carries the mapping in a session attribute
//! ([`PT_MAP_ATTRIBUTE`]) that WHEP Output reads from webrtcbin's remote
//! description to put the original payload type in each RTP packet.

use std::collections::{HashMap, HashSet};

pub use crate::blocks::builtin::webrtcsink_pt_restore::PT_MAP_ATTRIBUTE;

/// Payload types the proxy moved: `(offered, renumbered)` pairs, sorted.
pub type PtMapping = Vec<(u8, u8)>;

/// Move H.265 payload types below 96, and their RTX payload types, into
/// [96, 127] in a browser's offer. Returns the offer to forward and the
/// pairs moved, empty when nothing was.
///
/// The returned offer names the moves in a session-level
/// `a=x-strom-pt-map:<renumbered>=<offered> ...` attribute.
pub fn renumber_offer(sdp: &str, endpoint_id: &str) -> (String, PtMapping) {
    let mut used_pts: HashSet<u8> = HashSet::new();
    let mut h265_pts: Vec<u8> = Vec::new();
    let mut rtx_for: Vec<(u8, u8)> = Vec::new(); // (rtx_pt, source_pt)
                                                 // One pass over the offer. Payload types in use by any m= section count
                                                 // as taken: the session is bundled, so two sections must not share one.
    let mut in_video = false;
    for line in sdp.lines() {
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video ");
        }
        if let Some((pt, rest)) = pt_prefix(line, "a=rtpmap:") {
            used_pts.insert(pt);
            let codec = rest.trim_start().split('/').next().unwrap_or("");
            if in_video && codec.eq_ignore_ascii_case("H265") {
                h265_pts.push(pt);
            }
        } else if !in_video {
            continue;
        } else if let Some((pt, rest)) = pt_prefix(line, "a=fmtp:") {
            if let Some(source) = apt_value(rest) {
                rtx_for.push((pt, source));
            }
        }
    }

    let mut to_move: Vec<u8> = h265_pts.iter().copied().filter(|pt| *pt < 96).collect();
    if to_move.is_empty() {
        return (sdp.to_string(), Vec::new());
    }
    to_move.sort_unstable();
    let mut rtx_to_move: Vec<u8> = rtx_for
        .iter()
        .filter(|(rtx, source)| to_move.contains(source) && *rtx < 96)
        .map(|(rtx, _)| *rtx)
        .collect();
    rtx_to_move.sort_unstable();

    let mut mapping: PtMapping = Vec::new();
    let mut free = (96..=127u8).filter(|pt| !used_pts.contains(pt));
    for &old in to_move.iter().chain(rtx_to_move.iter()) {
        match free.next() {
            Some(new_pt) => mapping.push((old, new_pt)),
            None => {
                tracing::warn!(
                    "WHEP offer for endpoint '{}': no free payload type in [96,127] for H.265 payload type {}",
                    endpoint_id,
                    old
                );
            }
        }
    }
    // An RTX payload type whose media payload type did not move has to stay
    // where it is, or its apt= points at nothing; the same the other way.
    let moved: HashSet<u8> = mapping.iter().map(|(old, _)| *old).collect();
    mapping.retain(
        |(old, _)| match rtx_for.iter().find(|(rtx, _)| rtx == old) {
            Some((_, source)) => moved.contains(source),
            None => true,
        },
    );
    if mapping.is_empty() {
        return (sdp.to_string(), mapping);
    }
    mapping.sort_unstable();

    let map: HashMap<u8, u8> = mapping.iter().copied().collect();
    let rewritten = rewrite_video_pts(sdp, &map);
    let out = insert_session_attribute(&rewritten, &pt_map_attribute(&mapping));

    tracing::info!(
        "WHEP offer for endpoint '{}': renumbered low H.265 PTs (old->new): {:?}",
        endpoint_id,
        mapping
    );
    (out, mapping)
}

/// Map webrtcsink's answer back to the payload types the browser offered,
/// for the pairs [`renumber_offer`] moved.
pub fn restore_answer(sdp: &str, mapping: &[(u8, u8)]) -> String {
    if mapping.is_empty() {
        return sdp.to_string();
    }
    let back: HashMap<u8, u8> = mapping.iter().map(|(old, new)| (*new, *old)).collect();
    let prefix = format!("a={}:", PT_MAP_ATTRIBUTE);
    let without_attr: String = sdp
        .split_inclusive('\n')
        .filter(|line| !line.starts_with(&prefix))
        .collect();
    rewrite_video_pts(&without_attr, &back)
}

/// `a=x-strom-pt-map:105=49 113=50` for `[(49, 105), (50, 113)]`.
fn pt_map_attribute(mapping: &[(u8, u8)]) -> String {
    let pairs: Vec<String> = mapping
        .iter()
        .map(|(old, new)| format!("{}={}", new, old))
        .collect();
    format!("a={}:{}", PT_MAP_ATTRIBUTE, pairs.join(" "))
}

/// Insert a session-level attribute before the first `m=` line.
fn insert_session_attribute(sdp: &str, attribute: &str) -> String {
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(sdp.len() + attribute.len() + 2);
    let mut inserted = false;
    for line in sdp.split_inclusive('\n') {
        if !inserted && line.starts_with("m=") {
            out.push_str(attribute);
            out.push_str(eol);
            inserted = true;
        }
        out.push_str(line);
    }
    if !inserted {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(attribute);
        out.push_str(eol);
    }
    out
}

/// `Some((pt, rest))` for a line `<prefix><pt><rest>`.
fn pt_prefix<'a>(line: &'a str, prefix: &str) -> Option<(u8, &'a str)> {
    let rest = line.strip_prefix(prefix)?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let pt = rest[..end].parse::<u8>().ok()?;
    Some((pt, &rest[end..]))
}

/// The `apt=` value of an fmtp parameter list.
fn apt_value(params: &str) -> Option<u8> {
    params
        .split(|c: char| c == ';' || c.is_whitespace())
        .find_map(|p| p.trim().strip_prefix("apt="))
        .and_then(|v| v.parse::<u8>().ok())
}

/// Rewrite every payload type reference in the `m=video` sections: the
/// `m=` payload type list, `a=rtpmap`, `a=rtcp-fb`, `a=fmtp` and its `apt=`.
fn rewrite_video_pts(sdp: &str, map: &HashMap<u8, u8>) -> String {
    let map_pt = |pt: u8| map.get(&pt).copied().unwrap_or(pt);
    let mut out = String::with_capacity(sdp.len() + 16);
    let mut in_video = false;
    for line in sdp.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let eol = &line[body.len()..];
        if body.starts_with("m=") {
            in_video = body.starts_with("m=video ");
            if in_video {
                let rewritten: Vec<String> = body
                    .split(' ')
                    .enumerate()
                    .map(|(idx, token)| match token.parse::<u8>() {
                        // <media> <port> <proto> come first.
                        Ok(pt) if idx >= 3 => map_pt(pt).to_string(),
                        _ => token.to_string(),
                    })
                    .collect();
                out.push_str(&rewritten.join(" "));
                out.push_str(eol);
                continue;
            }
        }
        if in_video {
            let mut done = false;
            for prefix in ["a=rtpmap:", "a=rtcp-fb:", "a=fmtp:"] {
                if let Some((pt, rest)) = pt_prefix(body, prefix) {
                    out.push_str(prefix);
                    out.push_str(&map_pt(pt).to_string());
                    if prefix == "a=fmtp:" {
                        out.push_str(&rewrite_apt(rest, &map_pt));
                    } else {
                        out.push_str(rest);
                    }
                    out.push_str(eol);
                    done = true;
                    break;
                }
            }
            if done {
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

/// Rewrite the `apt=` value in an fmtp parameter list.
fn rewrite_apt(params: &str, map_pt: &impl Fn(u8) -> u8) -> String {
    let Some(idx) = params.find("apt=") else {
        return params.to_string();
    };
    let head = &params[..idx + "apt=".len()];
    let tail = &params[idx + "apt=".len()..];
    let end = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    match tail[..end].parse::<u8>() {
        Ok(apt) => format!("{}{}{}", head, map_pt(apt), &tail[end..]),
        Err(_) => params.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The video section of a Chrome offer: VP8 and H.264 on high payload
    /// types, H.265 on 49/51 with RTX on 50/52, AV1 on 45, and an audio
    /// section that must not be touched.
    const CHROME_OFFER: &str = "v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97 45 46 49 50 51 52 103 104\r\n\
a=mid:0\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:97 rtx/90000\r\n\
a=fmtp:97 apt=96\r\n\
a=rtpmap:45 AV1/90000\r\n\
a=fmtp:45 level-idx=5;profile=0;tier=0\r\n\
a=rtpmap:46 rtx/90000\r\n\
a=fmtp:46 apt=45\r\n\
a=rtpmap:49 H265/90000\r\n\
a=rtcp-fb:49 nack\r\n\
a=rtcp-fb:49 nack pli\r\n\
a=fmtp:49 level-id=180;profile-id=1;tier-flag=0;tx-mode=SRST\r\n\
a=rtpmap:50 rtx/90000\r\n\
a=fmtp:50 apt=49\r\n\
a=rtpmap:51 H265/90000\r\n\
a=fmtp:51 level-id=180;profile-id=2;tier-flag=0;tx-mode=SRST\r\n\
a=rtpmap:52 rtx/90000\r\n\
a=fmtp:52 apt=51\r\n\
a=rtpmap:103 H264/90000\r\n\
a=rtpmap:104 rtx/90000\r\n\
a=fmtp:104 apt=103\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111 98\r\n\
a=mid:1\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtpmap:98 telephone-event/48000\r\n";

    #[test]
    fn offer_moves_h265_and_its_rtx_into_the_dynamic_range() {
        let (offer, mapping) = renumber_offer(CHROME_OFFER, "test");
        // Free in order: 99, 100, 101, 102 (96-98, 103, 104 are taken; 98 by
        // the audio section).
        assert_eq!(mapping, vec![(49, 99), (50, 101), (51, 100), (52, 102)]);
        assert!(
            offer.contains("m=video 9 UDP/TLS/RTP/SAVPF 96 97 45 46 99 101 100 102 103 104\r\n")
        );
        assert!(offer.contains("a=rtpmap:99 H265/90000\r\n"));
        assert!(offer.contains("a=rtcp-fb:99 nack pli\r\n"));
        assert!(offer.contains("a=fmtp:99 level-id=180;profile-id=1;"));
        assert!(offer.contains("a=rtpmap:101 rtx/90000\r\na=fmtp:101 apt=99\r\n"));
        assert!(offer.contains("a=fmtp:102 apt=100\r\n"));
        // AV1 on 45 is not H.265: rtpav1pay takes any payload type.
        assert!(offer.contains("a=rtpmap:45 AV1/90000\r\n"));
        assert!(offer.contains("a=fmtp:46 apt=45\r\n"));
        // Audio untouched.
        assert!(offer.contains("m=audio 9 UDP/TLS/RTP/SAVPF 111 98\r\n"));
        // The mapping travels in a session attribute, before the first m=.
        let attr = "a=x-strom-pt-map:99=49 101=50 100=51 102=52\r\nm=video";
        assert!(offer.contains(attr), "{offer}");
    }

    #[test]
    fn answer_is_mapped_back_to_the_offered_payload_types() {
        let (_, mapping) = renumber_offer(CHROME_OFFER, "test");
        // What webrtcsink answers for the renumbered offer: H.265 Main on 99
        // with RTX on 101, and opus.
        let answer = "v=0\r\n\
o=- 3 2 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 99 101\r\n\
a=rtpmap:99 H265/90000\r\n\
a=rtcp-fb:99 nack pli\r\n\
a=fmtp:99 profile-id=1;tier-flag=0;tx-mode=SRST\r\n\
a=rtpmap:101 rtx/90000\r\n\
a=fmtp:101 apt=99\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111 99\r\n\
a=rtpmap:111 OPUS/48000/2\r\n\
a=rtpmap:99 telephone-event/48000\r\n";
        let restored = restore_answer(answer, &mapping);
        assert!(restored.contains("m=video 9 UDP/TLS/RTP/SAVPF 49 50\r\n"));
        assert!(restored.contains("a=rtpmap:49 H265/90000\r\n"));
        assert!(restored.contains("a=rtcp-fb:49 nack pli\r\n"));
        assert!(restored.contains("a=fmtp:49 profile-id=1;"));
        assert!(restored.contains("a=rtpmap:50 rtx/90000\r\na=fmtp:50 apt=49\r\n"));
        // A payload type the audio section uses is not video's to map.
        assert!(restored.contains("m=audio 9 UDP/TLS/RTP/SAVPF 111 99\r\n"));
        assert!(restored.contains("a=rtpmap:99 telephone-event/48000\r\n"));
    }

    #[test]
    fn round_trip_restores_the_offer_payload_types() {
        let (offer, mapping) = renumber_offer(CHROME_OFFER, "test");
        let back = restore_answer(&offer, &mapping);
        assert_eq!(back, CHROME_OFFER);
    }

    #[test]
    fn offer_without_low_h265_is_unchanged() {
        let sdp = "v=0\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96 45\r\n\
a=rtpmap:96 H265/90000\r\na=rtpmap:45 AV1/90000\r\n";
        let (offer, mapping) = renumber_offer(sdp, "test");
        assert!(mapping.is_empty());
        assert_eq!(offer, sdp);
        assert_eq!(restore_answer(sdp, &mapping), sdp);
    }

    #[test]
    fn rtx_whose_media_could_not_move_stays() {
        // [96, 127] has one free slot (127): H.265 moves, its RTX cannot. The
        // RTX stays on 50 (webrtcsink only links the media payloader) and its
        // apt= follows the media to 127.
        let mut sdp = String::from("v=0\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 49 50\r\n");
        for pt in 96..127u8 {
            sdp.push_str(&format!("a=rtpmap:{pt} VP8/90000\r\n"));
        }
        sdp.push_str("a=rtpmap:49 H265/90000\r\na=rtpmap:50 rtx/90000\r\na=fmtp:50 apt=49\r\n");
        let (offer, mapping) = renumber_offer(&sdp, "test");
        assert_eq!(mapping, vec![(49, 127)]);
        assert!(offer.contains("a=fmtp:50 apt=127\r\n"));
    }
}
