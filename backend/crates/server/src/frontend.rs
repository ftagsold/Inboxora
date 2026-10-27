//! The web app and the bundled ntfy, served ahead of the API middleware stack (this replaced
//! the nginx container that used to front the backend).
//!
//! - `STATIC_DIR`: the built frontend (`frontend/dist`). Every GET/HEAD outside the backend's
//!   own paths is a file from there, or `index.html` for client-side routes. The app name in
//!   `index.html` and `manifest.json` is filled from `VITE_APP_NAME` at startup.
//! - `NTFY_URL`: the internal ntfy origin (e.g. `http://ntfy:80`). The UnifiedPush topic
//!   namespace (`/up` + 12 base62 characters) and ntfy's `/v1` API are proxied at the domain
//!   origin, `/push/*` with the prefix stripped (compatibility alias), WebSockets included.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tower::Service;
use tower_http::services::ServeDir;

use crate::http::middleware::request_is_secure;

const APP_NAME_PLACEHOLDER: &str = "__INBOXORA_APP_NAME__";

/// Paths the backend answers itself; everything else belongs to the web app.
const BACKEND_PREFIXES: &[&str] = &[
    "/api",
    "/ws",
    "/oauth",
    "/auth/oidc",
    "/carddav",
    "/caldav",
    "/.well-known/carddav",
    "/.well-known/caldav",
    "/calendar/feeds",
];

/// Content-hashed build output and other static media: cached for a year.
const IMMUTABLE_EXTENSIONS: &[&str] = &["js", "css", "png", "jpg", "jpeg", "gif", "ico", "svg", "woff", "woff2"];

// 'unsafe-inline' styles are required by the React app (inline style props); email images load
// from arbitrary https origins. The opt-in in-browser AI compiles WebAssembly and downloads its
// weights from Hugging Face and its model libraries from GitHub.
const CSP_HTTPS: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' https://fonts.gstatic.com data: https:; img-src 'self' data: https: blob:; connect-src 'self' https://huggingface.co https://*.huggingface.co https://*.hf.co https://raw.githubusercontent.com wss:; frame-src 'self'; object-src 'none'; base-uri 'self';";
const CSP_HTTP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' https://fonts.gstatic.com data: https:; img-src 'self' data: https: blob:; connect-src 'self' https://huggingface.co https://*.huggingface.co https://*.hf.co https://raw.githubusercontent.com wss: ws:; frame-src 'self'; object-src 'none'; base-uri 'self';";

#[derive(Clone, Default)]
pub struct Frontend {
    files: Option<Arc<StaticFiles>>,
    ntfy: Option<Arc<Ntfy>>,
}

impl Frontend {
    pub fn from_env() -> anyhow::Result<Self> {
        let env = inboxora_core::config::env;
        let files = match env("STATIC_DIR") {
            Some(dir) => {
                let name = env("VITE_APP_NAME").map(|n| n.replace(['\r', '\n'], "")).filter(|n| !n.is_empty());
                let files = StaticFiles::load(dir.into(), name.as_deref().unwrap_or("Inboxora"))?;
                tracing::info!("Serving the web app from {} as \"{}\"", files.dir.display(), files.name);
                Some(Arc::new(files))
            }
            None => None,
        };
        let ntfy = match env("NTFY_URL") {
            Some(url) => Some(Arc::new(Ntfy::new(&url)?)),
            None => None,
        };
        Ok(Self { files, ntfy })
    }
}

/// Outermost layer: ntfy paths are proxied, web-app paths served from disk, the rest goes on to
/// the API stack.
pub async fn dispatch(State(fe): State<Frontend>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if let Some(ntfy) = &fe.ntfy
        && let Some(target) = ntfy_target(path)
    {
        return match target {
            NtfyTarget::Redirect => (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "/push/")]).into_response(),
            NtfyTarget::Path(p) => ntfy.proxy(req, &p).await,
        };
    }
    if let Some(files) = &fe.files
        && matches!(*req.method(), Method::GET | Method::HEAD)
        && !is_backend_path(path)
    {
        return files.serve(req).await;
    }
    next.run(req).await
}

fn is_backend_path(path: &str) -> bool {
    BACKEND_PREFIXES.iter().any(|p| path.strip_prefix(p).is_some_and(|rest| rest.is_empty() || rest.starts_with('/')))
}

#[derive(Debug, PartialEq)]
enum NtfyTarget {
    Redirect,
    Path(String),
}

fn ntfy_target(path: &str) -> Option<NtfyTarget> {
    if path == "/push" {
        return Some(NtfyTarget::Redirect);
    }
    if let Some(rest) = path.strip_prefix("/push/") {
        return Some(NtfyTarget::Path(format!("/{rest}")));
    }
    if path.starts_with("/v1/") || is_unifiedpush_topic(path) {
        return Some(NtfyTarget::Path(path.to_owned()));
    }
    None
}

