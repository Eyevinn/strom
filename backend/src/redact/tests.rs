use super::*;
use crate::blocks::builtin::mpegtssrt::MpegTsSrtOutputBuilder;
use crate::blocks::builtin::mpegtssrt_input::MpegTsSrtInputBuilder;
use crate::blocks::{BlockBuildContext, BlockBuilder};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Everything logged on this thread while `f` runs, at every level.
fn capture_logs(f: impl FnOnce()) -> String {
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || Writer(sink.clone()))
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = buf.lock().unwrap().clone();
    String::from_utf8_lossy(&bytes).into_owned()
}

struct Writer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Writer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The SRT blocks logged their whole `srt_uri` at `info` on every build,
/// passphrase and stream id included.
#[test]
fn srt_blocks_log_no_passphrase() {
    gstreamer::init().expect("GStreamer init");
    let uri = "srt://192.0.2.1:9000?mode=caller&passphrase=topsecret1&streamid=feed42";
    let mut properties = HashMap::new();
    properties.insert("srt_uri".to_string(), PropertyValue::String(uri.into()));
    let ctx = BlockBuildContext::new(vec![], "all".to_string());

    let logs = capture_logs(|| {
        MpegTsSrtInputBuilder
            .build("srt_in", &properties, &ctx)
            .expect("input block");
        MpegTsSrtOutputBuilder
            .build("srt_out", &properties, &ctx)
            .expect("output block");
    });

    assert!(logs.contains("SRT source configured"), "{}", logs);
    assert!(logs.contains("SRT sink configured"), "{}", logs);
    for secret in ["topsecret1", "feed42"] {
        assert!(!logs.contains(secret), "{} in:\n{}", secret, logs);
    }
}

// Test vectors: every input that once leaked, each also checked as a
// `{:?}` dump and a dump of a dump.

