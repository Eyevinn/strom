//! Mask secrets before they reach a log line or an error text.
//!
//! Stream targets carry their credentials inline: an SRT `passphrase=` or
//! `streamid=` in the query, `user:pass@` before the host, an RTMP stream key in
//! the path, a token in an HTTP query. Blocks log the URI they configure, a flow
//! dump at `debug` carries every property, and GStreamer error texts may quote a
//! URI, so all of them go through here.
//!
//! The rules fail closed by construction: a part of a URL is shown only when it
//! matches a short list of plain forms (a host, a port, a path of plain
//! segments, `key=value` pairs of plain characters), everything else is masked.
//! Over-redaction is the acceptable direction: a log line may lose a harmless
//! part, a secret never survives.

use std::borrow::Cow;

use strom_types::{Flow, PropertyValue};

/// What a masked secret is replaced with.
pub const MASK: &str = "***";

/// Endings of a name (property or query key) whose value is a secret, matched
/// on the name in lower case without `-`, `_` and other separators:
/// `passphrase`, `auth_token`, `stream-key`, `streamKey`, `X-Amz-Signature`.
/// `pbkeylen` or `key-int-max` do not end in one, so they stay readable.
const SECRET_ENDINGS: &[&str] = &[
    "key",
    "sig",
    "auth",
    "pass",
    "pwd",
    "passwd",
    "password",
    "passphrase",
    "token",
    "secret",
    "signature",
    "credential",
    "credentials",
    "authorization",
    "cookie",
    "streamid",
    "hash",
    "hmac",
    "md5",
];

/// Values that name an authorization scheme, behind which the secret follows
/// as one more word: `Authorization: Bearer abc`.
const AUTH_SCHEMES: &[&str] = &["bearer", "basic", "digest"];

/// Parameter names (as [`plain_name`] reads them) that may show behind a
/// secret value, in a query (`passphrase=***&latency=120`) or behind a blank
/// in quotes (`"passphrase=*** latency=120"`): those of SRT, RIST and
/// GStreamer elements. Any other name there may be part of the secret
/// (`passphrase=a&b=c`) and is masked with the rest.
const KNOWN_PARAMS: &[&str] = &[
    "mode",
    "latency",
    "rcvlatency",
    "peerlatency",
    "pbkeylen",
    "localaddress",
    "localport",
    "adapter",
    "conntimeo",
    "connecttimeout",
    "maxbw",
    "inputbw",
    "oheadbw",
    "payloadsize",
    "transtype",
    "tlpktdrop",
    "nakreport",
    "linger",
    "timeout",
    "polltimeout",
    "waitforconnection",
    "autoreconnect",
    "keeplistening",
    "ipttl",
    "iptos",
    "mss",
    "sndbuf",
    "rcvbuf",
    "lossmaxttl",
    "minversion",
    "enforcedencryption",
    "kmrefreshrate",
    "kmpreannounce",
    "messageapi",
    "packetfilter",
    "groupconnect",
    "cname",
    "bandwidth",
    "buffer",
    "profile",
    "aestype",
    "expires",
    "name",
    "uri",
    "location",
    "sync",
    "async",
    "caps",
];

/// Words that may show as an HTTP or WebSocket path segment, alone or as
/// the parts of a file name (`segment_001.ts`). Anything else in such a path
/// may be the key: Cloudflare Stream publishes to `/<key>/webRTC/publish`.
const PATH_WORDS: &[&str] = &[
    "api",
    "v",
    "whip",
    "whep",
    "webrtc",
    "rtc",
    "publish",
    "play",
    "live",
    "stream",
    "streams",
    "endpoint",
    "resource",
    "resources",
    "session",
    "sessions",
    "ingest",
    "offer",
    "answer",
    "ice",
    "hls",
    "dash",
    "index",
    "playlist",
    "manifest",
    "master",
    "media",
    "video",
    "audio",
    "segment",
    "segments",
    "chunk",
    "chunks",
    "init",
    "app",
    "static",
    "assets",
    "status",
    "health",
    "metrics",
];

/// File extensions of a path word that names a file (`index.m3u8`).
const FILE_EXTENSIONS: &[&str] = &[
    "m3u8", "mpd", "ts", "m4s", "mp4", "m4a", "m4v", "aac", "vtt", "webm", "mkv", "mov", "json",
    "html", "js", "css", "png", "jpg", "ico", "txt", "xml",
];

/// Whether a property or query key names a secret.
pub fn is_secret_name(name: &str) -> bool {
    let plain = plain_name(name);
    SECRET_ENDINGS.iter().any(|end| plain.ends_with(end))
}

/// `name` in lower case, `%xx` decoded, letters and digits only: `Auth_Token`,
/// `auth-token`, `authToken` and `auth%5Ftoken` all read `authtoken`.
fn plain_name(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = String::with_capacity(name.len());
    let mut i = 0;
    while i < bytes.len() {
        let mut byte = bytes[i];
        let hex = |b: u8| (b as char).to_digit(16);
        if byte == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                byte = (high * 16 + low) as u8;
                i += 2;
            }
        }
        if byte.is_ascii_alphanumeric() {
            out.push(byte.to_ascii_lowercase() as char);
        }
        i += 1;
    }
    out
}