/// `/up` + 12 base62 characters, optionally followed by a subpath (`/upXXXXXXXXXXXX/ws`).
fn is_unifiedpush_topic(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/up") else { return false };
    let (topic, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    topic.len() == 12 && topic.bytes().all(|b| b.is_ascii_alphanumeric()) && (tail.is_empty() || tail.starts_with('/'))
}

struct StaticFiles {
    dir: PathBuf,
    name: String,
    serve_dir: ServeDir,
    index: Bytes,
    manifest: Option<Bytes>,
}

impl StaticFiles {
    fn load(dir: PathBuf, name: &str) -> anyhow::Result<Self> {
        let index = std::fs::read_to_string(dir.join("index.html"))
            .map_err(|e| anyhow::anyhow!("STATIC_DIR {}: cannot read index.html: {e}", dir.display()))?;
        let manifest = std::fs::read_to_string(dir.join("manifest.json")).ok();
        let json_name = serde_json::to_string(name)?;
        Ok(Self {
            serve_dir: ServeDir::new(&dir).append_index_html_on_directories(false),
            index: index.replace(APP_NAME_PLACEHOLDER, &escape_html(name)).into(),
            manifest: manifest.map(|m| m.replace(APP_NAME_PLACEHOLDER, &json_name[1..json_name.len() - 1]).into()),
            name: name.to_owned(),
            dir,
        })
    }

    async fn serve(&self, req: Request) -> Response {
        let secure = request_is_secure(req.headers());
        let path = req.uri().path().to_owned();
        let mut res = match path.as_str() {
            "/" | "/index.html" => self.index(),
            // Never cached, so a rename shows up on the next load.
            "/manifest.json" if self.manifest.is_some() => {
                let body = self.manifest.clone().unwrap_or_default();
                let mut res = Response::new(Body::from(body));
                res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
                res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                res
            }
            _ => {
                let immutable = path
                    .rsplit_once('.')
                    .is_some_and(|(_, ext)| IMMUTABLE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)));
                // ServeDir's error type is Infallible.
                let Ok(res) = self.serve_dir.clone().call(req).await;
                match res.status() {
                    // A missing asset is a 404; any other unknown path is a client-side route.
                    StatusCode::NOT_FOUND if immutable => StatusCode::NOT_FOUND.into_response(),
                    StatusCode::NOT_FOUND => self.index(),
                    status => {
                        let mut res = res.map(Body::new);
                        // The service worker controls push notifications and must update at once.
                        if path == "/sw.js" {
                            res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                        } else if immutable && status.is_success() {
                            res.headers_mut().insert(
                                header::CACHE_CONTROL,
                                HeaderValue::from_static("public, max-age=31536000, immutable"),
                            );
                        }
                        res
                    }
                }
            }
        };
        page_headers(res.headers_mut(), secure);
        res
    }

    /// The SPA shell: never cached, so browsers always get the current asset file names.
    fn index(&self) -> Response {
        let mut res = Response::new(Body::from(self.index.clone()));
        res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        res
    }
}

fn page_headers(h: &mut HeaderMap, secure: bool) {
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin-when-cross-origin"));
    h.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(if secure { CSP_HTTPS } else { CSP_HTTP }));
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

struct Ntfy {
    origin: Uri,
    client: Client<HttpConnector, Body>,
}

/// Connection-level headers that a proxy must not forward (RFC 9110 §7.6.1).
const HOP_BY_HOP: &[&str] =
    &["connection", "keep-alive", "proxy-connection", "te", "trailer", "transfer-encoding", "upgrade"];

impl Ntfy {
    fn new(url: &str) -> anyhow::Result<Self> {
        let origin: Uri = url.trim_end_matches('/').parse()?;
        anyhow::ensure!(
            origin.scheme_str() == Some("http") && origin.authority().is_some(),
            "NTFY_URL must be an http:// origin, got {url}"
        );
        Ok(Self { origin, client: Client::builder(TokioExecutor::new()).build_http() })
    }