/// Inputs and the secrets in them that must not survive.
const LEAKS: &[(&str, &[&str])] = &[
    // An `@` in the query.
    ("https://h/a?foo@bar&token=SEKRET", &["SEKRET"]),
    ("https://h/a?mail@x.de=1&token=SEKRET", &["SEKRET"]),
    ("https://h?my@token=SEKRET", &["SEKRET"]),
    (
        "https://user:p@ss@h?x@y&passphrase=SEKRET",
        &["SEKRET", "p@ss"],
    ),
    ("srt://h?passphrase=SEKRET&x=@", &["SEKRET"]),
    ("srt://h:9000?streamid=#!::u=a@b,r=SEKRET", &["SEKRET"]),
    (
        "srt://192.0.2.1:9000?streamid=#!::u=a@b,r=x&passphrase=SEKRET",
        &["SEKRET"],
    ),
    (
        "srt://192.0.2.1:9000?passphrase=top@SEKRET&latency=120",
        &["SEKRET"],
    ),
    ("srt://[::1]?passphrase=p@SEKRET", &["SEKRET"]),
    ("srt://ü?ä@ö&token=SEKRET", &["SEKRET"]),
    // Userinfo with `@`, `/`, `?`, `[`, quotes, backslashes.
    ("rtmp://u:SEKRET@h/app/KEYX?x@y", &["SEKRET", "KEYX"]),
    ("rtmp://h?a@b/KEYX", &["KEYX"]),
    ("http://h/@/x?token=SEKRET", &["SEKRET"]),
    (
        "srt://user:pa?ss=SEKA@h:9000?passphrase=SEKB",
        &["SEKA", "SEKB"],
    ),
    ("srt://user:SEKRET?R@h:9000", &["SEKRET"]),
    ("srt://u:SEKA?ss=x@h:9000", &["SEKA"]),
    ("srt://u:SEKA/b@h:9000", &["SEKA"]),
    ("srt://user:1234?x=SEKRET@h:9000", &["SEKRET"]),
    ("srt://user:?x=SEKRET@h:9000", &["SEKRET"]),
    ("srt://:12?x=SEKRET@h", &["SEKRET"]),
    ("srt://user:12/w?x=SEKRET@h", &["SEKRET"]),
    ("srt://u:[x]?a=SEKRET@h", &["SEKRET"]),
    ("srt://u:[SEKRET]@h", &["SEKRET"]),
    ("srt://us(SEKRET)er:9000", &["SEKRET"]),
    ("srt://u:p@ss?w=SEKRET@h", &["SEKRET"]),
    ("srt://u:SEKRET@[::1]:9000?x@y", &["SEKRET"]),
    ("srt://ü:SEKA@hö?passphrase=SEKB", &["SEKA", "SEKB"]),
    ("https://u%3ASEKRET@h/x", &["SEKRET"]),
    ("http://u:SEKRET@[fe80::1%eth0]:8080/x", &["SEKRET"]),
    ("turn://user:SEKRET@turn.example.com:3478", &["SEKRET"]),
    ("rtmp://user:p/ss@host/live/KEYX", &["p/ss", "KEYX"]),
    ("rtmp://user:1234?x=SEKRET@h/app/KEYX", &["SEKRET", "KEYX"]),
    (
        "rtmps://alice:SEKRET@example.com/live/KEYX",
        &["SEKRET", "KEYX"],
    ),
    ("srt://user:'SEKRET@h:9000?mode=caller", &["SEKRET"]),
    ("srt://u:12'SEKRET@h:9000", &["SEKRET"]),
    ("uri=srt://u:1'SEKRET@h", &["SEKRET"]),
    ("srt://:'SEKRET@h:9000", &["SEKRET"]),
    ("srt://h:9000'SEKRET@x", &["SEKRET"]),
    ("https://user:\"SEKRET@h/x", &["SEKRET"]),
    ("srt://user:\"SEKRET@h", &["SEKRET"]),
    ("srt://\\SEKRET@h", &["SEKRET"]),
    ("srt://u:`SEKRET@h", &["SEKRET"]),
    ("srt://u:<SEKRET@h", &["SEKRET"]),
    ("srt://u:ab/c\"SEKRET@h", &["SEKRET"]),
    ("String(\"srt://u:12\\\"SEKRET@h:9000\")", &["SEKRET"]),
    ("\"srt://u:pa\"SEKRET@h\"", &["SEKRET"]),
    ("\"srt://user\"SEKRET@h\"", &["SEKRET"]),
    ("uri=\"srt://u:'SEKA SEKB'@h\"", &["SEKA", "SEKB"]),
    ("us@er://x", &["us@"]),
    // Without `//`.
    ("rtmp:/user:SEKRET@host/live/KEYX", &["SEKRET", "KEYX"]),
    ("turn:user:SEKRET@turn.example.com", &["SEKRET"]),
    // Query values and keys.
    ("srt://h:9000?mode=caller&passphrase=SEKRET", &["SEKRET"]),
    ("srt://[::1]:9000?passphrase=SEKRET", &["SEKRET"]),
    ("srt://h:9000?a=1?passphrase=SEKRET", &["SEKRET"]),
    ("https://h/a#x?token=SEKRET", &["SEKRET"]),
    ("https://h/x?token=SEKA%40SEKB", &["SEKA", "SEKB"]),
    ("https://h/x?auth_token=SEKRET", &["SEKRET"]),
    ("https://h/x?X-Amz-Signature=SEKRET&x=1", &["SEKRET"]),
    ("srt://u:p@h:9000?x=1'Y&passphrase=SEKRET", &["SEKRET"]),
    ("uri=srt://h?cb=https://u:SEKRET@x/y", &["SEKRET"]),
    ("https://h/?next=srt://h?passphrase=SEKRET", &["SEKRET"]),
    ("uri=srt://h?passphrase=ab'SEKRET x", &["SEKRET"]),
    ("uri=srt://h?passphrase=\"SEKA SEKB\"", &["SEKA", "SEKB"]),
    (
        "{\"uri\":\"srt://h:1?passphrase=SEKRET\",\"latency\":120}",
        &["SEKRET"],
    ),
    (
        "{\"uri\":\"srt://h?passphrase=ab\\\"SEKRET\",\"x\":1}",
        &["SEKRET"],
    ),
    ("<srt://h:1?streamid=SEKRET>", &["SEKRET"]),
    (
        "Could not open srt://192.0.2.1:9000?passphrase=SEKRET: refused",
        &["SEKRET"],
    ),
    (
        "srtsink uri=srt://h:1?streamid=SEKA passphrase=SEKB latency=120",
        &["SEKA", "SEKB"],
    ),
    // RTMP stream keys.
    ("rtmp://h/live/KEYX", &["KEYX"]),
    ("rtmp://h/live/KEYX: Connection refused", &["KEYX"]),
    ("rtmp://u:SEKA@h/app/KEYX?x=SEKB", &["SEKA", "SEKB", "KEYX"]),
    ("rtmp://ü/ä/KEYX?ö@ü", &["KEYX"]),
    ("rtmp?://h/app/KEYX", &["KEYX"]),
    ("rtmp:/h/KEYX?r=http://x", &["KEYX"]),
    ("String(\"rtmp://h/live/KEYX\")", &["KEYX"]),
    // A URL in quotes is the quoted run, blanks and all.
    ("srtsink uri=\"srt://h:9000?passphrase=SEKA SEKB\" sync=false", &["SEKA", "SEKB"]),
    ("uri='srt://h:9000?mode=caller&passphrase=SEKA SEKB'", &["SEKA", "SEKB"]),
    ("uri=\"srt://u:SEKA SEKB@h:9000\"", &["SEKA", "SEKB"]),
    ("\"srt://u:12 SEKRET@h:9000\"", &["SEKRET"]),
    ("location=\"rtmp://h/live/SEKA SEKB\"", &["SEKA", "SEKB"]),
    ("whip-endpoint=\"https://h/whip/ep?token=SEKA SEKB\"", &["SEKA", "SEKB"]),
    ("\"srt://h:9000?passphrase=SEKA&x=1 SEKB\"", &["SEKA", "SEKB"]),
    ("String(\"srt://h:9000?passphrase=SEKA SEKB\")", &["SEKA", "SEKB"]),
    (
        "{\"srt_uri\": String(\"srt://h:9000?mode=caller&passphrase=SEKA SEKB\")}",
        &["SEKA", "SEKB"],
    ),
    ("x \"srt://h:9000?passphrase=SEKA SEKB", &["SEKA", "SEKB"]),
    (
        "gst-launch-1.0 srtsink \"uri=srt://h:9000?mode=caller&passphrase=SEKA SEKB\"",
        &["SEKA", "SEKB"],
    ),
    ("\"uri=srt://h:9000?passphrase=SEKA SEKB\"", &["SEKA", "SEKB"]),
    ("'location=rtmp://h/live/SEKA SEKB' x", &["SEKA", "SEKB"]),
    ("uri=`srt://h?passphrase=SEKA SEKB`", &["SEKA", "SEKB"]),
    ("uri='srt://h?passphrase=SEKA's SEKB'", &["SEKA", "SEKB"]),
    // A quote around the whole argument holds a secret value too.
    (
        "gst-launch-1.0 srtsrc \"passphrase=SEKA SEKB\" uri=srt://h:9000 ! fakesink",
        &["SEKA", "SEKB"],
    ),
    ("srtsrc 'passphrase=SEKA SEKB' latency=120", &["SEKA", "SEKB"]),
    ("srtsink \"streamid=#!::r=live,passphrase=SEKA SEKB\"", &["SEKA", "SEKB"]),
    ("\"name=out uri=srt://h:9000?passphrase=SEKA SEKB\"", &["SEKA", "SEKB"]),
    ("\"name=out passphrase=SEKA SEKB\" x", &["SEKA", "SEKB"]),
    ("srtsrc 'passphrase=SEKA's SEKB' x", &["SEKA", "SEKB"]),
    ("\"srt://u:pa\"-SEKRET@h\"", &["SEKRET"]),
    // A key in the path of an HTTP endpoint (Cloudflare Stream WHIP).
    (
        "whipclientsink0: Unexpected status 401 for https://customer-abc.cloudflarestream.com/SEKRETKEY0123/webRTC/publish",
        &["SEKRETKEY0123"],
    ),
    ("error sending request for url (https://h/SEKRETKEY/webRTC/publish)", &["SEKRETKEY"]),
    ("wss://h/SEKRETKEY", &["SEKRETKEY"]),
    // Pairs: names, several in a word, a secret name before `://`.
    ("auth_token=SEKRET", &["SEKRET"]),
    ("auth-token=SEKRET", &["SEKRET"]),
    ("stream_key=SEKRET", &["SEKRET"]),
    ("streamKey=SEKRET", &["SEKRET"]),
    ("X-Amz-Signature=SEKRET", &["SEKRET"]),
    ("pass%70hrase=SEKRET", &["SEKRET"]),
    ("pass%77ord=SEKRET", &["SEKRET"]),
    ("(passphrase=SEKRET)", &["SEKRET"]),
    ("latency=120&passphrase=SEKRET", &["SEKRET"]),
    ("mode=caller;passphrase=SEKRET", &["SEKRET"]),
    ("opts=latency=1,streamid=SEKRET", &["SEKRET"]),
    ("a=b,passphrase=SEKRET", &["SEKRET"]),
    ("caps=application/x-srt,passphrase=SEKRET", &["SEKRET"]),
    ("passphrase=SEKRET://x", &["SEKRET"]),
    ("token=ab:SEKRET//x://y", &["SEKRET"]),
    ("{\"details\":\"passphrase=SEKRET\"}", &["SEKRET"]),
    // Quotes and backslashes in values.
    ("passphrase=ab\\SEKRET x", &["SEKRET"]),
    ("passphrase=\\SEKRET", &["SEKRET"]),
    ("passphrase=ab'SEKRET x", &["SEKRET"]),
    ("e: \"passphrase=ab\\\"SEKRET\" x\"", &["SEKRET"]),
    ("passphrase='SEKA\\'SEKB' x", &["SEKA", "SEKB"]),
    ("token=\"SEKA\\\"SEKB\" x", &["SEKA", "SEKB"]),
    ("passphrase=\\\\\"SEKA SEKB\\\\\"", &["SEKA", "SEKB"]),
    (
        "srtsink passphrase=\"two SEKA words\" latency=120",
        &["SEKA"],
    ),
    (
        "{\"e\":\"passphrase=\\\"a\\\\\\\"SEKA\\\" SEKB\"}",
        &["SEKA"],
    ),
    ("passphrase=\"SEKRET", &["SEKRET"]),
    ("x passphrase=\"ü SEKRET ö\" y", &["SEKRET"]),
    ("PassPhrase='SEKA SEKB'", &["SEKA", "SEKB"]),
    ("name=\"v\" passphrase=\"SEKRET\" after \"q\"", &["SEKRET"]),
    ("a passphrase='SEKRET' b 'c' d", &["SEKRET"]),
    ("auth-token='SEKA SEKB'", &["SEKA", "SEKB"]),
    ("passphrase=(string)\"SEKA SEKB\"", &["SEKA", "SEKB"]),
    ("passphrase=ab\"SEKA SEKB\"", &["SEKA", "SEKB"]),
    ("é?passphrase=\"SEKA SEKB\"", &["SEKA", "SEKB"]),
    ("passphrase=\"abc\"SEKRET", &["SEKRET"]),
    ("passphrase= SEKRET", &["SEKRET"]),
    // `name: value` and quoted names.
    ("\"passphrase\"=\"SEKRET\"", &["SEKRET"]),
    ("'passphrase'='SEKRET'", &["SEKRET"]),
    ("{\"passphrase\":\"SEKRET\",\"x\":1}", &["SEKRET"]),
    ("\"passphrase\": \"SEKA SEKB\"", &["SEKA", "SEKB"]),
    ("Foo { passphrase: \"SEKRET\" }", &["SEKRET"]),
    ("Authorization: Bearer SEKRET", &["SEKRET"]),
    ("Authorization:Bearer SEKRET", &["SEKRET"]),
    ("Authorization=Bearer SEKRET", &["SEKRET"]),
    ("srt://h?md5=SEKRET&expires=1", &["SEKRET"]),
    ("srt://h?wowzatokenhash=SEKRET", &["SEKRET"]),
    ("hdnts=exp=1~hmac=SEKRET", &["SEKRET"]),
    // Nothing to leak, nothing may panic.
    ("token=\"", &[]),
    ("token=\\\"", &[]),
    ("a=\"ü", &[]),
    ("ü=\"ä", &[]),
    ("é://x", &[]),
    ("ü@é:ö", &[]),
    ("\"'\\", &[]),
    ("?a@b", &[]),
    ("?@", &[]),
    ("@", &[]),
    ("@?", &[]),
    ("?", &[]),
    ("", &[]),
    ("://", &[]),
    ("?://@", &[]),
    ("h/live/KEYX?cb=http://x", &[]),
    // A mask must not end before the end of the word `mask_word` masked.
    ("srtsrc 'passphrase=SEKA'\\''SEKB' latency=1", &["SEKA", "SEKB"]),
    ("srtsrc 'passphrase=SEKA'\"'\"'SEKB'", &["SEKA", "SEKB"]),
    ("srtsrc \"passphrase=SEKA\"'SEKB'", &["SEKA", "SEKB"]),
    ("'passphrase=SEKA'SEKB'SEKC'", &["SEKA", "SEKB", "SEKC"]),
    // A shell-quoted value with a blank and an apostrophe.
    ("passphrase='SEKA SEKB'\\''SEKC' x", &["SEKA", "SEKB", "SEKC"]),
    (
        "'passphrase=SEKA'\"'\"'SEKB SEKC'\"'\"'SEKD' x",
        &["SEKA", "SEKB", "SEKC", "SEKD"],
    ),
    // A stray quote before a quoted secret.
    ("the '90s and 'passphrase=SEKA SEKB' x", &["SEKA", "SEKB"]),
    ("location=/a/\"x.ts and \"passphrase=SEKA SEKB\" x", &["SEKA", "SEKB"]),
    ("the '90s uri='srt://h?passphrase=SEKA SEKB'", &["SEKA", "SEKB"]),
    ("\"open {\"details\":\"passphrase=SEKA SEKB\"} x", &["SEKA", "SEKB"]),
    ("location=/a/\"x.ts {\"details\":\"token=SEKA SEKB\"}", &["SEKA", "SEKB"]),
    // A raw blank in a password.
    ("srt://u:SEKA SEKB@h:9000", &["SEKA", "SEKB"]),
    ("Could not open srt://u:SEKA SEKB SEKC@h:9000: refused", &["SEKA", "SEKB", "SEKC"]),
    ("srt://u:SEKA's SEKB's SEKC@h", &["SEKA", "SEKB", "SEKC"]),
    ("\"open srt://u:SEKA's SEKB\"SEKC's SEKD@h:9000) done", &["SEKA", "SEKB", "SEKC", "SEKD"]),
    // A raw `&` in a secret value: behind it only known names show.
    ("srt://h?passphrase=SEKA&SEKB=SEKC", &["SEKA", "SEKB", "SEKC"]),
    ("srt://h?passphrase=SEKA&latency=1&SEKB=SEKC", &["SEKA", "SEKB", "SEKC"]),
    ("srt://h?passphrase=SEKA&SEKBtoken=SEKC", &["SEKA", "SEKB", "SEKC"]),
    // A token in the path of another scheme, an `@` in a path.
    ("rtsp://h:554/SEKRET0123456789/stream", &["SEKRET"]),
    ("srt://h:9000/SEKRETabcdefghij1", &["SEKRET"]),
    ("rtsp://u:p@h/live/Abcdefghijklmnop: 404", &["Abcdefghijklmnop"]),
    ("rtmp://h/live/SEKA@SEKB", &["SEKA", "SEKB"]),
    ("https://h/whip/SEKA@SEKB/x", &["SEKA", "SEKB"]),
    // HTTP path words: anything else masks the rest.
    ("https://h/whip/SEKRET/whip", &["SEKRET"]),
    ("https://h/live/SEKRET.m3u8", &["SEKRET"]),
    ("https://h/index.SEKRET", &["SEKRET"]),
    ("https://h/segment_SEKRET.ts", &["SEKRET"]),
    ("https://h/stream-SEKRET/whip", &["SEKRET"]),
    ("https://h/whip/12345678901", &["12345678901"]),
    ("https://h/v12345678901", &["12345678901"]),
    (
        "https://h/live/live-1234567890-9876543210-5555555555",
        &["9876543210", "5555555555"],
    ),
    ("https://h/live/chunk-12345-678901.m4s", &["678901"]),
    ("wss://h/whip/SEKRET", &["SEKRET"]),
    ("https://h/whip?token=SEKRET", &["SEKRET"]),
    // A shell quote in a quoted value: the rest of the text is masked.
    ("srtsrc 'passphrase=\"SEKA\"'\\''s SEKB' x", &["SEKA", "SEKB"]),
    (
        "srtsrc 'passphrase=SEKA\"SEKB\"SEKC'\\'', SEKD' x",
        &["SEKA", "SEKB", "SEKC", "SEKD"],
    ),
    // A header value runs to the end of its line.
    ("auth-token: SEKA SEKB", &["SEKA", "SEKB"]),
    ("x-auth-token: SEKA SEKB, SEKC", &["SEKA", "SEKB", "SEKC"]),
    ("Authorization: Bearer SEKA SEKB", &["SEKA", "SEKB"]),
    // Behind a blank in quotes only a known pair ends a value.
    ("\"passphrase=SEKA SEKB latencyx=1\"", &["SEKA", "SEKB"]),
    ("\"passphrase=SEKA SEKBtoken=x\"", &["SEKA", "SEKB"]),
    ("\"srt://h?passphrase=SEKA SEKB&latency=1\"", &["SEKA", "SEKB"]),
    // A quoted value ends at its partner only where a value ends.
    ("passphrase=\"SEKA\",SEKB x", &["SEKA", "SEKB"]),
    ("{\"passphrase\":\"SEKA\"SEKB}", &["SEKA", "SEKB"]),
    ("'passphrase=SEKA',SEKB x", &["SEKA", "SEKB"]),
    ("\"passphrase=SEKA\"}SEKB", &["SEKA", "SEKB"]),
    ("Foo { passphrase: \"SEKA\"SEKB, x: 1 }", &["SEKA", "SEKB"]),
];

