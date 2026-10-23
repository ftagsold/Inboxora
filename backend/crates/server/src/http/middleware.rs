//! Middleware, in Express registration order: CORS → timing → security headers → session →
//! CSRF gate (/api) → lock gate (/api) → routes → 404.

use std::time::Instant;

use axum::body::Body as AxumBody;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::state::AppState;

fn is_dav_path(path: &str) -> bool {
    path == "/carddav" || path.starts_with("/carddav/") || path == "/caldav" || path.starts_with("/caldav/")
}

/// `Origin` followed by the other (de-duplicated) fields of existing Vary headers.
fn vary_after_origin<'a>(existing: impl Iterator<Item = &'a str>) -> String {
    let mut fields = vec!["Origin".to_owned()];
    for f in existing.flat_map(|v| v.split(',')).map(str::trim).filter(|f| !f.is_empty()) {
        if !fields.iter().any(|x| x.eq_ignore_ascii_case(f)) {
            fields.push(f.to_owned());
        }
    }
    fields.join(", ")
}

/// `cors({ origin: FRONTEND_URL, credentials: true })`, skipped for DAV paths. Preflights
/// (any OPTIONS) end here with 204, before routing, exactly like the `cors` package.
pub async fn cors(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if is_dav_path(req.uri().path()) {
        return next.run(req).await;
    }
    let origin = HeaderValue::from_str(&state.cfg.frontend_url).unwrap_or(HeaderValue::from_static("*"));
    if req.method() == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        let h = res.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        h.append(header::VARY, HeaderValue::from_static("Origin"));
        h.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
        h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET,HEAD,PUT,PATCH,POST,DELETE"));
        if let Some(requested) = req.headers().get(header::ACCESS_CONTROL_REQUEST_HEADERS).cloned() {
            h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested);
            h.append(header::VARY, HeaderValue::from_static("Access-Control-Request-Headers"));
        }
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        return res;
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    // `cors` adds `Vary: Origin` before the handler runs, so fields the handler varies on
    // (`res.vary('Accept')`) follow it in one merged header.
    let vary = vary_after_origin(h.get_all(header::VARY).iter().filter_map(|v| v.to_str().ok()));
    h.insert(header::VARY, HeaderValue::from_str(&vary).unwrap_or(HeaderValue::from_static("Origin")));
    h.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
    res
}

/// A handler-chosen metrics key (`METHOD pattern`) for routes dispatched inside one handler.
#[derive(Clone, Debug)]
pub struct RoutePattern(pub String);

/// Record request latency under the matched route pattern (never the concrete URL).
pub async fn timing(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let res = next.run(req).await;
    let key = match res.extensions().get::<RoutePattern>() {
        Some(RoutePattern(k)) => k.clone(),
        None => {
            let pattern = res
                .extensions()
                .get::<MatchedPath>()
                .map_or_else(|| "unmatched".to_owned(), |p| inboxora_core::metrics::express_pattern(p.as_str()));
            format!("{method} {pattern}")
        }
    };
    inboxora_core::metrics::record_http(&key, start.elapsed(), res.status().is_server_error());
    res
}

/// Copy the matched path into the response so the outer timing layer can see it.
pub async fn expose_matched_path(req: Request, next: Next) -> Response {
    let matched = req.extensions().get::<MatchedPath>().cloned();
    let mut res = next.run(req).await;
    if let Some(m) = matched {
        res.extensions_mut().insert(m);
    }
    res
}

/// Security headers on every response; JSON responses also get Express's explicit charset.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    if h.get(header::CONTENT_TYPE).is_some_and(|v| v.as_bytes() == b"application/json") {
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    res
}

/// Marks a response written without `res.send` (no ETag), e.g. the 404 page.
#[derive(Clone, Copy)]
pub struct NoEtag;

/// Express's weak ETag (`etag` package) for a fully buffered body.
pub fn weak_etag(body: &[u8]) -> String {
    use base64::Engine;
    use sha1::Digest;
    let hash = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(body));
    format!("W/\"{:x}-{}\"", body.len(), &hash[..27])
}

const ETAG_MAX_BYTES: u64 = 16 * 1024 * 1024;

fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    let strip = |t: &str| t.trim().trim_start_matches("W/").to_owned();
    if_none_match.trim() == "*" || if_none_match.split(',').any(|t| strip(t) == strip(etag))
}