    async fn proxy(&self, mut req: Request, path: &str) -> Response {
        let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_canonical());
        let client_upgrade = req.headers().get(header::UPGRADE).cloned().map(|u| (u, hyper::upgrade::on(&mut req)));
        let (mut parts, body) = req.into_parts();
        let query = parts.uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        let Ok(uri) = Uri::builder()
            .scheme("http")
            .authority(self.origin.authority().map_or("", |a| a.as_str()))
            .path_and_query(format!("{path}{query}"))
            .build()
        else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        parts.uri = uri;
        parts.version = axum::http::Version::HTTP_11;
        let h = &mut parts.headers;
        for name in HOP_BY_HOP {
            h.remove(*name);
        }
        if let Some((protocol, _)) = &client_upgrade {
            h.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
            h.insert(header::UPGRADE, protocol.clone());
        }
        // ntfy runs with behind-proxy and rate-limits by the X-Forwarded-For client address.
        if let Some(ip) = peer {
            let forwarded = match h.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
                Some(prior) => format!("{prior}, {ip}"),
                None => ip.to_string(),
            };
            if let Ok(v) = HeaderValue::from_str(&forwarded) {
                h.insert(HeaderName::from_static("x-forwarded-for"), v);
            }
        }
        if !h.contains_key("x-forwarded-proto") {
            h.insert(HeaderName::from_static("x-forwarded-proto"), HeaderValue::from_static("http"));
        }

        let mut res = match self.client.request(Request::from_parts(parts, body)).await {
            Ok(res) => res,
            Err(e) => {
                tracing::warn!("ntfy proxy: {e}");
                return StatusCode::BAD_GATEWAY.into_response();
            }
        };
        if res.status() == StatusCode::SWITCHING_PROTOCOLS
            && let Some((_, client_upgrade)) = client_upgrade
        {
            let upstream_upgrade = hyper::upgrade::on(&mut res);
            tokio::spawn(async move {
                match tokio::try_join!(client_upgrade, upstream_upgrade) {
                    Ok((client, upstream)) => {
                        let _ =
                            tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(upstream)).await;
                    }
                    Err(e) => tracing::debug!("ntfy proxy upgrade failed: {e}"),
                }
            });
        }
        res.map(Body::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_paths_bypass_the_web_app() {
        for p in [
            "/api",
            "/api/health",
            "/ws",
            "/oauth/token",
            "/auth/oidc/x/callback",
            "/carddav/",
            "/caldav",
            "/.well-known/caldav",
            "/calendar/feeds/abc",
        ] {
            assert!(is_backend_path(p), "{p}");
        }
        for p in ["/", "/inbox", "/apix", "/assets/index.js", "/calendar", "/settings/accounts", "/auth/login"] {
            assert!(!is_backend_path(p), "{p}");
        }
    }

    #[test]
    fn ntfy_paths() {
        assert_eq!(ntfy_target("/push"), Some(NtfyTarget::Redirect));
        assert_eq!(ntfy_target("/push/"), Some(NtfyTarget::Path("/".into())));
        assert_eq!(ntfy_target("/push/upAbc123XYZ789/ws"), Some(NtfyTarget::Path("/upAbc123XYZ789/ws".into())));
        assert_eq!(ntfy_target("/upAbc123XYZ789"), Some(NtfyTarget::Path("/upAbc123XYZ789".into())));
        assert_eq!(ntfy_target("/upAbc123XYZ789/json"), Some(NtfyTarget::Path("/upAbc123XYZ789/json".into())));
        assert_eq!(ntfy_target("/v1/health"), Some(NtfyTarget::Path("/v1/health".into())));
        assert_eq!(ntfy_target("/upAbc123XYZ78"), None);
        assert_eq!(ntfy_target("/upAbc123XYZ7890"), None);
        assert_eq!(ntfy_target("/upAbc123XYZ-89"), None);
        assert_eq!(ntfy_target("/upload"), None);
        assert_eq!(ntfy_target("/pushes"), None);
        assert_eq!(ntfy_target("/v1"), None);
    }

    fn static_dir(index: &str, manifest: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("inboxora-static-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), index).unwrap();
        std::fs::write(dir.join("manifest.json"), manifest).unwrap();
        std::fs::write(dir.join("assets/app.js"), "console.log(1)").unwrap();
        std::fs::write(dir.join("sw.js"), "self").unwrap();
        dir
    }

    #[test]
    fn fills_and_escapes_the_app_name() {
        let dir = static_dir("<title>__INBOXORA_APP_NAME__</title>", r#"{"name":"__INBOXORA_APP_NAME__"}"#);
        let name = r#"A & "B" <i>'C'</i> | \ $HOME /x/"#;
        let files = StaticFiles::load(dir.clone(), name).unwrap();
        assert_eq!(files.index, "<title>A &amp; &quot;B&quot; &lt;i&gt;&#39;C&#39;&lt;/i&gt; | \\ $HOME /x/</title>");
        let manifest: serde_json::Value = serde_json::from_slice(files.manifest.as_ref().unwrap()).unwrap();
        assert_eq!(manifest["name"], name);
        std::fs::remove_dir_all(dir).unwrap();
    }

    async fn get(files: &StaticFiles, path: &str) -> Response {
        files.serve(Request::builder().uri(path).body(Body::empty()).unwrap()).await
    }

    #[tokio::test]
    async fn serves_files_with_spa_fallback_and_cache_rules() {
        let dir = static_dir("<title>__INBOXORA_APP_NAME__</title>", "{}");
        let files = StaticFiles::load(dir.clone(), "Inboxora").unwrap();

        let res = get(&files, "/assets/app.js").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CACHE_CONTROL], "public, max-age=31536000, immutable");

        let res = get(&files, "/settings/accounts").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(res.headers()[header::X_FRAME_OPTIONS], "SAMEORIGIN");
        let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        assert_eq!(body, "<title>Inboxora</title>");

        assert_eq!(get(&files, "/assets/missing.js").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(get(&files, "/sw.js").await.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(get(&files, "/manifest.json").await.headers()[header::CACHE_CONTROL], "no-cache");
        // Directories never list; they fall back to the app.
        assert_eq!(get(&files, "/assets/").await.headers()[header::CACHE_CONTROL], "no-store");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
