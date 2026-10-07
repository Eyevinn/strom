//! Naming a downloaded file: from `Content-Disposition`, from the URL, and
//! made safe to create inside the media directory.

use url::Url;

/// Longest file name a download is saved under, in bytes.
pub const MAX_FILENAME_BYTES: usize = 200;

/// Name used when neither the response nor the URL offers one.
pub const FALLBACK_FILENAME: &str = "download";

/// Prefix and suffix of the temporary file a download or an upload is written
/// to before it is renamed into place. The media listing hides these.
pub const TEMP_PREFIX: &str = ".download-";
pub const TEMP_SUFFIX: &str = ".part";

/// Whether `name` is a download's or an upload's temporary file.
pub fn is_temp_file(name: &str) -> bool {
    name.starts_with(TEMP_PREFIX) && name.ends_with(TEMP_SUFFIX)
}

/// Make `raw` a plain file name that is safe to create in the media directory.
///
/// Keeps only the last path component, drops control characters, replaces
/// characters Windows does not allow, strips leading dots (no hidden files, no
/// `..`) and trailing dots and spaces, guards Windows device names, and caps
/// the length while keeping the extension. Returns `None` when nothing usable
/// is left.
pub fn sanitize_filename(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();

    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim();
    if trimmed.is_empty() {
        return None;
    }

    let stem = trimmed
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    let name = if reserved {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };

    Some(truncate_keeping_extension(&name, MAX_FILENAME_BYTES))
}

fn truncate_keeping_extension(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 16 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut end = max.saturating_sub(ext.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &stem[..end], ext)
}

/// The file name a `Content-Disposition` header offers, if any, undecoded
/// path parts included (sanitize the result). `filename*` (RFC 5987/6266)
/// wins over `filename`.
pub fn content_disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;

    for param in split_params(value) {
        let Some((key, val)) = param.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim();
        if key == "filename*" {
            let val = val.trim_matches('"');
            if let Some((charset, rest)) = val.split_once('\'') {
                if let Some((_lang, encoded)) = rest.split_once('\'') {
                    if charset.eq_ignore_ascii_case("utf-8") {
                        extended = urlencoding::decode(encoded).ok().map(|s| s.into_owned());
                    }
                }
            }
        } else if key == "filename" {
            plain = Some(unquote(val));
        }
    }
    extended.or(plain).filter(|s| !s.is_empty())
}

/// Split header parameters on `;`, leaving quoted strings intact.
fn split_params(value: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => {
                current.push(c);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ';' if !quoted => params.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    params.push(current);
    params
}

fn unquote(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The URL's last non-empty path segment, percent-decoded.
pub fn url_filename(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.rfind(|s| !s.is_empty())?;
    urlencoding::decode(segment).ok().map(|s| s.into_owned())
}

/// The URL as shown to operators and written to logs: no user info, query
/// or fragment, where signed URLs keep their secrets.
pub fn display_url(url: &Url) -> String {
    let mut shown = url.clone();
    let _ = shown.set_username("");
    let _ = shown.set_password(None);
    shown.set_query(None);
    shown.set_fragment(None);
    shown.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_ordinary_names() {
        assert_eq!(
            sanitize_filename("stinger.mov").as_deref(),
            Some("stinger.mov")
        );
        assert_eq!(
            sanitize_filename("Intro clip (v2).mp4").as_deref(),
            Some("Intro clip (v2).mp4")
        );
        assert_eq!(
            sanitize_filename("räksmörgås.wav").as_deref(),
            Some("räksmörgås.wav")
        );
    }

    #[test]
    fn sanitize_drops_path_parts_and_traversal() {
        assert_eq!(
            sanitize_filename("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            sanitize_filename("..\\..\\boot.ini").as_deref(),
            Some("boot.ini")
        );
        assert_eq!(
            sanitize_filename("/abs/path/clip.mp4").as_deref(),
            Some("clip.mp4")
        );
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("."), None);
        assert_eq!(sanitize_filename("dir/"), None);
        assert_eq!(sanitize_filename(""), None);
    }

    #[test]
    fn sanitize_strips_hidden_control_and_reserved() {
        assert_eq!(sanitize_filename(".bashrc").as_deref(), Some("bashrc"));
        assert_eq!(
            sanitize_filename("a\u{0}b\nc.mp4").as_deref(),
            Some("abc.mp4")
        );
        assert_eq!(sanitize_filename("what?.mp4").as_deref(), Some("what_.mp4"));
        assert_eq!(
            sanitize_filename("clip.mp4. . ").as_deref(),
            Some("clip.mp4")
        );
        assert_eq!(sanitize_filename("CON.mp4").as_deref(), Some("_CON.mp4"));
        assert_eq!(sanitize_filename("com1").as_deref(), Some("_com1"));
        assert_eq!(
            sanitize_filename("console.mp4").as_deref(),
            Some("console.mp4")
        );
    }

    #[test]
    fn sanitize_caps_length_and_keeps_extension() {
        let long = format!("{}.mp4", "å".repeat(300));
        let name = sanitize_filename(&long).unwrap();
        assert!(name.len() <= MAX_FILENAME_BYTES, "{}", name.len());
        assert!(name.ends_with(".mp4"));
    }

    #[test]
    fn content_disposition_forms() {
        assert_eq!(
            content_disposition_filename("attachment; filename=\"clip.mp4\"").as_deref(),
            Some("clip.mp4")
        );
        assert_eq!(
            content_disposition_filename("attachment; filename=clip.mp4").as_deref(),
            Some("clip.mp4")
        );
        assert_eq!(
            content_disposition_filename(
                "attachment; filename=\"fallback.mp4\"; filename*=UTF-8''r%C3%A4k%20s.mp4"
            )
            .as_deref(),
            Some("räk s.mp4")
        );
        assert_eq!(
            content_disposition_filename("attachment; filename=\"a;b \\\"q\\\".mp4\"").as_deref(),
            Some("a;b \"q\".mp4")
        );
        assert_eq!(content_disposition_filename("inline"), None);
    }

    #[test]
    fn url_filename_and_display() {
        let url =
            Url::parse("https://user:pw@cdn.example.com/a/my%20clip.mp4?sig=secret#t=1").unwrap();
        assert_eq!(url_filename(&url).as_deref(), Some("my clip.mp4"));
        assert_eq!(display_url(&url), "https://cdn.example.com/a/my%20clip.mp4");
        let dir = Url::parse("https://cdn.example.com/stingers/").unwrap();
        assert_eq!(url_filename(&dir).as_deref(), Some("stingers"));
        let root = Url::parse("https://cdn.example.com/").unwrap();
        assert_eq!(url_filename(&root), None);
    }

    #[test]
    fn temp_files_are_recognised() {
        assert!(is_temp_file(".download-1234.part"));
        assert!(!is_temp_file("download-1234.part"));
        assert!(!is_temp_file(".download-1234.mp4"));
    }
}