/// Mask the secrets in one URL.
///
/// Shown are only the scheme, a plain `host[:port]` or `[v6][:port]`, path
/// segments of plain characters (letters, digits, `.-_~%`) and query pairs
/// whose key and value are plain characters (letters, digits, `.-_~`). The
/// value of a secret key is masked, and behind it only pairs whose key is a
/// known parameter or a whole secret name (`KNOWN_PARAMS`) show. The first
/// part that is not shown is masked together with all after it. Userinfo
/// runs to the last `@` before the query and is masked; an `@` in the query
/// (a password with a raw `?`, or a value) masks everything after the
/// scheme. A path segment that looks like a token (16 or more letters,
/// digits, `-_%` with a digit or a capital letter in them) is masked with
/// the rest, except in a `file:` URL. `rtmp*` schemes: as [`rtmp_url`];
/// `http`, `https`, `ws`, `wss`: only the origin and path segments that are
/// known words (`PATH_WORDS`, also as a file name: `index.m3u8`,
/// `segment_001.ts`, with at most 10 digits), the query masked. Text that
/// does not start with `scheme://` is masked whole.
///
/// ```text
/// srt://h:9000?mode=caller&passphrase=abc -> srt://h:9000?mode=caller&passphrase=***
/// https://u:p@h/a.mp4?token=abc           -> https://***@h/***
/// https://h/whip/0123abcd/x               -> https://h/whip/***
/// srt://u:p?w=x@h:9000                    -> srt://***
/// ```
pub fn url(raw: &str) -> String {
    url_as(raw, Rules::ANY)
}

/// Mask the secrets in an RTMP URL.
///
/// As [`url`], and besides everything after the application (the first path
/// segment), the stream key, which for most servers is the whole
/// authorisation, and the whole query, which servers use for tokens under any
/// name. Host and application stay, so a line still names its output.
///
/// ```text
/// rtmp://u:p@h/live/key?t=1 -> rtmp://***@h/live/***
/// ```
pub fn rtmp_url(raw: &str) -> String {
    url_as(raw, Rules::RTMP)
}

/// Only where a URL goes: `scheme://host[:port]`, the path and query masked.
///
/// For HTTP endpoints (WHIP, WHEP, web pages), whose path may be the whole
/// credential: Cloudflare Stream publishes to
/// `https://customer-<id>.cloudflarestream.com/<key>/webRTC/publish`.
///
/// ```text
/// https://h:8443/abc/webRTC/publish?x=1 -> https://h:8443/***
/// ```
pub fn origin(raw: &str) -> String {
    url_as(raw, Rules::ORIGIN)
}

/// [`url`] with `rules`, or stricter ones when the scheme asks for them.
fn url_as(raw: &str, rules: Rules) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let Some(sep) = raw.find("://") else {
        return MASK.to_string();
    };
    let scheme = &raw[..sep];
    if !is_scheme(scheme) {
        return MASK.to_string();
    }
    mask_url(raw, sep, rules.and(Rules::of(scheme))).shown
}

/// What of a URL's path and query may show.
#[derive(Clone, Copy)]
struct Rules {
    /// How many path segments may show.
    segments: usize,
    /// Whether a segment behind those may show when it is a path word
    /// ([`path_word`]).
    words: bool,
    /// Whether a segment that looks like a token ([`token_like`]) is masked.
    tokens: bool,
    /// Whether query pairs may show.
    query: bool,
}

impl Rules {
    /// No limit beyond the plain forms.
    const ANY: Rules = Rules {
        segments: usize::MAX,
        words: true,
        tokens: false,
        query: true,
    };
    /// Plain parts; a token in the path is masked.
    const PLAIN: Rules = Rules {
        tokens: true,
        ..Rules::ANY
    };
    /// The application; the stream key and query are masked.
    const RTMP: Rules = Rules {
        segments: 1,
        words: false,
        tokens: false,
        query: false,
    };
    /// Path words only; any other segment and the query are masked.
    const HTTP: Rules = Rules {
        segments: 0,
        words: true,
        tokens: false,
        query: false,
    };
    const ORIGIN: Rules = Rules {
        segments: 0,
        words: false,
        tokens: false,
        query: false,
    };

    /// The rules of a scheme: RTMP for `rtmp*`, path words only for HTTP and
    /// WebSocket (a key in the path is common there), all of a local `file:`
    /// path, else plain.
    fn of(scheme: &str) -> Rules {
        let scheme = scheme.to_ascii_lowercase();
        if scheme.starts_with("rtmp") {
            return Rules::RTMP;
        }
        match scheme.as_str() {
            "http" | "https" | "ws" | "wss" => Rules::HTTP,
            "file" => Rules::ANY,
            _ => Rules::PLAIN,
        }
    }

    /// The stricter of both.
    fn and(self, other: Rules) -> Rules {
        Rules {
            segments: self.segments.min(other.segments),
            words: self.words && other.words,
            tokens: self.tokens || other.tokens,
            query: self.query && other.query,
        }
    }

    /// Whether path segment `n` may show.
    fn shows(self, n: usize, segment: &str) -> bool {
        let plain = segment.bytes().all(|b| plain_byte(b) || b == b'%');
        if segment.is_empty() {
            return true;
        }
        if !plain || (self.tokens && token_like(segment)) {
            return false;
        }
        n < self.segments || (self.words && path_word(segment))
    }
}

/// Whether a path segment is a word of [`PATH_WORDS`], or a file name
/// whose parts (split at `_` and `-`) are such words, each with digits
/// behind it, or numbers: `whip`, `v1`, `index.m3u8`,
/// `chunk-stream0-00001.m4s`. A number alone is no word, and a segment
/// with more than 10 digits in all is none (`live-1234567890-9876543210`
/// may be a numeric key).
fn path_word(segment: &str) -> bool {
    let (stem, ext) = match segment.rsplit_once('.') {
        Some((stem, ext)) => (stem, Some(ext)),
        None => (segment, None),
    };
    if ext.is_some_and(|ext| !FILE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e))) {
        return false;
    }
    if stem.bytes().filter(u8::is_ascii_digit).count() > 10 {
        return false;
    }
    let mut word = false;
    for part in stem.split(['_', '-']) {
        let letters = part.trim_end_matches(|c: char| c.is_ascii_digit());
        if letters.is_empty() && !part.is_empty() {
            continue;
        }
        if !PATH_WORDS.iter().any(|w| letters.eq_ignore_ascii_case(w)) {
            return false;
        }
        word = true;
    }
    word
}