/// Ordinary lines that must come back unchanged.
const UNCHANGED: &[&str] = &[
    "flow started id=abc name=\"Main\" latency=120",
    "srt://192.0.2.1:9000?mode=caller&latency=120",
    "srt://:9000?mode=listener&latency=200&pbkeylen=16",
    "key-int-max=60 bitrate=4000 pbkeylen=16",
    "Could not connect to srt://h:9000: Connection refused",
    "<srt://h:9000>",
    "pad src (caps=\"video/x-raw, width=1920\")",
    "Element { name: \"srtsink0\", uri: \"srt://h:9000?mode=caller\" }",
    "{\"uri\":\"srt://h:9000?mode=caller\",\"latency\":120}",
    "srt://kamera-büro.local:9000?latency=200",
    "http://[fe80::1%eth0]:8080",
    "Failed to open srt://h:9000?mode=caller&latency=120: Connection refused",
    "rtmp://a.rtmp.youtube.com/live2: Connection refused",
    "srtsink uri=\"srt://h:9000?mode=caller\" sync=false",
    "srtsink \"uri=srt://h:9000?mode=caller\" sync=false",
    "name=\"a\" uri=srt://h:9000 sync=false",
    "can't open srt://h:9000: Connection refused",
    "https://example.com/ refused",
    "rtmps://example.com/live",
    "Loading file: file:///home/user/My%20Video.mp4",
    "mail from user@example.com at 12:30:00 user=alice",
    "state changed to PLAYING",
    // HTTP path words and file names, other paths without tokens.
    "GET https://h:8443/live/index.m3u8: 404 Not Found",
    "https://h/hls/stream0/segment_001.ts not found",
    "https://h/hls/segment_1730000000.ts not found",
    "Element { name: \"whep\", uri: \"https://h/whep/resource\" }",
    "rtsp://h:554/Streaming/Channels/101",
    "rtsp://h/cam/realmonitor?channel=1&subtype=0",
    "file:///home/user/2026-10-31T12-30-00_recording.mp4",
    // A quote that ends behind a blank still closes.
    "{\"a\":\"b \",\"uri\":\"srt://h:9000\"}",
];

