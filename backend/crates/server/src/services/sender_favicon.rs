//! Sender favicons from twenty-icons.com (port of `services/senderFavicon.ts`): strict domain
//! normalisation, square-PNG validation, a Redis cache with outcome-specific TTLs, a walk to
//! the registrable parent on a definitive miss, and in-flight deduplication.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine as _;
use fred::prelude::*;
use futures_util::FutureExt;
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};

const PROVIDER_ORIGIN: &str = "https://twenty-icons.com";
const MAX_BYTES: usize = 65_536;
const MAX_DIMENSION: u32 = 64;
const POSITIVE_TTL: i64 = 7 * 24 * 60 * 60;
const DEFINITIVE_TTL: i64 = 6 * 60 * 60;
const TRANSIENT_TTL: i64 = 5 * 60;
const MAX_LABELS: usize = 10;
const TIMEOUT: Duration = Duration::from_secs(5);
const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
const PUBLIC_SUFFIXES: [&str; 32] = [
    "co.uk",
    "org.uk",
    "gov.uk",
    "ac.uk",
    "me.uk",
    "net.uk",
    "sch.uk",
    "co.jp",
    "or.jp",
    "ne.jp",
    "ac.jp",
    "go.jp",
    "com.au",
    "net.au",
    "org.au",
    "edu.au",
    "gov.au",
    "com.br",
    "com.mx",
    "com.ar",
    "com.sg",
    "com.tw",
    "com.cn",
    "com.hk",
    "com.tr",
    "com.ua",
    "co.in",
    "co.za",
    "co.nz",
    "co.kr",
    "co.il",
    "github.io",
];

#[derive(Debug, Clone, PartialEq)]
pub enum Favicon {
    Image(Vec<u8>),
    /// `not-found`, `invalid-image`, `transient` or `cache-unavailable`.
    Miss(&'static str),
}

/// `normalizeSenderDomain`: a lower-case ASCII host name, or `None`.
pub fn normalize_domain(value: &str) -> Option<String> {
    static FORBIDDEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[@/:\\\s?#]").expect("regex"));
    static LABEL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$").expect("regex"));
    let raw = value.trim();
    if raw.is_empty() || FORBIDDEN.is_match(raw) {
        return None;
    }
    let raw = raw.strip_suffix('.').unwrap_or(raw);
    let ascii = match url::Host::parse(raw).ok()? {
        url::Host::Domain(d) => d.to_lowercase(),
        _ => return None,
    };
    if ascii.is_empty() || ascii.len() > 253 || !ascii.contains('.') {
        return None;
    }
    let labels: Vec<&str> = ascii.split('.').collect();
    if labels.len() > MAX_LABELS || labels.iter().any(|l| !LABEL.is_match(l)) {
        return None;
    }
    Some(ascii)
}

/// `validateSquarePng`.
pub fn square_png(b: &[u8]) -> bool {
    if b.len() < 24 || b[..8] != PNG_SIGNATURE || &b[12..16] != b"IHDR" {
        return false;
    }
    let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
    let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
    w >= 1 && w == h && w <= MAX_DIMENSION
}

fn cache_key(domain: &str) -> String {
    format!("sender-favicon:v2:{}", inboxora_core::crypto::sha256_hex(domain))
}

fn next_parent(domain: &str) -> Option<String> {
    let labels: Vec<&str> = domain.split('.').skip(1).collect();
    if labels.len() < 2 {
        return None;
    }
    let candidate = labels.join(".");
    (!PUBLIC_SUFFIXES.contains(&candidate.as_str())).then_some(candidate)
}

fn serialize(r: &Favicon) -> String {
    match r {
        Favicon::Image(b) => {
            json!({ "v": 1, "kind": "image", "pngBase64": base64::engine::general_purpose::STANDARD.encode(b) })
        }
        Favicon::Miss(reason) => json!({ "v": 1, "kind": "miss", "reason": reason }),
    }
    .to_string()
}

fn parse_cached(v: &str) -> Option<Favicon> {
    let e: Value = serde_json::from_str(v).ok()?;
    if e.get("v") != Some(&json!(1)) {
        return None;
    }
    match e.get("kind").and_then(Value::as_str) {
        Some("miss") => match e.get("reason").and_then(Value::as_str) {
            Some("not-found") => Some(Favicon::Miss("not-found")),
            Some("invalid-image") => Some(Favicon::Miss("invalid-image")),
            Some("transient") => Some(Favicon::Miss("transient")),
            _ => None,
        },
        Some("image") => {
            let bytes = base64::engine::general_purpose::STANDARD.decode(e.get("pngBase64")?.as_str()?).ok()?;
            (bytes.len() <= MAX_BYTES && square_png(&bytes)).then_some(Favicon::Image(bytes))
        }
        _ => None,
    }
}

fn ttl(r: &Favicon) -> i64 {
    match r {
        Favicon::Image(_) => POSITIVE_TTL,
        Favicon::Miss("transient") => TRANSIENT_TTL,
        Favicon::Miss(_) => DEFINITIVE_TTL,
    }
}

async fn fetch_provider(domain: &str) -> Favicon {
    let url = format!("{PROVIDER_ORIGIN}/{}/64", inboxora_mail::sanitize::encode_uri_component(domain));
    let Ok(req) = inboxora_net::safe_get(&url, inboxora_net::Guard::default()) else {
        return Favicon::Miss("transient");
    };
    let Ok(response) = req.header("Accept", "image/png").timeout(TIMEOUT).send().await else {
        return Favicon::Miss("transient");
    };
    // `redirect: 'error'`: a redirected answer is not trusted.
    if response.url().as_str() != url {
        return Favicon::Miss("transient");
    }
    let status = response.status().as_u16();
    if status != 200 {
        return if [400, 404, 410].contains(&status) { Favicon::Miss("not-found") } else { Favicon::Miss("transient") };
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_lowercase());
    let declared =
        response.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok());
    if content_type.as_deref() != Some("image/png") || declared.is_some_and(|d| d > MAX_BYTES) {
        return Favicon::Miss("invalid-image");
    }
    let mut stream = response.bytes_stream();
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { return Favicon::Miss("transient") };
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_BYTES {
            return Favicon::Miss("invalid-image");
        }
    }
    if square_png(&bytes) { Favicon::Image(bytes) } else { Favicon::Miss("invalid-image") }
}