/// Whether a path segment looks like a token: a run of 16 or more letters,
/// digits, `-`, `_` or `%` (between `.` and `~`) with a digit or a capital
/// letter in it. `ch01`, `Streaming`, `my-camera-front-door` and
/// `My%20Video.mp4` do not.
fn token_like(segment: &str) -> bool {
    segment.split(['.', '~']).any(|run| {
        run.len() >= 16
            && run
                .bytes()
                .any(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
    })
}

/// Whether `name` is a URL scheme: a letter, then letters, digits, `+-.`.
fn is_scheme(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic()) && name.chars().all(is_scheme_char)
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// A URL as shown, and where masking began.
struct Masked {
    shown: String,
    /// Byte offset in the URL of the first masked part, and in `shown`.
    from: Option<(usize, usize)>,
    /// Whether the URL ends in a masked part.
    tail: bool,
    /// Whether the authority was masked without an `@` in the URL: a
    /// password with a raw blank may go on to an `@` in a later word.
    open_authority: bool,
}

impl Masked {
    fn show(&mut self, part: &str) {
        self.shown.push_str(part);
        if !part.is_empty() {
            self.tail = false;
        }
    }

    fn mask(&mut self, at: usize) {
        self.from.get_or_insert((at, self.shown.len()));
        self.shown.push_str(MASK);
        self.tail = true;
    }
}

/// Mask `raw`, a URL whose scheme ends at `sep` (the `://`); see [`url`].
fn mask_url(raw: &str, sep: usize, rules: Rules) -> Masked {
    let body = sep + 3;
    let mut m = Masked {
        shown: raw[..body].to_string(),
        from: None,
        tail: false,
        open_authority: false,
    };
    let rest = &raw[body..];
    let query = rest.find('?');
    let before = &rest[..query.unwrap_or(rest.len())];
    if query.is_some_and(|q| rest[q..].contains('@')) {
        m.mask(body);
        return m;
    }

    // An `@` behind a `/` is in the path when a plain host without a port
    // comes first (`rtmp://h/live/a@b`), and ends a password with a raw `/`
    // when it does not (`rtmp://user:p/ss@h`). With a port it may be either
    // (`srt://u:12/34@h`): all is masked.
    let first = &before[..before.find('/').unwrap_or(before.len())];
    let at_sign = before.rfind('@').filter(|&at| {
        at < first.len() || !plain_host(first) || (first.contains(':') && !first.starts_with('['))
    });
    if at_sign.is_some_and(|at| at > first.len() && plain_host(first)) {
        m.mask(body);
        return m;
    }
    let host_at = match at_sign {
        Some(at) => {
            m.mask(body);
            m.shown.push('@');
            at + 1
        }
        None => 0,
    };
    let after = &before[host_at..];
    let slash = after.find('/').unwrap_or(after.len());
    if !plain_host(&after[..slash]) {
        m.mask(body + host_at);
        m.open_authority = host_at == 0;
        return m;
    }
    m.show(&after[..slash]);

    let mut at = body + host_at + slash;
    if let Some(path) = after[slash..].strip_prefix('/') {
        for (n, segment) in path.split('/').enumerate() {
            m.shown.push('/');
            at += 1;
            if !rules.shows(n, segment) {
                m.mask(at);
                return m;
            }
            m.show(segment);
            at += segment.len();
        }
    }

    let Some(q) = query else {
        return m;
    };
    m.shown.push('?');
    let mut at = body + q + 1;
    if !rules.query {
        m.mask(at);
        return m;
    }
    // Behind a secret value only known names show, up to the end: the value
    // may hold a raw `&` (`passphrase=a&b=c`).
    let mut after_secret = false;
    for (n, pair) in rest[q + 1..].split('&').enumerate() {
        if n > 0 {
            m.shown.push('&');
        }
        match pair.split_once('=') {
            Some((key, value))
                if !key.is_empty()
                    && key.bytes().all(plain_byte)
                    && (!after_secret || known_name(key)) =>
            {
                m.show(key);
                m.shown.push('=');
                let value_at = at + key.len() + 1;
                if !value.bytes().all(plain_byte) {
                    m.mask(value_at);
                    return m;
                }
                match is_secret_name(key) {
                    true => {
                        after_secret = true;
                        m.mask(value_at);
                    }
                    false => m.show(value),
                }
            }
            _ if pair.is_empty() => {}
            _ => {
                m.mask(at);
                return m;
            }
        }
        at += pair.len() + 1;
    }
    m
}

/// Whether `name` may show behind a secret value: a known parameter, or a
/// secret ending as a whole name (`passphrase`, `token`), whose value is
/// masked in turn. A longer name only ends in one and may be part of the
/// secret (`a btoken=`).
fn known_name(name: &str) -> bool {
    let plain = plain_name(name);
    KNOWN_PARAMS.contains(&plain.as_str()) || SECRET_ENDINGS.contains(&plain.as_str())
}

/// A byte of a plain path segment, query key or value.
fn plain_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~')
}

/// Whether `authority` is `host[:port]` or `[v6%zone][:port]`, port digits
/// only. Anything else may be (part of) a credential.
fn plain_host(authority: &str) -> bool {
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let Some((addr, after)) = v6.split_once(']') else {
                return false;
            };
            let (addr, zone) = addr.split_once('%').unwrap_or((addr, ""));
            let plain = !addr.is_empty()
                && addr
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.'))
                && zone.bytes().all(plain_byte);
            if !plain {
                return false;
            }
            match after.strip_prefix(':') {
                Some(port) => ("", port),
                None if after.is_empty() => ("", ""),
                None => return false,
            }
        }
        None => authority.split_once(':').unwrap_or((authority, "")),
    };
    host.chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && port.bytes().all(|b| b.is_ascii_digit())
}

