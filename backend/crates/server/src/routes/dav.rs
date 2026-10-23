//! Plumbing shared by the CardDAV and CalDAV servers (ports of the router preambles in
//! carddav.ts/caldav.ts and of davServerAuth.ts): the per-IP request budget, Basic
//! authentication with DAV application passwords, Express-style path matching and the
//! RFC 6764 well-known redirects.

use axum::body::Body as AxumBody;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use inboxora_core::Error;

use crate::http::middleware::RoutePattern;
use crate::routes::util::empty;
use crate::services::auth;
use crate::state::AppState;

/// Requests per IP per auth-limiter window.
const MAX_REQUESTS: u64 = 500;
/// Raw request bodies are read whole; Node reads them unbounded, this caps them.
const BODY_LIMIT: usize = 16 * 1024 * 1024;

/// The in-memory per-IP request budget (`cardavRateLimit` / `caldavRateLimit`).
pub struct Buckets(crate::http::limit::Windows);

impl Default for Buckets {
    fn default() -> Self {
        Self(crate::http::limit::Windows::new(10_000))
    }
}

impl Buckets {
    /// `Some(429)` once the IP has used its budget for the current window.
    pub fn check(&self, ip: &str) -> Option<Response> {
        let (hits, left) = self.0.hit(ip, auth::auth_limits().window);
        (hits > MAX_REQUESTS).then(|| {
            let mut res = empty(429);
            res.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(left.as_millis().div_ceil(1000) as u64));
            res
        })
    }
}

/// `createDavAuthMiddleware`: the authenticated user id, or the rejection to send.
pub async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    ip: &str,
    realm: &str,
    event_type: &str,
) -> Result<String, Response> {
    let reject = async |username: Option<&str>| {
        let limits = auth::auth_limits();
        let (limited, _) =
            auth::rate_consume(&state.redis, &format!("auth:{ip}"), limits.max_requests, limits.window).await;
        auth::log_auth_event(&state.db, event_type, username, None, Some(ip), false);
        let mut res = empty(if limited { 429 } else { 401 });
        if let Ok(v) = HeaderValue::from_str(&format!("Basic realm=\"{realm}\"")) {
            res.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
        res
    };
    // Node reads header bytes as latin1; only the base64 payload matters here.
    let authorization: String = headers
        .get(header::AUTHORIZATION)
        .map(|v| v.as_bytes().iter().map(|&b| b as char).collect())
        .unwrap_or_default();
    let Some(encoded) = authorization.strip_prefix("Basic ") else { return Err(reject(None).await) };
    let decoded = String::from_utf8_lossy(&inboxora_mail::headers::base64_lenient(encoded)).into_owned();
    let Some((username, password)) = decoded.split_once(':') else { return Err(reject(None).await) };
    match auth::authenticate_dav(&state.db, username, password).await {
        Ok(Some((user_id, _credential_id))) => Ok(user_id),
        Ok(None) => Err(reject(Some(username)).await),
        Err(e) => {
            tracing::error!("DAV authentication error: {e}");
            Err(empty(500))
        }
    }
}

/// JS `decodeURIComponent`: `None` where it would throw a URIError.
pub fn decode_uri_component(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_owned());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = |c: u8| (c as char).to_digit(16);
            let (Some(h), Some(l)) = (b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c))) else {
                return None;
            };
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Express (non-strict) matching of the path below a DAV mount: the decoded segments of
/// `/`, `/:a/`, `/:a/:b/` or `/:a/:b/:c` (one trailing slash optional). `Err` mirrors the
/// router's parameter-decoding failure, which Express turns into an unhandled error.
pub fn segments(rest: &str) -> Result<Option<Vec<String>>, Error> {
    if decode_uri_component(rest).is_none() {
        return Err(Error::Other(anyhow::anyhow!("Failed to decode param '{rest}'")));
    }
    if rest.is_empty() || rest == "/" {
        return Ok(Some(Vec::new()));
    }
    let Some(inner) = rest.strip_prefix('/') else { return Ok(None) };
    let inner = inner.strip_suffix('/').unwrap_or(inner);
    let parts: Vec<&str> = inner.split('/').collect();
    if parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return Ok(None);
    }
    Ok(Some(parts.into_iter().filter_map(decode_uri_component).collect()))
}