/// `s`, `{:?}` of it and `{:?}` of that.
fn dumps(s: &str) -> [String; 3] {
    let once = format!("{:?}", s);
    let twice = format!("{:?}", once);
    [s.to_string(), once, twice]
}

/// Inputs that would be slow on a quadratic path, and odd ones.
fn stress_inputs() -> Vec<String> {
    let n = 1 << 16;
    let mut inputs: Vec<String> = [
        "a",
        "\"",
        "'",
        "\\",
        "\\\"",
        "=",
        ":",
        "@",
        "a=",
        "x:",
        "passphrase=\"",
        "passphrase=a ",
        "srt://",
        "a://",
        "\"srt://h\"",
        "@?",
        "srt://h?a=1&",
        "ü",
        "😀=",
        "passphrase=😀\"",
    ]
    .iter()
    .map(|unit| unit.repeat(n / unit.len().max(1)))
    .collect();
    inputs.push(format!("passphrase=\"{}", "\\\"".repeat(n)));
    inputs.push(format!("srt://h?{}", "a=1&".repeat(n)));
    inputs
}

/// Inputs that once panicked: a quote that closed in a skipped part,
/// a backtick, more nested quotes than are tracked.
fn panic_inputs() -> Vec<String> {
    let mut inputs: Vec<String> = [
        "'til now: srtsrc \"passphrase=it's\" ! srtsink uri='srt://h:9000'",
        "the '90s srtsrc \"passphrase=it's x\" ! srtsink 'uri=srt://h:9000'",
        "a `\"passphrase=x` b\" srt://h:9000",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for quote in ["\"", "'", "`"] {
        let nested: String = (0..20)
            .map(|i| format!(" {}{}a", "\\".repeat(i % 4), quote))
            .collect();
        inputs.push(format!("{} passphrase=x srt://h:9000?a=1 b", nested));
        inputs.push(format!("passphrase={} srt://h:9000 x", nested));
    }
    inputs
}

/// `n` pseudo-random gst-like lines of quotes, backslashes, pairs and
/// URLs, the same on every run.
fn fuzz_inputs(n: usize) -> impl Iterator<Item = String> {
    const TOKENS: &[&str] = &[
        "\"",
        "'",
        "`",
        "\\",
        " ",
        " ",
        "a",
        "it's",
        "=",
        ":",
        "@",
        "?",
        "&",
        ",",
        ")",
        "!",
        "srt://h:9000",
        "srt://u:p@h",
        "https://h/p?x=1",
        "rtmp://h/live/k",
        "passphrase=",
        "uri=",
        "token:",
        "Bearer ",
        "?passphrase=",
        "ü",
        "\n",
    ];
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..n).map(move |_| {
        let len = 1 + next() % 24;
        (0..len)
            .map(|_| TOKENS[(next() % TOKENS.len() as u64) as usize])
            .collect()
    })
}