/// Mask every URL and every secret `name=value` or `name: value` in free text:
/// a log line, a GStreamer error, a gst-launch string, a `{:?}` dump.
///
/// A word is a run of non-blanks; quotes do not end it.
/// - A word with `://` is a URL behind a plain prefix (`uri=`, `String("`,
///   `<`): the URL goes through [`url`] (so HTTP shows only its origin and
///   path words), and a trailing `:`, `,`, `)`, quote or the like stays
///   outside it (`srt://h:9000: refused` keeps its host). A URL inside an
///   open quote (`uri="srt://…`, `"uri=srt://…`) is the whole quoted run,
///   blanks included, up to the partner of that quote, when the URL in its
///   word ends masked or an `@` follows. A URL whose authority is masked
///   with no `@` in its word masks on through the last `@` in its line
///   (`srt://u:a b@h`). Any other prefix masks the whole word.
/// - At each `=` or `:` of a word (quotes around the name allowed), a name
///   that ends in a secret ending ([`is_secret_name`]) masks the value: up to
///   the end of the word; when the separator ends the word, the next word
///   behind `=` (`passphrase= abc`) and the rest of the line behind `:`
///   (`auth-token: a b`, a header) up to a blank followed by a known pair
///   or a `!` (`auth-token: a b latency=1`); one more word behind `Bearer`,
///   `Basic`, `Digest`. A value that starts with a quote is the quoted run
///   through its partner (`{"passphrase":"a b","x":1}`).
/// - `user:pass@host` without `//` is masked whole.
/// - A quote that opens in a masked part (`passphrase="two words"`,
///   `passphrase=(string)"a b"`, `\"` in a `{:?}` dump) masks through its
///   partner with as many backslashes before it, or to the end of the text,
///   and on to the end of the partner's word unless the value ends there
///   (`ends_value`: `'a b'\''c'`).
/// - Quotes are followed from the start of the text: a `"`, `'` or `` ` ``
///   at the start of a word or behind a non-letter opens, the same quote with
///   as many backslashes closes, except at the start of a word or value
///   (behind a blank or `([{=:,;`) with a word behind it: in
///   `the '90s … 'passphrase=a b'` the second opens. Whenever masking begins
///   inside an open quote (`"passphrase=a b"`, `'uri=srt://h?passphrase=a
///   b'`, a quote opened in an earlier word), the mask runs through the
///   partner of the innermost one, or to the end of the text; on past it
///   when a letter follows (an apostrophe in the value), to the end of its
///   word when the value does not end there (`'a'\''b'`) or an `@` is in
///   that word. When the value runs on past the partner, what follows the
///   partner is masked as it would be behind a value that ends there (a
///   shell `'\''` to the end of the text: `'passphrase="my"'\''s secret'`).
///   Otherwise the partner itself and what follows show. Inside the quote
///   the mask ends early at a blank followed by a known pair
///   (`"passphrase=a b latency=120"`, see `pair_start`). A mask that ends
///   inside its URL, with shown parts behind it
///   (`"srt://h?passphrase=***&latency=1 x"`), does not run to the partner.
///
/// Not covered, because nothing tells them from prose: a secret behind a
/// name that has no secret ending or with no `=` or `:` between them; an
/// unquoted value with a raw blank (`passphrase=a b`, `rtmp://h/live/a b`),
/// except a password before the last `@` of its line, unless the word
/// before the blank holds an `@` (`srt://u:a@b c@h`) or ends in digits only
/// (`srt://u:12 34@h` reads as a host and port); a path secret in a URL of
/// another scheme than HTTP and RTMP that is shorter than 16 characters or
/// of small letters only (`rtsp://h/abcdef`); a secret value that holds `&`
/// followed by a known name (`passphrase=a&latency=1`), or in quotes a
/// blank followed by a known pair or a `!` (`"passphrase=a latency=1"`).
/// A property value is no prose: [`value`] masks a URL in it whole.
///
/// A bug in here must neither abort strom (bus messages are masked in a
/// GLib callback, where a panic aborts) nor let a secret through: a panic
/// masks the whole text.
pub fn text(s: &str) -> Cow<'_, str> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| mask_text(s)))
        .unwrap_or_else(|_| Cow::Owned(MASK.to_string()))
}

/// [`text`] without the guard against a panic.
fn mask_text(s: &str) -> Cow<'_, str> {
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    let mut word = Word {
        end: 0,
        at_sign: None,
        sep: None,
        colon: None,
    };
    let mut value_next = Next::No;
    let mut quotes = Quotes::default();
    while let Some(c) = s[at..].chars().next() {
        if c.is_whitespace() {
            out.push(c);
            at += c.len_utf8();
            continue;
        }
        if at >= word.end {
            let end = s[at..]
                .find(char::is_whitespace)
                .map_or(s.len(), |i| at + i);
            let at_sign = s[at..end].rfind('@').map(|i| at + i);
            word = Word {
                end,
                at_sign,
                sep: s[at..end].find("://").map(|i| at + i),
                colon: s[at..end].find(':').map(|i| at + i),
            };
        }
        let step = match value_next {
            Next::No => mask_word(s, at, &mut word, &mut quotes, &mut out),
            next => {
                let w = &s[at..word.end];
                let shown_at = out.len();
                out.push_str(MASK);
                // A header value runs to the end of its line, or to a known
                // pair; a quoted one is the quoted run.
                let end = match next == Next::Line && !starts_quoted(w) {
                    true => {
                        let line = quotes.line_end(s, at);
                        quotes.value_end(s, at, line)
                    }
                    false => word.end,
                };
                let scheme = AUTH_SCHEMES.iter().any(|a| w.eq_ignore_ascii_case(a));
                Step {
                    next: value_masked_end(s, at, end),
                    value_next: match scheme && next == Next::Word {
                        true => Next::Word,
                        false => Next::No,
                    },
                    mask: Some((at, shown_at)),
                    bounded: false,
                }
            }
        };
        (at, value_next) = (step.next, step.value_next);
        // Masking began inside an open quote: the secret runs to its partner.
        let Some((from, shown_at)) = step.mask.filter(|_| !step.bounded) else {
            continue;
        };
        let Some(stop) = quotes.masked_to(s, from) else {
            continue;
        };
        out.truncate(shown_at);
        out.push_str(MASK);
        (at, value_next) = match stop {
            // The partner and what follows show.
            Stop::Partner(close, escapes) => {
                out.push_str(&s[close - escapes..=close]);
                (close + 1, Next::No)
            }
            // The value ended at a blank inside the quote.
            Stop::Before(blank) => (blank.max(step.next), Next::No),
            // Never short of what `mask_word` masked.
            Stop::At(stop) => (stop.max(step.next), Next::No),
        };
    }

    if out == s {
        return Cow::Borrowed(s);
    }
    Cow::Owned(out)
}

