//! Outbound network policy: private-address validation and an SSRF-guarded HTTP client
//! (ports of `hostValidation.ts` and `safeFetch.ts`).
//!
//! The guarded client validates every connection it makes — including redirect hops — so a
//! public URL cannot redirect into the internal network: hostnames are resolved by a custom
//! resolver that rejects private or reserved addresses, and IP-literal hosts (which bypass
//! resolvers) are checked before the request and on every redirect.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum NetError {
    #[error("Invalid URL")]
    InvalidUrl,
    #[error("Only http(s) URLs are allowed")]
    UnsupportedScheme,
    #[error("Plaintext HTTP is not allowed for this request")]
    InsecureTransport,
    #[error("Host resolves to a private or reserved IP address")]
    BlockedPrivateIp,
}

impl NetError {
    /// The Node error `code`.
    pub fn code(&self) -> &'static str {
        match self {
            NetError::InvalidUrl => "ERR_INVALID_URL",
            NetError::UnsupportedScheme => "ERR_UNSUPPORTED_SCHEME",
            NetError::InsecureTransport => "ERR_INSECURE_TRANSPORT",
            NetError::BlockedPrivateIp => "ERR_BLOCKED_PRIVATE_IP",
        }
    }
}

fn v4_in(ip: Ipv4Addr, base: [u8; 4], bits: u32) -> bool {
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    (u32::from(ip) & mask) == (u32::from(Ipv4Addr::from(base)) & mask)
}

pub fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    v4_in(ip, [0, 0, 0, 0], 8)
        || v4_in(ip, [10, 0, 0, 0], 8)
        || v4_in(ip, [100, 64, 0, 0], 10)
        || v4_in(ip, [127, 0, 0, 0], 8)
        || v4_in(ip, [169, 254, 0, 0], 16)
        || v4_in(ip, [172, 16, 0, 0], 12)
        || v4_in(ip, [192, 0, 0, 0], 24)
        || v4_in(ip, [192, 168, 0, 0], 16)
        || v4_in(ip, [198, 18, 0, 0], 15)
        || v4_in(ip, [240, 0, 0, 0], 4)
        || ip == Ipv4Addr::BROADCAST
}

pub fn is_private_ipv6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    if ip == Ipv6Addr::LOCALHOST {
        return true;
    }
    // fc00::/7 (string prefixes "fc"/"fd") and fe80:: link-local ("fe80" string prefix).
    let first = seg[0];
    if (first & 0xff00) == 0xfc00 || (first & 0xff00) == 0xfd00 || first == 0xfe80 {
        return true;
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_private_ipv4(v4);
    }
    // 6to4 (2002::/16) embedding a private IPv4 address.
    if first == 0x2002 {
        let embedded = Ipv4Addr::new((seg[1] >> 8) as u8, seg[1] as u8, (seg[2] >> 8) as u8, seg[2] as u8);
        if is_private_ipv4(embedded) {
            return true;
        }
    }
    // 2001:0::/32 (Teredo) — the Node check is `^2001:0{1,4}:`.
    first == 0x2001 && seg[1] == 0
}

pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_ipv4(v4),
        IpAddr::V6(v6) => is_private_ipv6(v6),
    }
}

/// `validateHostLiteral`: an error message when the host is a local name or a private literal.
pub fn validate_host_literal(host: &str, allow_private: bool) -> Option<&'static str> {
    if host.is_empty() || allow_private {
        return None;
    }
    let h = host.trim().to_lowercase();
    if h == "localhost" || h.ends_with(".local") || h.ends_with(".localhost") || h.ends_with(".internal") {
        return Some("Host cannot be a local address");
    }
    let bare = h.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(&h);
    match bare.parse::<IpAddr>() {
        Ok(ip) if is_private_ip(ip) => Some("Host cannot be a private or reserved IP address"),
        _ => None,
    }
}

/// `validateHost`: literal check, then every resolved address.
pub async fn validate_host(host: &str, allow_private: bool) -> Option<&'static str> {
    if let Some(e) = validate_host_literal(host, allow_private) {
        return Some(e);
    }
    let h = host.trim().to_lowercase();
    let bare = h.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(&h).to_owned();
    if bare.parse::<IpAddr>().is_ok() || allow_private {
        return None;
    }
    let addrs = resolve_all(&bare).await;
    addrs.iter().any(|a| is_private_ip(*a)).then_some("Host resolves to a private or reserved IP address")
}

/// All A/AAAA addresses for a name (empty on failure).
pub async fn resolve_all(host: &str) -> Vec<IpAddr> {
    match tokio::net::lookup_host((host, 0)).await {
        Ok(it) => {
            let mut out: Vec<IpAddr> = Vec::new();
            for a in it {
                if !out.contains(&a.ip()) {
                    out.push(a.ip());
                }
            }
            out
        }
        Err(_) => Vec::new(),
    }
}

