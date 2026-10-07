//! Which addresses a URL download may connect to, and the connection that
//! keeps to that decision.
//!
//! The server fetches on the operator's behalf, so a URL must not be a way
//! into the host's own services, the local network or a cloud metadata
//! endpoint. Every hop is resolved here, every resolved address is vetted, and
//! the request then connects to exactly those addresses: reqwest gets a
//! resolver that answers only for the vetted host with the vetted addresses,
//! so nothing is resolved again between the check and the connect. Redirects
//! are not followed by reqwest; each `Location` goes through the same check.

use super::{DownloadError, MediaDownloadSettings};
use futures::future::BoxFuture;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use strom_types::media_download::MEDIA_DOWNLOAD_MAX_REDIRECTS;
use url::{Host, Url};

/// How an address is treated by the download guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressClass {
    /// A public address. Always allowed.
    Global,
    /// Loopback, private (RFC 1918, ULA), link-local or carrier-grade NAT.
    /// Allowed only with `allow_private_addresses`.
    Private,
    /// Never allowed: unspecified, multicast, broadcast, reserved,
    /// documentation, and cloud metadata endpoints.
    Forbidden,
}

/// Cloud instance metadata endpoints. Refused even when private addresses are
/// allowed: they hand out credentials, and no media lives there.
const METADATA_V4: [Ipv4Addr; 3] = [
    Ipv4Addr::new(169, 254, 169, 254), // AWS, GCP, Azure, OpenStack, ...
    Ipv4Addr::new(169, 254, 170, 2),   // AWS ECS task metadata
    Ipv4Addr::new(100, 100, 100, 200), // Alibaba Cloud
];
const METADATA_V6: [Ipv6Addr; 1] = [
    Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254), // AWS IMDS over IPv6
];

/// Classify an address for the download guard.
pub fn classify(ip: IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

/// Whether a download may connect to `ip`.
pub fn is_allowed(ip: IpAddr, allow_private: bool) -> bool {
    match classify(ip) {
        AddressClass::Global => true,
        AddressClass::Private => allow_private,
        AddressClass::Forbidden => false,
    }
}

fn classify_v4(ip: Ipv4Addr) -> AddressClass {
    let [a, b, c, _] = ip.octets();

    if METADATA_V4.contains(&ip)
        || a == 0 // "this network", including 0.0.0.0
        || a >= 224 // multicast 224/4, reserved 240/4, broadcast
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2) // TEST-NET-1
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113) // TEST-NET-3
        || (a == 192 && b == 88 && c == 99)
    // deprecated 6to4 relay anycast
    {
        return AddressClass::Forbidden;
    }

    if a == 127 // loopback
        || a == 10
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 169 && b == 254) // link-local
        || (a == 100 && (64..=127).contains(&b)) // carrier-grade NAT
        || (a == 198 && (b == 18 || b == 19))
    // benchmarking, used in labs
    {
        return AddressClass::Private;
    }

    AddressClass::Global
}

fn classify_v6(ip: Ipv6Addr) -> AddressClass {
    // An IPv4 address carried in IPv6 is judged as the IPv4 address it reaches.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return classify_v4(v4);
    }
    if METADATA_V6.contains(&ip) {
        return AddressClass::Forbidden;
    }

    let s = ip.segments();
    let embedded_v4 =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);

    if ip.is_unspecified() || ip.is_multicast() {
        return AddressClass::Forbidden;
    }
    if ip.is_loopback() {
        return AddressClass::Private;
    }
    // Deprecated IPv4-compatible addresses (::a.b.c.d).
    if s[..6].iter().all(|&x| x == 0) {
        return AddressClass::Forbidden;
    }
    // NAT64 well-known prefix 64:ff9b::/96 reaches the embedded IPv4 address.
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6].iter().all(|&x| x == 0) {
        return classify_v4(embedded_v4(s[6], s[7]));
    }
    // Local-use NAT64 64:ff9b:1::/48.
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0x0001 {
        return AddressClass::Private;
    }
    // 6to4 2002::/16 reaches the IPv4 address in bits 16..48.
    if s[0] == 0x2002 {
        return classify_v4(embedded_v4(s[1], s[2]));
    }
    // Unique local fc00::/7, link-local fe80::/10, deprecated site-local fec0::/10.
    if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xffc0) == 0xfec0 {
        return AddressClass::Private;
    }
    // IETF protocol assignments 2001::/23 (Teredo, ORCHID, ...) and
    // documentation 2001:db8::/32.
    if (s[0] == 0x2001 && s[1] < 0x0200) || (s[0] == 0x2001 && s[1] == 0x0db8) {
        return AddressClass::Forbidden;
    }
    // Everything else outside global unicast 2000::/3 is reserved.
    if (s[0] & 0xe000) != 0x2000 {
        return AddressClass::Forbidden;
    }
    AddressClass::Global
}