/// Whether the word after a secret name is its value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    No,
    /// `passphrase= abc`, `Bearer abc`: the next word.
    Word,
    /// `auth-token: a b`: the rest of the line.
    Line,
}

/// What [`mask_word`] did: where to go on, whether the next word is a
/// secret value, where masking began (in `s` and in the output), and
/// whether the mask ended inside a URL with shown parts behind it.
struct Step {
    next: usize,
    value_next: Next,
    mask: Option<(usize, usize)>,
    bounded: bool,
}

impl Step {
    fn new(next: usize, value_next: Next, mask: Option<(usize, usize)>) -> Self {
        Step {
            next,
            value_next,
            mask,
            bounded: false,
        }
    }
}

/// Where a mask that began inside an open quote ends ([`Quotes::masked_to`]).
enum Stop {
    /// Right behind the partner, at this offset with as many backslashes.
    Partner(usize, usize),
    /// At this blank inside the quote, which stays open.
    Before(usize),
    /// At this offset.
    At(usize),
}

/// The quotes open in the text read so far, innermost last: a `"`, `'` or
/// `` ` `` at the start of a word or behind a non-letter opens (an
/// apostrophe in `can't` does not), the same quote with as many backslashes
/// before it closes it and any opened inside it, unless it may not close
/// ([`can_close`]).
#[derive(Default)]
struct Quotes {
    open: Vec<(usize, usize)>,
    /// How far the text is read.
    read: usize,
    /// Built on the first lookup.
    index: Option<Index>,
}

/// Where the quotes that may close, blanks, line ends and `@` of the text
/// stand, so that a lookup is a binary search and not a scan: a quote open
/// over many words is asked for its partner and the word behind it once
/// per URL in them.
struct Index {
    /// Each quote as (quote, backslashes right before it, offset), sorted.
    quotes: Vec<(u8, usize, usize)>,
    blanks: Vec<usize>,
    lines: Vec<usize>,
    at_signs: Vec<usize>,
}

impl Index {
    fn new(s: &str) -> Self {
        let mut index = Index {
            quotes: Vec::new(),
            blanks: Vec::new(),
            lines: Vec::new(),
            at_signs: Vec::new(),
        };
        let mut escapes = 0;
        // The last character that is not a backslash.
        let mut last = None;
        for (i, c) in s.char_indices() {
            match c {
                '"' | '\'' | '`' if closes(last, s[i + 1..].chars().next()) => {
                    index.quotes.push((c as u8, escapes, i))
                }
                '@' => index.at_signs.push(i),
                '\n' => {
                    index.lines.push(i);
                    index.blanks.push(i);
                }
                c if c.is_whitespace() => index.blanks.push(i),
                _ => {}
            }
            escapes = if c == '\\' { escapes + 1 } else { 0 };
            if c != '\\' {
                last = Some(c);
            }
        }
        index.quotes.sort_unstable();
        index
    }

    /// The first offset in `sorted` at or behind `from`.
    fn first(sorted: &[usize], from: usize) -> Option<usize> {
        sorted.get(sorted.partition_point(|&i| i < from)).copied()
    }
}

impl Quotes {
    /// How deep a closing quote looks for its opening one.
    const DEPTH: usize = 16;