#[test]
fn shared_vectors_leak_nothing() {
    for (raw, secrets) in LEAKS {
        for input in dumps(raw) {
            let shown = text(&input);
            for secret in *secrets {
                assert!(!shown.contains(secret), "{:?} -> {:?}", input, shown);
            }
            let shown = url(&input);
            for secret in *secrets {
                assert!(!shown.contains(secret), "url {:?} -> {:?}", input, shown);
            }
        }
    }
}

#[test]
fn shared_vectors_keep_ordinary_lines() {
    for line in UNCHANGED {
        for input in dumps(line) {
            assert_eq!(text(&input), input.as_str());
        }
    }
}

#[test]
fn long_and_odd_inputs_do_not_panic() {
    for input in stress_inputs() {
        let _ = text(&input);
        let _ = url(&input);
        let _ = rtmp_url(&input);
    }
}

/// The masking itself must not panic, not only `text` behind its guard.
#[test]
fn fuzzed_inputs_do_not_panic() {
    for input in panic_inputs().into_iter().chain(fuzz_inputs(100_000)) {
        let run = std::panic::catch_unwind(|| {
            let _ = mask_text(&input);
            let _ = url(&input);
            let _ = rtmp_url(&input);
        });
        assert!(run.is_ok(), "panicked on {:?}", input);
    }
}