/// Resolves a host name to addresses. Swappable so tests can make a
/// public-looking name resolve to a private address without real DNS.
pub trait HostResolver: Send + Sync {
    /// All addresses `host` resolves to.
    fn lookup(&self, host: String, port: u16) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>>;
}

/// The operating system's resolver.
pub struct SystemResolver;

impl HostResolver for SystemResolver {
    fn lookup(&self, host: String, port: u16) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), port)).await?;
            Ok(addrs.map(|a| a.ip()).collect())
        })
    }
}

/// The resolver reqwest uses for one hop: it answers for the vetted host only,
/// with the vetted addresses only. Anything else is an error, so reqwest never
/// falls back to a lookup of its own.
struct PinnedResolver {
    host: String,
    addrs: Vec<SocketAddr>,
}

impl reqwest::dns::Resolve for PinnedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let result = if name.as_str().eq_ignore_ascii_case(&self.host) {
            let addrs: reqwest::dns::Addrs = Box::new(self.addrs.clone().into_iter());
            Ok(addrs)
        } else {
            Err(format!("{} was not vetted for this download", name.as_str()).into())
        };
        Box::pin(std::future::ready(result))
    }
}

/// Check one URL and return the addresses its request may connect to.
async fn vet(
    url: &Url,
    settings: &MediaDownloadSettings,
    resolver: &dyn HostResolver,
) -> Result<Vec<IpAddr>, DownloadError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(DownloadError::BadRequest(format!(
            "Only http and https URLs can be downloaded, not {}",
            url.scheme()
        )));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| DownloadError::BadRequest("URL has no port".to_string()))?;

    let addrs = match url.host() {
        Some(Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
        Some(Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
        Some(Host::Domain(domain)) => {
            let lookup = resolver.lookup(domain.to_string(), port);
            match tokio::time::timeout(settings.connect_timeout, lookup).await {
                Ok(Ok(addrs)) => addrs,
                Ok(Err(e)) => {
                    return Err(DownloadError::Upstream(format!(
                        "Could not resolve {domain}: {e}"
                    )))
                }
                Err(_) => {
                    return Err(DownloadError::Timeout(format!(
                        "Resolving {domain} timed out"
                    )))
                }
            }
        }
        None => return Err(DownloadError::BadRequest("URL has no host".to_string())),
    };

    if addrs.is_empty() {
        return Err(DownloadError::Upstream(format!(
            "{} resolved to no addresses",
            url.host_str().unwrap_or_default()
        )));
    }
    // Refuse the host if any of its addresses is refused: picking the allowed
    // ones would let a name mix a public address with a private one.
    if let Some(refused) = addrs
        .iter()
        .find(|ip| !is_allowed(**ip, settings.allow_private_addresses))
    {
        let hint = match classify(*refused) {
            AddressClass::Private => {
                " (private and local addresses are off; see media.download_allow_private_addresses)"
            }
            _ => "",
        };
        let target = match url.host() {
            Some(Host::Domain(domain)) => format!("{domain} resolves to {refused}, which"),
            _ => refused.to_string(),
        };
        return Err(DownloadError::Forbidden(format!(
            "{target} is an address downloads may not connect to{hint}"
        )));
    }
    Ok(addrs)
}

/// Send a GET for `url`, following redirects, with every hop vetted and the
/// connection pinned to the vetted addresses. Returns the response to the
/// last hop (headers only; the body is still to be read) and its URL.
pub(crate) async fn open(
    url: Url,
    settings: &MediaDownloadSettings,
    resolver: &dyn HostResolver,
) -> Result<(reqwest::Response, Url), DownloadError> {
    let mut current = url;
    for _ in 0..=MEDIA_DOWNLOAD_MAX_REDIRECTS {
        let addrs = vet(&current, settings, resolver).await?;
        let pinned = PinnedResolver {
            host: current.host_str().unwrap_or_default().to_string(),
            addrs: addrs.iter().map(|ip| SocketAddr::new(*ip, 0)).collect(),
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // A proxy would make the connection go somewhere else than the
            // address vetted here.
            .no_proxy()
            .dns_resolver(Arc::new(pinned))
            .connect_timeout(settings.connect_timeout)
            .read_timeout(settings.read_timeout)
            .timeout(settings.total_timeout)
            .user_agent(concat!("Strom/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| DownloadError::Internal(format!("HTTP client: {e}")))?;

        let send = client.get(current.clone()).send();
        let response = match tokio::time::timeout(settings.response_timeout, send).await {
            Ok(Ok(response)) => response,
            Ok(Err(e)) if e.is_timeout() => {
                return Err(DownloadError::Timeout(format!(
                    "{} did not answer in time",
                    super::filename::display_url(&current)
                )))
            }
            Ok(Err(e)) => {
                return Err(DownloadError::Upstream(format!(
                    "Request to {} failed: {}",
                    super::filename::display_url(&current),
                    error_chain(&e)
                )))
            }
            Err(_) => {
                return Err(DownloadError::Timeout(format!(
                    "{} did not answer in time",
                    super::filename::display_url(&current)
                )))
            }
        };

        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    DownloadError::Upstream(format!(
                        "Redirect ({}) without a Location header",
                        response.status()
                    ))
                })?;
            current = current
                .join(location)
                .map_err(|e| DownloadError::Upstream(format!("Redirect to an invalid URL: {e}")))?;
            continue;
        }
        return Ok((response, current));
    }
    Err(DownloadError::Upstream(format!(
        "More than {MEDIA_DOWNLOAD_MAX_REDIRECTS} redirects"
    )))
}

/// The error with its causes, which is where reqwest keeps the useful part.
pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> AddressClass {
        classify(s.parse().unwrap())
    }

    #[test]
    fn public_ipv4_is_global() {
        for addr in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "100.63.255.255",
            "172.32.0.1",
        ] {
            assert_eq!(v4(addr), AddressClass::Global, "{addr}");
        }
    }

    #[test]
    fn local_ipv4_is_private() {
        for addr in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "198.18.0.1",
        ] {
            assert_eq!(v4(addr), AddressClass::Private, "{addr}");
        }
    }

    #[test]
    fn unroutable_and_metadata_ipv4_is_forbidden() {
        for addr in [
            "0.0.0.0",
            "0.1.2.3",
            "169.254.169.254",
            "169.254.170.2",
            "100.100.100.200",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
            "192.0.2.10",
            "198.51.100.1",
            "203.0.113.5",
            "192.0.0.8",
        ] {
            assert_eq!(v4(addr), AddressClass::Forbidden, "{addr}");
        }
    }

    #[test]
    fn ipv6_classes() {
        let cases = [
            ("2606:4700::1111", AddressClass::Global),
            ("2a00:1450:4001::1", AddressClass::Global),
            ("::1", AddressClass::Private),
            ("fc00::1", AddressClass::Private),
            ("fd12:3456::1", AddressClass::Private),
            ("fe80::1", AddressClass::Private),
            ("64:ff9b:1::1", AddressClass::Private),
            ("::", AddressClass::Forbidden),
            ("ff02::1", AddressClass::Forbidden),
            ("2001:db8::1", AddressClass::Forbidden),
            ("2001::1", AddressClass::Forbidden), // Teredo
            ("fd00:ec2::254", AddressClass::Forbidden),
            ("::7f00:1", AddressClass::Forbidden), // IPv4-compatible
            ("100::1", AddressClass::Forbidden),
        ];
        for (addr, class) in cases {
            assert_eq!(v4(addr), class, "{addr}");
        }
    }

    #[test]
    fn ipv4_inside_ipv6_is_judged_as_ipv4() {
        // IPv4-mapped
        assert_eq!(v4("::ffff:127.0.0.1"), AddressClass::Private);
        assert_eq!(v4("::ffff:10.1.2.3"), AddressClass::Private);
        assert_eq!(v4("::ffff:169.254.169.254"), AddressClass::Forbidden);
        assert_eq!(v4("::ffff:8.8.8.8"), AddressClass::Global);
        // NAT64
        assert_eq!(v4("64:ff9b::a00:1"), AddressClass::Private);
        assert_eq!(v4("64:ff9b::808:808"), AddressClass::Global);
        // 6to4
        assert_eq!(v4("2002:c0a8:0101::1"), AddressClass::Private);
        assert_eq!(v4("2002:a9fe:a9fe::1"), AddressClass::Forbidden);
        assert_eq!(v4("2002:0808:0808::1"), AddressClass::Global);
    }

    #[test]
    fn allow_private_unlocks_private_only() {
        let private: IpAddr = "192.168.0.10".parse().unwrap();
        let metadata: IpAddr = "169.254.169.254".parse().unwrap();
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        assert!(!is_allowed(private, false));
        assert!(is_allowed(private, true));
        assert!(!is_allowed(metadata, true));
        assert!(is_allowed(public, false));
    }
}