    /// Read on to `to`.
    fn read_to(&mut self, s: &str, to: usize) {
        let b = s.as_bytes();
        for i in self.read..to {
            if !matches!(b[i], b'"' | b'\'' | b'`') {
                continue;
            }
            let escapes = backslashes_before(b, 0, i);
            let depth = self.open.len();
            let matching = match can_close(s, i) {
                true => self
                    .open
                    .iter()
                    .rev()
                    .take(Self::DEPTH)
                    .position(|&(o, e)| b[o] == b[i] && e == escapes),
                false => None,
            };
            match matching {
                Some(k) => self.open.truncate(depth - 1 - k),
                None => {
                    let after_letter = s[..i]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_alphanumeric);
                    if !after_letter {
                        self.open.push((i, escapes));
                    }
                }
            }
        }
        self.read = self.read.max(to);
    }

    fn index(&mut self, s: &str) -> &Index {
        self.index.get_or_insert_with(|| Index::new(s))
    }

    /// The partner of the open quote at `open`, as [`partner`] finds it: the
    /// backslashes before a later quote never reach back past `open`.
    fn partner(&mut self, s: &str, open: usize, escapes: usize) -> Option<usize> {
        let key = (s.as_bytes()[open], escapes, open + 1);
        let quotes = &self.index(s).quotes;
        let &(quote, e, close) = quotes.get(quotes.partition_point(|&q| q < key))?;
        (quote == key.0 && e == escapes).then_some(close)
    }

    /// The first `@` at or behind `from`.
    fn at_sign_from(&mut self, s: &str, from: usize) -> Option<usize> {
        Index::first(&self.index(s).at_signs, from)
    }

    /// The last `@` in `from..to`.
    fn last_at_sign(&mut self, s: &str, from: usize, to: usize) -> Option<usize> {
        let at_signs = &self.index(s).at_signs;
        let k = at_signs.partition_point(|&i| i < to);
        at_signs[..k].last().copied().filter(|&a| a >= from)
    }

    /// The end of the line that `at` stands in.
    fn line_end(&mut self, s: &str, at: usize) -> usize {
        Index::first(&self.index(s).lines, at).unwrap_or(s.len())
    }

    /// The end of the word that `close` stands in, and whether an `@`
    /// follows in it.
    fn word_from(&mut self, s: &str, close: usize) -> (usize, bool) {
        let end = Index::first(&self.index(s).blanks, close).unwrap_or(s.len());
        (end, self.at_sign_from(s, close).is_some_and(|a| a < end))
    }

    /// Where a secret value that may hold blanks, from `from`, ends before
    /// `end`: at the first blank followed by a known pair ([`pair_start`]),
    /// or `end`.
    fn value_end(&mut self, s: &str, from: usize, end: usize) -> usize {
        let blanks = &self.index(s).blanks;
        let mut k = blanks.partition_point(|&i| i < from);
        while let Some(&blank) = blanks.get(k) {
            if blank >= end {
                break;
            }
            let start = blank + s[blank..].chars().next().map_or(1, char::len_utf8);
            let stop = blanks.get(k + 1).map_or(s.len(), |&b| b).min(end);
            if start < stop && pair_start(&s[start..stop]) {
                return blank;
            }
            k += 1;
        }
        end
    }

    /// The innermost quote open at `at`, and its backslashes. A quote that
    /// closed in a part skipped as masked is dropped here.
    fn open_at(&mut self, s: &str, at: usize) -> Option<(usize, usize)> {
        self.read_to(s, at);
        while let Some(&(open, escapes)) = self.open.last() {
            match self.partner(s, open, escapes) {
                Some(close) if close < at => {
                    self.open.pop();
                }
                _ => return Some((open, escapes)),
            }
        }
        None
    }

    /// Where a mask that begins at `from` ends when a quote is open there:
    /// at a blank before a known pair inside the quote ([`Quotes::value_end`]),
    /// right behind its partner, or the end of `s`. Past the partner the mask
    /// goes on when a quote opened in the value runs past it (`masked_end`),
    /// to the next such quote when a letter follows (`'it's a'`: the partner
    /// was an apostrophe), and to the end of the partner's word when the
    /// value does not end there ([`ends_value`]: `'a'\''b'`, `"a"'b'`) or an
    /// `@` follows in that word (a password with a quote).
    fn masked_to(&mut self, s: &str, from: usize) -> Option<Stop> {
        let (open, escapes) = self.open_at(s, from)?;
        let b = s.as_bytes();
        let Some(close) = self.partner(s, open, escapes) else {
            self.open.clear();
            self.read = b.len();
            return Some(Stop::At(b.len()));
        };
        let blank = self.value_end(s, from, close);
        if blank < close && masked_end(s, from, from, blank) <= blank {
            self.read = self.read.max(blank);
            return Some(Stop::Before(blank));
        }
        self.open.pop();
        let inner = masked_end(s, from, from, close);
        let (close_end, at_after) = self.word_from(s, close);
        let after = s[close + 1..].chars().next();
        // What follows the partner, read as it would be behind a value
        // that ends at the partner; none when the value ends there.
        let behind = if after.is_some_and(char::is_alphanumeric) {
            // An apostrophe: the value goes on to the next one.
            Some(
                masked_end(s, close, close - escapes, close + 1).max(masked_end(
                    s,
                    close + 1,
                    close + 1,
                    close_end,
                )),
            )
        } else if !ends_value(s, close + 1) || at_after {
            Some(masked_end(s, close + 1, close + 1, close_end))
        } else {
            None
        };
        // A value that runs on past the partner does not excuse what follows
        // the partner: `'passphrase="my"'\''s secret'`.
        let stop = match (inner > close, behind) {
            (true, behind) => Stop::At(inner.max(behind.unwrap_or(0))),
            (false, Some(behind)) => Stop::At(behind),
            (false, None) => Stop::Partner(close, escapes),
        };
        self.read = self.read.max(match stop {
            Stop::At(at) => at,
            _ => close + 1,
        });
        Some(stop)
    }
}

/// The word of [`text`] being masked: where it ends, its last `@`, and its
/// first `://` and `:` from where it is read, looked up again only once
/// passed, so that a long word masked in many places is read once.
struct Word {
    end: usize,
    at_sign: Option<usize>,
    sep: Option<usize>,
    colon: Option<usize>,
}

impl Word {
    /// The first `pat` at or behind `at` in the word, from `cached`.
    fn next(
        s: &str,
        at: usize,
        end: usize,
        cached: &mut Option<usize>,
        pat: &str,
    ) -> Option<usize> {
        if cached.is_some_and(|i| i < at) {
            *cached = s[at..end].find(pat).map(|i| at + i);
        }
        *cached
    }
}