/// A quote open over the whole text with URLs in it, quoted and not, or
/// many quoted URLs in one word: a lookup must not scan the rest of the
/// text again for each URL. Quadratic, the first took 30 s in a debug
/// build, the others longer; now each takes well under a second.
#[test]
fn long_quoted_runs_take_linear_time() {
    let n = 1 << 18;
    let inputs = [
        format!("\"{}\"", " 'srt://h:9000' srt://h:9000".repeat(n / 28)),
        format!(
            "{:?}",
            format!("\"{}\"", " \"srt://a\" srt://b".repeat(n / 20))
        ),
        "\"srt://h?passphrase=a\",".repeat(n / 23),
        format!("\"{} \"{}", " srt://h".repeat(n / 16), "x".repeat(n / 2)),
    ];
    for input in inputs {
        let start = std::time::Instant::now();
        let _ = mask_text(&input);
        let took = start.elapsed();
        assert!(took.as_secs() < 5, "{:?} for {:?}…", took, &input[..40]);
    }
}

#[test]
fn srt_passphrase_and_streamid_are_masked() {
    assert_eq!(
        url("srt://192.0.2.1:9000?mode=caller&passphrase=topsecret1&latency=120"),
        "srt://192.0.2.1:9000?mode=caller&passphrase=***&latency=120"
    );
    // A value that is not plain masks all after it.
    assert_eq!(
        url("srt://h:9000?passphrase=topsecret1&streamid=#!::r=live/feed&latency=120"),
        "srt://h:9000?passphrase=***&streamid=***"
    );
}

/// An `@` behind the `?` can be a password with a raw `?` or a query
/// value (a stream id `u=user@domain`); everything after the scheme goes.
#[test]
fn an_at_sign_in_the_query_masks_all_after_the_scheme() {
    for raw in [
        "srt://192.0.2.1:9000?streamid=#!::u=a@b,r=x&passphrase=topsecret1",
        "srt://192.0.2.1:9000?passphrase=top@secret1&latency=120",
        "srt://user:pa?ss=x@h:9000",
    ] {
        assert_eq!(url(raw), "srt://***", "{}", raw);
    }
    assert_eq!(url("https://h/a?foo@bar&token=abc"), "https://***");
    assert_eq!(url("rtmp://user:pw?x=1@host/app/key"), "rtmp://***");
}

/// Anything but plain `host[:port]` in the authority may be a credential.
#[test]
fn an_odd_authority_is_masked() {
    assert_eq!(url("srt://u:[x]@h:1?a=1"), "srt://***@h:1?a=1");
    assert_eq!(url("srt://us(er)x:9000?a=1"), "srt://***");
    assert_eq!(url("srt://[::1]:9000?a=1"), "srt://[::1]:9000?a=1");
    assert_eq!(
        url("srt://kamera-büro.local:9000"),
        "srt://kamera-büro.local:9000"
    );
}

/// Only plain parts survive: a path segment or query pair with anything
/// else masks the rest, and text without `scheme://` is masked whole.
#[test]
fn what_is_not_plain_is_masked() {
    assert_eq!(url("srt://h/a b/c?x=1"), "srt://h/***");
    assert_eq!(url("srt://h/a?x=1&flag&y=2"), "srt://h/a?x=1&***");
    assert_eq!(url("srt://h/a?cb=http%3A%2F%2Fx"), "srt://h/a?cb=***");
    assert_eq!(url("turn:user:pw@h"), "***");
    assert_eq!(url(" rtmp://h/live/key"), "***");
    assert_eq!(url(""), "");
}