/// `res.send` ETag + freshness: buffered responses get a weak ETag, and a GET/HEAD whose
/// If-None-Match matches a 2xx response is answered with 304. Streamed bodies are untouched.
pub async fn etag(req: Request, next: Next) -> Response {
    use http_body_util::BodyExt;
    let conditional = matches!(*req.method(), Method::GET | Method::HEAD);
    let inm = req.headers().get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(str::to_owned);
    // `fresh`: a request with Cache-Control: no-cache is always stale.
    let no_cache = req
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == "no-cache"));
    let res = next.run(req).await;
    let status = res.status();
    let exact = http_body::Body::size_hint(res.body()).exact();
    // A handler-supplied ETag (`res.set('ETag', …).send()`): Express still answers a fresh
    // conditional GET with 304.
    if let Some(existing) = res.headers().get(header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned) {
        if conditional
            && !no_cache
            && status.is_success()
            && inm.as_deref().is_some_and(|i| etag_matches(i, &existing))
            && res.extensions().get::<NoEtag>().is_none()
        {
            let (mut parts, _) = res.into_parts();
            parts.status = StatusCode::NOT_MODIFIED;
            for h in [header::CONTENT_TYPE, header::CONTENT_LENGTH, header::TRANSFER_ENCODING] {
                parts.headers.remove(h);
            }
            return Response::from_parts(parts, AxumBody::empty());
        }
        return res;
    }
    if status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
        || exact.is_none_or(|n| n > ETAG_MAX_BYTES)
        || res.headers().contains_key(header::UPGRADE)
        || res.extensions().get::<NoEtag>().is_some()
    {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "").into_response(),
    };
    let tag = weak_etag(&bytes);
    if let Ok(v) = HeaderValue::from_str(&tag) {
        parts.headers.insert(header::ETAG, v);
    }
    if conditional && !no_cache && status.is_success() && inm.as_deref().is_some_and(|i| etag_matches(i, &tag)) {
        parts.status = StatusCode::NOT_MODIFIED;
        for h in [header::CONTENT_TYPE, header::CONTENT_LENGTH, header::TRANSFER_ENCODING] {
            parts.headers.remove(h);
        }
        return Response::from_parts(parts, AxumBody::empty());
    }
    Response::from_parts(parts, AxumBody::from(bytes))
}

/// `req.secure` behind one trusted proxy: the first X-Forwarded-Proto value.
pub fn request_is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
}

/// Load the session before the handler and commit it afterwards (save / touch / Set-Cookie).
pub async fn session(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let cookie = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let secure = request_is_secure(req.headers());
    let session = match state.sessions.load(cookie.as_deref(), secure).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Unhandled route error: session store: {e}");
            return internal_error();
        }
    };
    req.extensions_mut().insert(session.clone());
    let mut res = next.run(req).await;
    match state.sessions.commit(&session).await {
        Ok(Some(set_cookie)) => {
            if let Ok(v) = HeaderValue::from_str(&set_cookie) {
                res.headers_mut().append(header::SET_COOKIE, v);
            }
        }
        Ok(None) => {}
        Err(e) => tracing::error!("session save failed: {e}"),
    }
    res
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({ "error": "Internal server error" }))).into_response()
}

fn is_api(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// CSRF defense-in-depth for the cookie-authenticated /api surface: a mutating request must
/// carry `X-Requested-With`, which a cross-site form cannot set.
pub async fn csrf(req: Request, next: Next) -> Response {
    if is_api(req.uri().path()) && !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        let present = req.headers().get("x-requested-with").is_some_and(|v| !v.as_bytes().is_empty());
        if !present {
            return (StatusCode::FORBIDDEN, axum::Json(json!({ "error": "Missing required X-Requested-With header" })))
                .into_response();
        }
    }
    next.run(req).await
}

const LOCK_ALLOWED: [&str; 5] = ["/api/auth/unlock", "/api/auth/logout", "/api/auth/me", "/api/health", "/api/version"];

/// Screen-lock enforcement: a locked session may only reach the lock-screen endpoints.
pub async fn lock(req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let locked = req.extensions().get::<inboxora_core::session::Session>().is_some_and(|s| s.is_locked());
    if locked && is_api(path) && !LOCK_ALLOWED.contains(&path) {
        return (StatusCode::LOCKED, axum::Json(json!({ "error": "Locked", "locked": true }))).into_response();
    }
    next.run(req).await
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Express's default 404 page (finalhandler).
pub async fn not_found(req: Request) -> Response {
    let body = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Error</title>\n</head>\n<body>\n<pre>Cannot {} {}</pre>\n</body>\n</html>\n",
        req.method(),
        escape_html(
            req.extensions().get::<axum::extract::OriginalUri>().map_or_else(|| req.uri().path(), |o| o.0.path())
        )
    );
    let mut res = Response::new(AxumBody::from(body));
    *res.status_mut() = StatusCode::NOT_FOUND;
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'"));
    res.extensions_mut().insert(NoEtag);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_etag_matches_express() {
        // Express: res.json({ status: 'ok' }) → W/"f-VaSQ4oDUiZblZNAEkkN+sX+q3Sg"
        assert_eq!(weak_etag(br#"{"status":"ok"}"#), "W/\"f-VaSQ4oDUiZblZNAEkkN+sX+q3Sg\"");
        assert!(etag_matches("W/\"f-VaSQ4oDUiZblZNAEkkN+sX+q3Sg\", \"x\"", "W/\"f-VaSQ4oDUiZblZNAEkkN+sX+q3Sg\""));
    }

    #[test]
    fn csrf_and_lock_scope_is_api_only() {
        assert!(is_api("/api") && is_api("/api/x") && !is_api("/apix") && !is_api("/carddav/"));
        assert!(is_dav_path("/caldav/") && !is_dav_path("/caldavx"));
        assert_eq!(vary_after_origin(["Accept", "origin, Cookie"].into_iter()), "Origin, Accept, Cookie");
        assert_eq!(vary_after_origin(std::iter::empty()), "Origin");
    }
}