/// Mask the word, or the rest of it, that starts at `at` into `out`. Returns
/// where to go on, and whether the next word is a secret value.
fn mask_word(s: &str, at: usize, word: &mut Word, quotes: &mut Quotes, out: &mut String) -> Step {
    let end = word.end;
    let w = &s[at..end];
    let url = Word::next(s, at, end, &mut word.sep, "://").map(|sep| {
        let sep = sep - at;
        let start = w[..sep]
            .char_indices()
            .rev()
            .find(|&(_, c)| !is_scheme_char(c))
            .map_or(0, |(i, c)| i + c.len_utf8());
        (start, sep)
    });

    let names = url.map_or(w, |(start, _)| &w[..start]);
    if let Some(value) = secret_value(names) {
        out.push_str(&w[..value]);
        if value == w.len() {
            let next = match w.ends_with(':') {
                true => Next::Line,
                false => Next::Word,
            };
            return Step::new(end, next, None);
        }
        let mask = Some((at + value, out.len()));
        out.push_str(MASK);
        // `Authorization:Bearer abc`: the secret is the next word.
        if AUTH_SCHEMES
            .iter()
            .any(|a| w[value..].eq_ignore_ascii_case(a))
        {
            return Step::new(end, Next::Word, mask);
        }
        return Step::new(value_masked_end(s, at + value, end), Next::No, mask);
    }

    let Some((start, sep)) = url else {
        // `user:pass@host`, a URL without `//`.
        let colon = Word::next(s, at, end, &mut word.colon, ":");
        if word
            .at_sign
            .is_some_and(|a| a > at && colon.is_some_and(|c| c < a))
        {
            let mask = Some((at, out.len()));
            out.push_str(MASK);
            return Step::new(masked_end(s, at, at, end), Next::No, mask);
        }
        out.push_str(w);
        return Step::new(end, Next::No, None);
    };
    let prefix = &w[..start];
    let scheme = &w[start..sep];
    if !is_scheme(scheme) || !prefix.chars().all(is_prefix_char) {
        let mask = Some((at, out.len()));
        out.push_str(MASK);
        return Step::new(masked_end(s, at, at, end), Next::No, mask);
    }
    out.push_str(prefix);
    let url_at = at + start;
    let sep = sep - start;
    let rules = Rules::of(scheme);

    // A URL in quotes (`uri="srt://…"`, `"uri=srt://…"`, a quote open since
    // an earlier word) is the quoted run up to the partner of the quote
    // (`"srt://h?a=1","b":2`), or to the end of the partner's word when an
    // `@` follows it there (a password with a quote). Behind a blank the run
    // counts when the URL in this word ends masked
    // (`"srt://h?passphrase=a b"`), up to a blank before a known pair, or an
    // `@` follows before the partner (`"srt://u:1 2@h"`); otherwise a clean
    // URL ends with its word, so a quoted message that starts with one stays
    // readable. A quote without a partner masks all to the end.
    let mut stop = end;
    let mut run = false;
    if let Some((open, escapes)) = quotes.open_at(s, url_at) {
        let Some(close) = quotes.partner(s, open, escapes) else {
            out.push_str(&s[url_at..url_at + sep + 3]);
            let mask = Some((url_at + sep + 3, out.len()));
            out.push_str(MASK);
            return Step::new(s.len(), Next::No, mask);
        };
        let (close_end, at_after) = match close < end {
            true => (end, word.at_sign.is_some_and(|a| a > close)),
            false => quotes.word_from(s, close),
        };
        let at_inside = quotes.at_sign_from(s, end).is_some_and(|a| a < close);
        let tail = || {
            mask_url(
                &s[url_at..url_at + url_len(&s[url_at..end], sep)],
                sep,
                rules,
            )
            .tail
        };
        if at_after {
            stop = close_end;
        } else if close < end || at_inside {
            stop = close - escapes;
        } else if tail() {
            stop = quotes.value_end(s, end, close - escapes);
        }
        // Never before the end of the scheme (a stale quote must not cut
        // the URL backwards).
        stop = stop.max(url_at + sep + 3);
        run = stop > end;
    }

    let part = &s[url_at..stop];
    let url_end = url_len(part, sep);
    let m = mask_url(&part[..url_end], sep, rules);
    let shown_at = out.len();
    out.push_str(&m.shown);
    let Some((from, from_shown)) = m.from else {
        out.push_str(&part[url_end..]);
        return Step::new(stop, Next::No, None);
    };
    let mask = Some((url_at + from, shown_at + from_shown));
    let url_stop = url_at + url_end;
    let mut next = masked_end(s, url_at + from, url_at, url_stop);
    // A password with a raw blank runs to the last `@` in the line, unless
    // the quoted run holds it.
    if m.open_authority && (!run || next > stop) {
        let line = quotes.line_end(s, url_stop);
        if let Some(a) = quotes.last_at_sign(s, url_stop, line).filter(|&a| a > next) {
            next = masked_end(s, next, url_at, a);
        }
    }
    if next > url_stop {
        return Step::new(next, Next::No, mask);
    }
    // A masked end may go on behind the punctuation: drop it.
    if !m.tail {
        out.push_str(&part[url_end..]);
    }
    Step {
        next: stop,
        value_next: Next::No,
        mask,
        bounded: !m.tail,
    }
}

/// The length of the URL at the start of `part` without the punctuation
/// behind it ([`is_trailing`]); `sep` is where its `://` starts.
fn url_len(part: &str, sep: usize) -> usize {
    let bracket = part.contains('[');
    let mut len = part.len();
    while len > sep + 3 && is_trailing(part.as_bytes()[len - 1], bracket) {
        len -= 1;
    }
    len
}

/// Characters that may stand before a URL in a word: `uri=`, `String("`,
/// `<`, `(location:`, `\"`.
fn is_prefix_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '_' | '.' | '-' | '=' | ':' | ',' | '(' | '[' | '{' | '<' | '"' | '\'' | '`' | '\\'
        )
}

/// Punctuation behind a URL that is not part of it: `srt://h:9000:`,
/// `(srt://h)`, `"srt://h",`. A `]` only when the URL has no `[`.
fn is_trailing(b: u8, bracket: bool) -> bool {
    matches!(
        b,
        b':' | b',' | b';' | b'.' | b'!' | b'?' | b')' | b'}' | b'>' | b'"' | b'\'' | b'`' | b'\\'
    ) || (b == b']' && !bracket)
}

/// Where the value of the first secret `name=` or `name:` in `w` starts. The
/// name is the run of name characters before the separator, also in quotes:
/// `"passphrase":`, `\"token\"=`.
fn secret_value(w: &str) -> Option<usize> {
    for (i, c) in w.char_indices() {
        if c != '=' && c != ':' {
            continue;
        }
        let mut name_end = i;
        if let Some(unquoted) = w[..i].strip_suffix(['"', '\'']) {
            name_end = unquoted.trim_end_matches('\\').len();
        }
        let name_start = w[..name_end]
            .char_indices()
            .rev()
            .find(|&(_, c)| !is_name_char(c))
            .map_or(0, |(j, c)| j + c.len_utf8());
        if is_secret_name(&w[name_start..name_end]) {
            return Some(i + 1);
        }
    }
    None
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '%')
}

/// Whether a word starts a new pair behind a secret value that may hold
/// blanks: `name=` or `name:` with a known name or a secret one
/// ([`known_name`]), or a gst-launch link `!`.
fn pair_start(word: &str) -> bool {
    if word == "!" {
        return true;
    }
    let Some(sep) = word.find(['=', ':']) else {
        return false;
    };
    let name = &word[..sep];
    !name.is_empty() && name.chars().all(is_name_char) && known_name(name)
}