#[test]
fn srt_without_secrets_is_unchanged() {
    let raw = "srt://:9000?mode=listener&latency=200&pbkeylen=16";
    assert_eq!(url(raw), raw);
}

#[test]
fn userinfo_is_masked_by_the_last_at_sign() {
    assert_eq!(
        url("srt://user:p@ss/word@example.com/a"),
        "srt://***@example.com/a"
    );
    assert_eq!(url("us@er://x"), "***");
}

#[test]
fn rtmp_stream_key_and_query_are_masked() {
    assert_eq!(
        url("rtmps://example.com/live/streamkey123?token=abc"),
        "rtmps://example.com/live/***"
    );
    assert_eq!(
        url("rtmps://example.com/live?token=abc"),
        "rtmps://example.com/live?***"
    );
    assert_eq!(url("rtmps://example.com"), "rtmps://example.com");
    assert_eq!(rtmp_url("h/live/key"), "***");
    // A mistyped scheme is still masked.
    assert_eq!(url("rtmp:/user:pass@host/live/key"), "***");
}

#[test]
fn http_signature_is_masked() {
    assert_eq!(
        url("s3://example.com/a.mp4?X-Amz-Credential=c&X-Amz-Signature=s&x=1"),
        "s3://example.com/a.mp4?X-Amz-Credential=***&***"
    );
    // Behind a secret only known names show, up to the end.
    assert_eq!(
        url("srt://h?passphrase=a&streamid=b&latency=1&x=1"),
        "srt://h?passphrase=***&streamid=***&latency=1&***"
    );
}

/// An HTTP path may be the whole key (Cloudflare Stream WHIP), so HTTP
/// shows only where it goes and path words; `origin` not even those.
#[test]
fn http_shows_only_its_origin() {
    assert_eq!(url("https://h/whip/0123abcd/x"), "https://h/whip/***");
    assert_eq!(url("https://h/whip/publish"), "https://h/whip/publish");
    assert_eq!(origin("https://h/whip/publish"), "https://h/***");
    let cloudflare = "https://customer-abc.cloudflarestream.com/0123abcd/webRTC/publish";
    assert_eq!(
        url(cloudflare),
        "https://customer-abc.cloudflarestream.com/***"
    );
    assert_eq!(
        origin(cloudflare),
        "https://customer-abc.cloudflarestream.com/***"
    );
    assert_eq!(origin("https://u:p@h:8443?x=1"), "https://***@h:8443?***");
    assert_eq!(origin("srt://h:9000/a?b=1"), "srt://h:9000/***");
    assert_eq!(origin("https://h"), "https://h");
    assert_eq!(origin("stage"), "***");
}

#[test]
fn secret_names() {
    for name in [
        "passphrase",
        "auth_token",
        "auth-token",
        "authToken",
        "stream_key",
        "streamKey",
        "wowzatokenhash",
        "hmac",
        "md5",
        "password",
        "streamid",
        "key",
        "Authorization",
        "pass%70hrase",
    ] {
        assert!(is_secret_name(name), "{}", name);
    }
    for name in [
        "pbkeylen",
        "key-int-max",
        "srt_uri",
        "location",
        "latency",
        "",
    ] {
        assert!(!is_secret_name(name), "{}", name);
    }
}

#[test]
fn free_text_masks_urls_and_pairs() {
    let line = "Could not open srt://192.0.2.1:9000?passphrase=abcdefghij: refused";
    assert_eq!(
        text(line),
        "Could not open srt://192.0.2.1:9000?passphrase=*** refused"
    );

    let launch =
        "videotestsrc ! srtsink uri=srt://h:1?streamid=feed passphrase=abcdefghij latency=120";
    assert_eq!(
        text(launch),
        "videotestsrc ! srtsink uri=srt://h:1?streamid=*** passphrase=*** latency=120"
    );

    let quoted = "srtsink passphrase=\"two secret words\" latency=120";
    assert_eq!(text(quoted), "srtsink passphrase=*** latency=120");

    // A header value runs to the end of its line.
    let header = "Authorization: Bearer abc next";
    assert_eq!(text(header), "Authorization: ***");

    // The mask runs through the quote the key is in; the quote shows.
    let dump = format!("{:?}", PropertyValue::String("rtmp://h/live/key".into()));
    assert_eq!(text(&dump), "String(\"rtmp://h/live/***\")");

    let launch = "srtsink uri=\"srt://h:1?passphrase=two words\" sync=false";
    assert_eq!(
        text(launch),
        "srtsink uri=\"srt://h:1?passphrase=***\" sync=false"
    );

    let json = "{\"uri\":\"srt://h:1?mode=caller\",\"passphrase\":\"abc\"}";
    assert_eq!(
        text(json),
        "{\"uri\":\"srt://h:1?mode=caller\",\"passphrase\":***}"
    );
}