/// The request path below `mount` (`req.path` inside the router).
pub fn rest_of<'a>(req: &'a Request, mount: &str) -> &'a str {
    req.uri().path().strip_prefix(mount).unwrap_or("")
}

/// A case-insensitive method comparison (Express lower-cases route methods).
pub fn is(method: &Method, name: &str) -> bool {
    method.as_str().eq_ignore_ascii_case(name)
}

/// A header as Node exposes it (`req.headers[name]`), or `None` when absent.
pub fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The raw body as UTF-8 text (`req.setEncoding('utf8')` replaces invalid sequences).
pub async fn raw_body(req: Request) -> Result<String, Response> {
    match crate::http::body::read_limited(req, BODY_LIMIT).await {
        Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        Err(e) => Err(empty(if matches!(e, Error::Http { status, .. } if status == StatusCode::PAYLOAD_TOO_LARGE) {
            413
        } else {
            400
        })),
    }
}

/// `res.set({ 'Content-Type': type, ETag }).send(body)` (Express re-formats the charset).
pub fn send_with_etag(content_type: &'static str, etag: Option<&str>, body: Option<String>) -> Response {
    let mut res = Response::new(AxumBody::from(body.unwrap_or_default()));
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", etag.unwrap_or("null"))) {
        h.insert(header::ETAG, v);
    }
    res
}

/// A non-empty request header (`req.headers[name]` is falsy when empty).
pub fn present<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    header_str(headers, name).filter(|v| !v.is_empty())
}

pub fn with_etag(status: u16, etag: &str) -> Response {
    let mut res = empty(status);
    if let Ok(v) = HeaderValue::from_str(&format!("\"{etag}\"")) {
        res.headers_mut().insert(header::ETAG, v);
    }
    res
}