/// Whether `w` starts with a quote, behind backslashes.
fn starts_quoted(w: &str) -> bool {
    w.trim_start_matches('\\').starts_with(['"', '\'', '`'])
}

/// The end of a masked secret value from `from` to at least `end`, as
/// [`masked_end`]; a value that starts with a quote is the quoted run
/// through its partner, unless the value goes on there ([`ends_value`]).
fn value_masked_end(s: &str, from: usize, end: usize) -> usize {
    let w = &s[from..];
    match starts_quoted(w) {
        true => {
            let quote = from + w.len() - w.trim_start_matches('\\').len();
            masked_end(s, from, from, quote + 1)
        }
        false => masked_end(s, from, from, end),
    }
}

/// Whether a quoted value ends at `i`, right behind its partner: at the end
/// of the text or a blank, or at `,;)]}` followed by one of these, a quote
/// or a backslash (`"a"}`, `"a", b`, `"a","b"`; not `"a",b`).
fn ends_value(s: &str, i: usize) -> bool {
    let mut chars = s[i..].chars();
    match chars.next() {
        None => true,
        Some(c) if c.is_whitespace() => true,
        Some(c) if ",;)]}".contains(c) => chars
            .next()
            .is_none_or(|d| d.is_whitespace() || ",;)]}\"'`\\".contains(d)),
        _ => false,
    }
}

/// The end of a masked run from `from` to at least `end`: a quote that opens
/// in it masks through its partner, wherever that is, or to the end of `s`,
/// and on to the end of the partner's word unless the value ends there
/// ([`ends_value`]). Backslashes before a quote are counted back to `run_at`.
fn masked_end(s: &str, from: usize, run_at: usize, end: usize) -> usize {
    let b = s.as_bytes();
    let mut end = end;
    let mut i = from;
    // The end of the word that `i` stands in, once looked up.
    let mut word_end = 0;
    while i < end {
        if matches!(b[i], b'"' | b'\'' | b'`') {
            let escapes = backslashes_before(b, run_at, i);
            match partner(s, i, escapes, b.len()) {
                Some(close) => {
                    i = close + 1;
                    end = end.max(i);
                    if !ends_value(s, i) {
                        if i > word_end {
                            word_end = s[i..].find(char::is_whitespace).map_or(s.len(), |k| i + k);
                        }
                        end = end.max(word_end);
                    }
                }
                None => return b.len(),
            }
            continue;
        }
        i += 1;
    }
    end
}

/// How many backslashes stand right before `at`, back to `from`.
fn backslashes_before(b: &[u8], from: usize, at: usize) -> usize {
    b[from..at]
        .iter()
        .rev()
        .take_while(|&&c| c == b'\\')
        .count()
}

/// Whether a quote may close one, with `last` the character before it and
/// its backslashes and `next` the one behind it. A quote that starts a word
/// or a value (at the start of the text, behind a blank or `([{=:,;`) and has
/// a word behind it opens one: in `the '90s … 'passphrase=a b'` and
/// `the '90s uri='srt://…'` the second quote opens.
fn closes(last: Option<char>, next: Option<char>) -> bool {
    let starts_word = last.is_none_or(|c| c.is_whitespace() || "([{=:,;".contains(c));
    let word_follows = next.is_some_and(|c| !c.is_whitespace() && !",;:.!?)]}>\"'`\\".contains(c));
    !(starts_word && word_follows)
}

/// Whether the quote at `i` may close one ([`closes`]).
fn can_close(s: &str, i: usize) -> bool {
    let run = backslashes_before(s.as_bytes(), 0, i);
    closes(s[..i - run].chars().next_back(), s[i + 1..].chars().next())
}

/// The partner of the quote at `open` that has `escapes` backslashes before
/// it: the next same quote with exactly as many (an inner quote is escaped
/// once more) that may close ([`can_close`]), before `end`.
fn partner(s: &str, open: usize, escapes: usize, end: usize) -> Option<usize> {
    let b = s.as_bytes();
    let quote = b[open];
    (open + 1..end).find(|&i| {
        b[i] == quote && backslashes_before(b, open + 1, i) == escapes && can_close(s, i)
    })
}

/// A property value as it may be logged: masked whole when its name is secret
/// (`auth_token`, `passphrase`); a string that is a URL as one URL (blanks in
/// it are no word ends), only its origin when the name ends in `endpoint`
/// (`whip_endpoint`: the path may be the key); any other string as [`text`].
pub fn value<'a>(name: &str, v: &'a PropertyValue) -> Cow<'a, PropertyValue> {
    if is_secret_name(name) {
        return Cow::Owned(PropertyValue::String(MASK.to_string()));
    }
    let PropertyValue::String(s) = v else {
        return Cow::Borrowed(v);
    };
    let trimmed = s.trim();
    let is_url = trimmed
        .find("://")
        .is_some_and(|sep| is_scheme(&trimmed[..sep]));
    let shown = if plain_name(name).ends_with("endpoint") {
        Cow::Owned(origin(trimmed))
    } else if is_url {
        Cow::Owned(url(trimmed))
    } else {
        text(s)
    };
    if shown == s.as_str() {
        return Cow::Borrowed(v);
    }
    Cow::Owned(PropertyValue::String(shown.into_owned()))
}

/// A copy of a flow with every property passed through [`value`], for a
/// `{:?}` dump in a log. Only built when the line is actually emitted.
pub fn flow(flow: &Flow) -> Flow {
    let mut copy = flow.clone();
    for element in &mut copy.elements {
        mask_all(&mut element.properties);
        for props in element.pad_properties.values_mut() {
            mask_all(props);
        }
    }
    for block in &mut copy.blocks {
        mask_all(&mut block.properties);
    }
    copy
}

/// Replace each property value by its loggable form.
fn mask_all(props: &mut std::collections::HashMap<String, PropertyValue>) {
    for (name, v) in props.iter_mut() {
        if let Cow::Owned(masked) = value(name, v) {
            *v = masked;
        }
    }
}

#[cfg(test)]
mod tests;
