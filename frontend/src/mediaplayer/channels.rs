//! Live channels offered in the playlist editor next to the media folder.
//!
//! Publicly reachable news streams in English, one per outlet, plus SVT1,
//! from the broadcasters' own CDNs, and the Big Buck Bunny test stream. Public does not mean licensed for
//! redistribution, and a stream may be geo-blocked or move without notice.

/// A live channel: the name shown in the list and the URL the player opens.
pub struct LiveChannel {
    pub name: &'static str,
    pub url: &'static str,
}

pub const LIVE_CHANNELS: &[LiveChannel] = &[
    LiveChannel {
        name: "Al Jazeera",
        url: "https://live-hls-apps-aje-fa.getaj.net/AJE/index.m3u8",
    },
    LiveChannel {
        name: "BBC News",
        url: "https://vs-cmaf-push-ww-live.akamaized.net/x=4/i=urn:bbc:pips:service:bbc_news_channel_hd/pc_hd_abr_v2.mpd",
    },
    LiveChannel {
        name: "France 24",
        url: "https://live.france24.com/hls/live/2037218-b/F24_EN_HI_HLS/master_5000.m3u8",
    },
    LiveChannel {
        name: "DW",
        url: "https://amg01644-amg01644c1-amgplt0343.playout.now3.amagi.tv/ts-eu-w1-n2/playlist/amg01644-amg01644c1-amgplt0343/playlist.m3u8",
    },
    LiveChannel {
        name: "NHK World",
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
        name: "i24NEWS",
        url: "https://i24newsenglish-cdn.encoders.immergo.tv/master.m3u8",
    },
    LiveChannel {
        name: "Arirang",
        url: "https://amdlive-ch02-ctnd-com.akamaized.net/arirang_2ch/smil:arirang_2ch.smil/playlist.m3u8",
    },
    LiveChannel {
        name: "ABC News (Australia)",
        url: "https://abc-news-dmd-streams-1.akamaized.net/out/v1/701126012d044971b3fa89406a440133/index.m3u8",
    },
    LiveChannel {
        name: "ABC News (US)",
        url: "https://abcnews-streams.akamaized.net/hls/live/2023560/abcnewshudson1/master.m3u8",
    },
    LiveChannel {
        name: "CBS News",
        url: "https://cbsn-ny.cbsnstream.cbsnews.com/out/v1/ec3897d58a9b45129a77d67aa247d136/master.m3u8",
    },
    LiveChannel {
        name: "LiveNOW from FOX",
        url: "https://fox-foxnewsnow-vizio.amagi.tv/playlist.m3u8",
    },
    LiveChannel {
        name: "SVT1",
        url: "https://ed16.cdn.svt.se/l4/se/svt1/master-fmp4.m3u8?format=hls-cmaf-live",
    },
    LiveChannel {
        name: "Big Buck Bunny",
        url: "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8",
    },
];