/// `xmlEscape(value)`: `String(value || '')` with the four XML specials escaped.
pub fn xml_escape(s: Option<&str>) -> String {
    let s = s.unwrap_or("");
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// `sendXml(res, status, body)`.
pub fn send_xml(status: u16, body: String) -> Response {
    let mut res = Response::new(AxumBody::from(body));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml; charset=utf-8"));
    res
}

/// Tag a response with the Express route pattern it was served by (for request metrics).
pub fn tagged(mut res: Response, method: &Method, pattern: &str) -> Response {
    res.extensions_mut().insert(RoutePattern(format!("{} {pattern}", method.as_str().to_ascii_uppercase())));
    res
}

/// `res.redirect(308, location)` for `app.all('/.well-known/…')`.
pub fn permanent_redirect(req: &Request, location: &'static str) -> Response {
    express_redirect(req.headers(), req.method(), StatusCode::PERMANENT_REDIRECT, location)
}

/// `encodeurl(url)`: percent-encode characters that may not appear in a URL, keeping
/// existing valid `%XX` escapes.
pub fn encode_url(url: &str) -> String {
    let b = url.as_bytes();
    let mut out = String::with_capacity(url.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let allowed = matches!(c, 0x21 | 0x23..=0x3B | 0x3D | 0x3F..=0x5F | 0x61..=0x7A | 0x7C | 0x7E);
        if c == b'%'
            && b.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
            && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            out.push('%');
            i += 1;
            continue;
        }
        if allowed {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
        i += 1;
    }
    out
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Express 4 `res.redirect([status,] url)`: Location plus a negotiated text/HTML body.
pub fn express_redirect(headers: &HeaderMap, method: &Method, status: StatusCode, location: &str) -> Response {
    let address = encode_url(location);
    let reason = status.canonical_reason().unwrap_or("");
    let kind = negotiate_text_or_html(header_str(headers, "accept"));
    let body = match kind {
        Some("text/plain") => format!("{reason}. Redirecting to {address}"),
        Some(_) => format!("<p>{reason}. Redirecting to {}</p>", escape_html(&address)),
        None => String::new(),
    };
    let len = body.len();
    let mut res =
        if *method == Method::HEAD { Response::new(AxumBody::empty()) } else { Response::new(AxumBody::from(body)) };
    *res.status_mut() = status;
    let h = res.headers_mut();
    h.insert(header::VARY, HeaderValue::from_static("Accept"));
    if let Some(k) = kind {
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(if k == "text/plain" {
                "text/plain; charset=utf-8"
            } else {
                "text/html; charset=utf-8"
            }),
        );
    }
    if let Ok(v) = HeaderValue::from_str(&address) {
        h.insert(header::LOCATION, v);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    res.extensions_mut().insert(crate::http::middleware::NoEtag);
    res.into_response()
}

/// `req.accepts(['text', 'html'])` (negotiator media-type preference): the chosen type, or
/// `None` when neither is acceptable.
fn negotiate_text_or_html(accept: Option<&str>) -> Option<&'static str> {
    let Some(accept) = accept else { return Some("text/plain") };
    // (q, specificity, accept index) of the best spec matching each provided type.
    let specs: Vec<(String, String, f64)> = accept
        .split(',')
        .filter_map(|entry| {
            let mut parts = entry.split(';');
            let full = parts.next()?.trim().to_ascii_lowercase();
            let (t, s) = full.split_once('/')?;
            let mut q = 1.0;
            for p in parts {
                if let Some((k, v)) = p.split_once('=')
                    && k.trim() == "q"
                {
                    q = v.trim().parse().unwrap_or(1.0);
                }
            }
            Some((t.trim().to_owned(), s.trim().to_owned(), q))
        })
        .collect();
    let best = |ty: &str, sub: &str| {
        specs
            .iter()
            .enumerate()
            .filter_map(|(i, (t, s, q))| {
                let mut spec = 0;
                if t == ty {
                    spec |= 4;
                } else if t != "*" {
                    return None;
                }
                if s == sub {
                    spec |= 2;
                } else if s != "*" {
                    return None;
                }
                Some((*q, spec, i))
            })
            .max_by(|a, b| a.1.cmp(&b.1).then(b.2.cmp(&a.2)).then_with(|| a.0.total_cmp(&b.0)))
    };
    let mut candidates: Vec<(f64, i32, usize, usize, &'static str)> =
        [("text", "plain", "text/plain"), ("text", "html", "text/html")]
            .iter()
            .enumerate()
            .filter_map(|(i, (t, s, name))| best(t, s).filter(|b| b.0 > 0.0).map(|(q, sp, o)| (q, sp, o, i, *name)))
            .collect();
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
    candidates.first().map(|c| c.4)
}

pub async fn well_known_carddav(req: Request) -> Response {
    let method = req.method().clone();
    tagged(permanent_redirect(&req, "/carddav/"), &method, "/.well-known/carddav")
}

pub async fn well_known_caldav(req: Request) -> Response {
    let method = req.method().clone();
    tagged(permanent_redirect(&req, "/caldav/"), &method, "/.well-known/caldav")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_like_js() {
        assert_eq!(decode_uri_component("a%20b").as_deref(), Some("a b"));
        assert_eq!(decode_uri_component("%E2%82%AC").as_deref(), Some("€"));
        assert_eq!(decode_uri_component("%E0"), None);
        assert_eq!(decode_uri_component("%zz"), None);
        assert_eq!(decode_uri_component("100%"), None);
    }

    #[test]
    fn segments_follow_express_non_strict_routes() {
        assert_eq!(segments("").unwrap(), Some(vec![]));
        assert_eq!(segments("/").unwrap(), Some(vec![]));
        assert_eq!(segments("/u").unwrap(), Some(vec!["u".to_owned()]));
        assert_eq!(segments("/u/").unwrap(), Some(vec!["u".to_owned()]));
        assert_eq!(segments("/u/b/a%2Fb.vcf/").unwrap(), Some(vec!["u".into(), "b".into(), "a/b.vcf".into()]));
        assert_eq!(segments("//").unwrap(), None);
        assert_eq!(segments("/a/b/c/d").unwrap(), None);
        assert!(segments("/u/%E0").is_err());
    }

    #[test]
    fn redirect_negotiation_matches_express() {
        assert_eq!(negotiate_text_or_html(None), Some("text/plain"));
        assert_eq!(negotiate_text_or_html(Some("*/*")), Some("text/plain"));
        assert_eq!(negotiate_text_or_html(Some("text/html,application/xhtml+xml,*/*;q=0.8")), Some("text/html"));
        assert_eq!(negotiate_text_or_html(Some("application/json")), None);
        assert_eq!(negotiate_text_or_html(Some("text/*;q=0.5, text/html")), Some("text/html"));
    }

    #[test]
    fn xml_escape_matches_node() {
        assert_eq!(xml_escape(Some("a<&>\"'")), "a&lt;&amp;&gt;&quot;'");
        assert_eq!(xml_escape(None), "");
    }
}