type Pending = Shared<BoxFuture<'static, Favicon>>;
static INFLIGHT: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `resolveDomain`: cache, then a shared in-flight resolution.
fn resolve(redis: Pool, domain: String) -> BoxFuture<'static, Favicon> {
    async move {
        let key = cache_key(&domain);
        let cached: Option<String> = match redis.get(&key).await {
            Ok(v) => v,
            Err(_) => return Favicon::Miss("cache-unavailable"),
        };
        if let Some(c) = cached {
            if let Some(r) = parse_cached(&c) {
                return r;
            }
            let _: std::result::Result<i64, _> = redis.del(&key).await;
        }
        let pending = {
            let mut map = INFLIGHT.lock();
            if let Some(p) = map.get(&domain) {
                p.clone()
            } else {
                let (r, d) = (redis.clone(), domain.clone());
                let fut = async move {
                    let out = resolve_with_parents(r, d.clone()).await;
                    INFLIGHT.lock().remove(&d);
                    out
                }
                .boxed()
                .shared();
                map.insert(domain.clone(), fut.clone());
                fut
            }
        };
        pending.await
    }
    .boxed()
}

/// `resolveWithParents`: this domain's own outcome, falling back to its parent's image.
async fn resolve_with_parents(redis: Pool, domain: String) -> Favicon {
    let mut result = fetch_provider(&domain).await;
    if result == Favicon::Miss("not-found")
        && let Some(parent) = next_parent(&domain)
    {
        match resolve(redis.clone(), parent).await {
            img @ Favicon::Image(_) => result = img,
            Favicon::Miss("transient" | "cache-unavailable") => result = Favicon::Miss("transient"),
            Favicon::Miss(_) => {}
        }
    }
    let _: std::result::Result<(), _> =
        redis.set(cache_key(&domain), serialize(&result), Some(Expiration::EX(ttl(&result))), None, false).await;
    result
}

/// `getSenderFavicon`.
pub async fn get(redis: &Pool, domain: &str) -> Favicon {
    match normalize_domain(domain) {
        Some(d) => resolve(redis.clone(), d).await,
        None => Favicon::Miss("invalid-image"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_domains_and_walks_parents() {
        assert_eq!(normalize_domain(" Mail.Example.COM. ").as_deref(), Some("mail.example.com"));
        assert_eq!(normalize_domain("bücher.de").as_deref(), Some("xn--bcher-kva.de"));
        assert_eq!(normalize_domain("1.2.3.4"), None);
        assert_eq!(normalize_domain("localhost"), None);
        assert_eq!(normalize_domain("a@b.c"), None);
        assert_eq!(normalize_domain("-bad.com"), None);
        assert_eq!(normalize_domain("a.a.a.a.a.a.a.a.a.a").as_deref(), Some("a.a.a.a.a.a.a.a.a.a"));
        assert_eq!(normalize_domain("a.a.a.a.a.a.a.a.a.a.a"), None);
        assert_eq!(next_parent("mail.example.co.uk").as_deref(), Some("example.co.uk"));
        assert_eq!(next_parent("example.co.uk"), None);
        assert_eq!(next_parent("example.com"), None);
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&32u32.to_be_bytes());
        png.extend_from_slice(&32u32.to_be_bytes());
        assert!(square_png(&png));
        assert_eq!(parse_cached(&serialize(&Favicon::Image(png.clone()))), Some(Favicon::Image(png)));
        assert_eq!(parse_cached(&serialize(&Favicon::Miss("transient"))), Some(Favicon::Miss("transient")));
    }
}