/// `resolveForConnection`: validated, pinned addresses for a mail/DAV connection. `Ok(None)`
/// means "connect by name" (an IP literal, or nothing resolved).
pub async fn resolve_for_connection(host: &str, allow_private: bool) -> Result<Option<Vec<IpAddr>>, String> {
    if let Some(e) = validate_host_literal(host, allow_private) {
        return Err(e.to_owned());
    }
    let h = host.trim();
    let bare = h.strip_prefix('[').and_then(|x| x.strip_suffix(']')).map_or_else(|| h.to_lowercase(), str::to_owned);
    if bare.parse::<IpAddr>().is_ok() {
        return Ok(None);
    }
    let addrs = resolve_all(&bare).await;
    if !allow_private && addrs.iter().any(|a| is_private_ip(*a)) {
        return Err("Host resolves to a private or reserved IP address".into());
    }
    Ok(if addrs.is_empty() { None } else { Some(addrs) })
}

struct GuardedResolver {
    allow_private: bool,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if !allow_private && addrs.iter().any(|a| is_private_ip(a.ip())) {
                return Err(Box::new(NetError::BlockedPrivateIp) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn check_url(url: &url::Url, allow_private: bool, require_https: bool) -> Result<(), NetError> {
    match url.scheme() {
        "https" => {}
        "http" if !require_https => {}
        "http" => return Err(NetError::InsecureTransport),
        _ => return Err(NetError::UnsupportedScheme),
    }
    let literal = match url.host() {
        Some(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        _ => None,
    };
    if !allow_private && literal.is_some_and(is_private_ip) {
        return Err(NetError::BlockedPrivateIp);
    }
    Ok(())
}

fn build(allow_private: bool, require_https: bool) -> reqwest::Client {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(GuardedResolver { allow_private }))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 20 {
                return attempt.error("redirect count exceeded");
            }
            match check_url(attempt.url(), allow_private, require_https) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        }))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .expect("TLS backend available")
}

static CLIENTS: LazyLock<[reqwest::Client; 3]> =
    LazyLock::new(|| [build(false, true), build(true, false), build(false, false)]);

/// Guard options (`safeFetch`'s third argument).
#[derive(Debug, Clone, Copy, Default)]
pub struct Guard {
    pub allow_private: bool,
    /// Defaults to `!allow_private` like the Node helper.
    pub require_https: Option<bool>,
}

/// A request builder on the guarded client, after validating the initial URL.
pub fn safe_request(method: reqwest::Method, raw_url: &str, guard: Guard) -> Result<reqwest::RequestBuilder, NetError> {
    let url = url::Url::parse(raw_url).map_err(|_| NetError::InvalidUrl)?;
    let require_https = guard.require_https.unwrap_or(!guard.allow_private);
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(NetError::UnsupportedScheme);
    }
    check_url(&url, guard.allow_private, require_https)?;
    let client = match (guard.allow_private, require_https) {
        (false, true) => &CLIENTS[0],
        (true, _) => &CLIENTS[1],
        (false, false) => &CLIENTS[2],
    };
    Ok(client.request(method, url))
}

/// `safeFetch(url)` GET.
pub fn safe_get(raw_url: &str, guard: Guard) -> Result<reqwest::RequestBuilder, NetError> {
    safe_request(reqwest::Method::GET, raw_url, guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges_match_node() {
        for ip in [
            "10.1.2.3",
            "127.0.0.1",
            "169.254.169.254",
            "172.31.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "198.19.0.1",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "1.1.1.1"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1", "2002:0a00:0001::1", "2001:0:1::1"] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(!is_private_ip("2606:4700::1111".parse().unwrap()));
        assert_eq!(validate_host_literal("printer.local", false), Some("Host cannot be a local address"));
        assert_eq!(validate_host_literal("[::1]", false), Some("Host cannot be a private or reserved IP address"));
        assert_eq!(validate_host_literal("10.0.0.1", true), None);
    }

    #[test]
    fn urls_are_checked_before_sending() {
        assert_eq!(safe_get("ftp://x.test/", Guard::default()).unwrap_err(), NetError::UnsupportedScheme);
        assert_eq!(safe_get("http://x.test/", Guard::default()).unwrap_err(), NetError::InsecureTransport);
        assert_eq!(safe_get("https://127.0.0.1/", Guard::default()).unwrap_err(), NetError::BlockedPrivateIp);
        assert!(safe_get("http://127.0.0.1/", Guard { allow_private: true, require_https: None }).is_ok());
        assert_eq!(safe_get("not a url", Guard::default()).unwrap_err(), NetError::InvalidUrl);
    }

    #[tokio::test]
    async fn resolver_blocks_names_pointing_inside() {
        let err = safe_get("https://localhost:9/", Guard::default()).unwrap().send().await.unwrap_err();
        let chain = format!("{err:?}");
        assert!(chain.contains("BlockedPrivateIp"), "{chain}");
    }
}