/// What follows a masked value shows when it is clearly not part of it.
#[test]
fn text_behind_a_mask_stays_readable() {
    for (raw, shown) in [
        // The partner of the quote the value is in, and what follows.
        (
            "{\"details\":\"passphrase=abc\"}",
            "{\"details\":\"passphrase=***\"}",
        ),
        (
            "{\"passphrase\":\"abc\",\"latency\":120}",
            "{\"passphrase\":***,\"latency\":120}",
        ),
        (
            "Foo { passphrase: \"abc\", latency: 120 }",
            "Foo { passphrase: ***, latency: 120 }",
        ),
        // A URL mask that ends inside the URL.
        (
            "{\"error\":\"Failed to open srt://h:9000?passphrase=abc&latency=120: Connection refused\"}",
            "{\"error\":\"Failed to open srt://h:9000?passphrase=***&latency=120: Connection refused\"}",
        ),
        (
            "\"srt://u:pw@h:9000 refused\"",
            "\"srt://***@h:9000 refused\"",
        ),
        // A known pair behind a blank in quotes, a `{:?}` dump of a line.
        (
            "\"srtsink passphrase=abc latency=120 ! fakesink\"",
            "\"srtsink passphrase=*** latency=120 ! fakesink\"",
        ),
        (
            "'uri=srt://h:9000?passphrase=a b mode=caller'",
            "'uri=srt://h:9000?passphrase=*** mode=caller'",
        ),
        ("passphrase: abc latency: 120", "passphrase: *** latency: 120"),
        // HTTP path words and file names.
        (
            "whipclientsink0: Unexpected status 401 for https://h/whip/0123abcd/x",
            "whipclientsink0: Unexpected status 401 for https://h/whip/***",
        ),
        (
            "https://h/v1/whip/publish/index.m3u8?token=abc",
            "https://h/v1/whip/publish/index.m3u8?***",
        ),
    ] {
        assert_eq!(text(raw), shown, "{}", raw);
    }
}

/// Former gaps: each now masked.
#[test]
fn former_gaps_are_masked() {
    for (raw, shown) in [
        ("passphrase='a b'\\''c' x", "passphrase=***"),
        (
            "the '90s and 'passphrase=a b' x",
            "the '90s and 'passphrase=***' x",
        ),
        (
            "location=/a/\"x.ts and \"passphrase=a b\" x",
            "location=/a/\"x.ts and \"passphrase=***\" x",
        ),
        ("srt://u:a b@h:9000 refused", "srt://***@h:9000 refused"),
        (
            "srt://h:9000?passphrase=a&b=c",
            "srt://h:9000?passphrase=***&***",
        ),
        (
            "srt://h:9000?passphrase=a&latency=120",
            "srt://h:9000?passphrase=***&latency=120",
        ),
        ("rtsp://h:554/0123456789abcdef0/live", "rtsp://h:554/***"),
        ("auth-token: a b", "auth-token: ***"),
        (
            "srtsrc 'passphrase=\"my\"'\\''s secret' x",
            "srtsrc 'passphrase=***",
        ),
        (
            "https://h/live/live-1234567890-9876543210-5555555555",
            "https://h/live/***",
        ),
        ("auth-token: a b latency=1 c", "auth-token: *** latency=1 c"),
    ] {
        assert_eq!(text(raw), shown, "{}", raw);
    }
}

#[test]
fn free_text_without_secrets_is_borrowed() {
    assert!(matches!(text("state changed to PLAYING"), Cow::Borrowed(_)));
    assert!(matches!(text("latency=120 srt://h:1"), Cow::Borrowed(_)));
}

#[test]
fn property_values() {
    let token = PropertyValue::String("bearer-123".into());
    assert_eq!(
        format!("{:?}", value("auth_token", &token)),
        "String(\"***\")"
    );

    let uri = PropertyValue::String("srt://h:1?passphrase=abcdefghij".into());
    assert_eq!(
        format!("{:?}", value("srt_uri", &uri)),
        "String(\"srt://h:1?passphrase=***\")"
    );

    // A URL property is one URL, blanks and all.
    let spaced = PropertyValue::String("srt://h:1?passphrase=two words".into());
    assert_eq!(
        format!("{:?}", value("srt_uri", &spaced)),
        "String(\"srt://h:1?passphrase=***\")"
    );

    // An endpoint shows only where it goes.
    let endpoint = PropertyValue::String("https://h/0123abcd/webRTC/publish".into());
    assert_eq!(
        format!("{:?}", value("whip_endpoint", &endpoint)),
        "String(\"https://h/***\")"
    );

    let latency = PropertyValue::Int(120);
    assert!(matches!(value("latency", &latency), Cow::Borrowed(_)));
}

#[test]
fn flow_dump_has_no_secrets() {
    let json = serde_json::json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "name": "f",
        "elements": [{
            "id": "e",
            "element_type": "srtsink",
            "properties": {"passphrase": "abcdefghij", "uri": "srt://h:1?passphrase=abcdefghij"},
            "pad_properties": {},
            "position": [0.0, 0.0]
        }],
        "blocks": [{
            "id": "b",
            "block_definition_id": "builtin.whip_output",
            "properties": {"auth_token": "bearer-123", "srt_uri": "srt://h:2?streamid=feed"},
            "position": {"x": 0.0, "y": 0.0}
        }]
    });
    let flow: Flow = serde_json::from_value(json).expect("flow");
    let dump = format!("{:?}", super::flow(&flow));
    for secret in ["abcdefghij", "bearer-123", "feed"] {
        assert!(!dump.contains(secret), "{} in {}", secret, dump);
    }
    assert!(dump.contains("srt://h:1"), "{}", dump);
}
