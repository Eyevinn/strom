//! Live channels offered in the playlist editor next to the media folder.
//!
//! Publicly reachable news streams from the broadcasters' own CDNs, for
//! testing HLS and DASH input. Public does not mean licensed for
//! redistribution, and a stream may be geo-blocked or move without notice.

/// A live channel: the name shown in the list and the URL the player opens.
pub struct LiveChannel {
    pub name: &'static str,
    pub url: &'static str,
}

pub const LIVE_CHANNELS: &[LiveChannel] = &[
    LiveChannel {
        name: "Al Jazeera English",
        url: "https://live-hls-apps-aje-fa.getaj.net/AJE/index.m3u8",
    },
    LiveChannel {
        name: "Al Jazeera Arabic",
        url: "https://live-hls-apps-aja-fa.getaj.net/AJA/01.m3u8",
    },
    LiveChannel {
        name: "Al Jazeera Mubasher",
        url: "https://live-hls-apps-ajm-fa.getaj.net/AJM/index.m3u8",
    },
    LiveChannel {
        name: "BBC News (DASH, H.264)",
        url: "https://vs-cmaf-push-ww-live.akamaized.net/x=4/i=urn:bbc:pips:service:bbc_news_channel_hd/pc_hd_abr_v2.mpd",
    },
    LiveChannel {
        name: "BBC News (DASH, HEVC)",
        url: "https://vs-cmaf-push-ww-live.akamaized.net/x=4/i=urn:bbc:pips:service:bbc_news_channel_hd/hevc_iptv_mse_v0.mpd",
    },
    LiveChannel {
        name: "BBC Arabic (DASH, H.264)",
        url: "https://vs-cmaf-pushb-ww-live.akamaized.net/x=4/i=urn:bbc:pips:service:bbc_arabic_tv/pc_hd_abr_v2.mpd",
    },
    LiveChannel {
        name: "BBC Persian (DASH, HEVC)",
        url: "https://vs-cmaf-pushb-ww-live.akamaized.net/x=4/i=urn:bbc:pips:service:bbc_persian_tv/hevc_iptv_mse_v0.mpd",
    },
    LiveChannel {
        name: "France 24 English",
        url: "https://live.france24.com/hls/live/2037218-b/F24_EN_HI_HLS/master_5000.m3u8",
    },
    LiveChannel {
        name: "France 24 Français",
        url: "https://live.france24.com/hls/live/2037179-b/F24_FR_HI_HLS/master_5000.m3u8",
    },
    LiveChannel {
        name: "DW English",
        url: "https://amg01644-amg01644c1-amgplt0343.playout.now3.amagi.tv/ts-eu-w1-n2/playlist/amg01644-amg01644c1-amgplt0343/playlist.m3u8",
    },
    LiveChannel {
        name: "DW Arabic",
        url: "https://dwamdstream103.akamaized.net/hls/live/2015526/dwstream103/master.m3u8",
    },
    LiveChannel {
        name: "DW Español",
        url: "https://dwamdstream104.akamaized.net/hls/live/2015530/dwstream104/master.m3u8",
    },
    LiveChannel {
        name: "NHK World-Japan",
        url: "https://masterpl.hls.nhkworld.jp/hls/w/live/smarttv.m3u8",
    },
    LiveChannel {
        name: "Reuters",
        url: "https://amg00453-reuters-amg00453c1-rakuten-uk-2110.playouts.now.amagi.tv/playlist/amg00453-reuters-reuters-rakutenuk/playlist.m3u8",
    },
    LiveChannel {
        name: "CGTN",
        url: "https://amg00405-rakutentv-cgtn-rakuten-i9tar.amagi.tv/master.m3u8",
    },
    LiveChannel {
        name: "i24NEWS English",
        url: "https://i24newsenglish-cdn.encoders.immergo.tv/master.m3u8",
    },
    LiveChannel {
        name: "Arirang UN",
        url: "https://amdlive-ch02-ctnd-com.akamaized.net/arirang_2ch/smil:arirang_2ch.smil/playlist.m3u8",
    },
    LiveChannel {
        name: "ABC News (Australia)",
        url: "https://abc-news-dmd-streams-1.akamaized.net/out/v1/701126012d044971b3fa89406a440133/index.m3u8",
    },
    LiveChannel {
        name: "ABC News Live (US)",
        url: "https://abcnews-streams.akamaized.net/hls/live/2023560/abcnewshudson1/master.m3u8",
    },
    LiveChannel {
        name: "CBS News New York",
        url: "https://cbsn-ny.cbsnstream.cbsnews.com/out/v1/ec3897d58a9b45129a77d67aa247d136/master.m3u8",
    },
    LiveChannel {
        name: "Euronews Greek",
        url: "https://cdn-euronews.akamaized.net/live/eds/euronews-el/25068/index.m3u8",
    },
    LiveChannel {
        name: "LiveNOW from FOX",
        url: "https://fox-foxnewsnow-vizio.amagi.tv/playlist.m3u8",
    },
];
