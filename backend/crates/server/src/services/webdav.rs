//! Outbound WebDAV file client for the Files explorer: listing, download, upload, folders,
//! move/copy and delete against a user's own WebDAV server. Every request goes through the
//! SSRF-guarded client in `inboxora-net`, and the host is re-validated before each request.
//! Paths are always relative to the server's configured root URL; `..` never leaves it.

use std::time::Duration;

use base64::Engine;
use inboxora_net::Guard;
use inboxora_net::reqwest::{self, Method};
use percent_encoding::percent_decode_str;
use serde_json::Value;

use super::dav_client::hostname;
use super::dav_xml;

/// One configured server, decrypted and ready to talk to.
#[derive(Debug, Clone)]
pub struct Conn {
    pub base: url::Url,
    pub username: String,
    pub password: String,
    pub allow_private: bool,
}

/// One file or folder as the explorer shows it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub name: String,
    /// Relative to the server root, starting with `/`; folders end with `/`.
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<String>,
    pub content_type: Option<String>,
}

/// A failed WebDAV call: the HTTP status to answer with and a user-facing message.
#[derive(Debug, Clone, PartialEq)]
pub struct DavError {
    pub status: u16,
    pub message: String,
}

impl DavError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
}

type R<T> = std::result::Result<T, DavError>;

/// The clean segments of a relative path: empty and `.` segments dropped, `..` rejected.
pub fn segments(rel: &str) -> R<Vec<String>> {
    let mut out = Vec::new();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Err(DavError::new(400, "Invalid path")),
            s if s.contains('\\') || s.chars().any(char::is_control) => {
                return Err(DavError::new(400, "Invalid path"));
            }
            s => out.push(s.to_owned()),
        }
    }
    Ok(out)
}

/// The canonical relative form of `rel` (`/a/b`, or `/a/b/` for a folder).
pub fn normalize(rel: &str, dir: bool) -> R<String> {
    let segs = segments(rel)?;
    let mut p = format!("/{}", segs.join("/"));
    if dir && !segs.is_empty() {
        p.push('/');
    }
    Ok(p)
}

/// The absolute URL of `rel` under the server root.
pub fn target(base: &url::Url, rel: &str, dir: bool) -> R<url::Url> {
    let segs = segments(rel)?;
    let mut u = base.clone();
    {
        let mut path = u.path_segments_mut().map_err(|()| DavError::new(400, "Invalid server URL"))?;
        path.pop_if_empty();
        for s in &segs {
            path.push(s);
        }
        if dir || segs.is_empty() {
            path.push("");
        }
    }
    u.set_query(None);
    u.set_fragment(None);
    Ok(u)
}

fn decoded(path: &str) -> String {
    path.split('/').map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned()).collect::<Vec<_>>().join("/")
}

/// The decoded root path, always ending with `/`.
fn root_path(base: &url::Url) -> String {
    let mut p = decoded(base.path());
    if !p.ends_with('/') {
        p.push('/');
    }
    p
}

fn basic_auth(username: &str, password: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}")))
}

/// Map a non-success WebDAV status to the answer the browser gets. Never 401 or 423: the app
/// reads those as its own session expiring or the screen lock.
fn status_error(status: u16) -> DavError {
    match status {
        401 => DavError::new(400, "Authentication failed — check the username and password"),
        403 => DavError::new(403, "The WebDAV server denied access"),
        404 => DavError::new(404, "File or folder not found"),
        405 | 409 | 412 => DavError::new(409, "An item with this name already exists or the parent folder is missing"),
        413 => DavError::new(413, "The file is too large for the WebDAV server"),
        423 => DavError::new(409, "The item is locked on the WebDAV server"),
        507 => DavError::new(507, "The WebDAV server is out of storage"),
        s => DavError::new(502, format!("WebDAV request failed ({s})")),
    }
}

/// One guarded request with Basic auth. The caller decides how to read the response.
pub async fn send(
    conn: &Conn,
    method: &str,
    url: &url::Url,
    headers: &[(&str, String)],
    body: Option<reqwest::Body>,
    timeout: Duration,
) -> R<reqwest::Response> {
    if let Some(e) = inboxora_net::validate_host(&hostname(url), conn.allow_private).await {
        return Err(DavError::new(400, e));
    }
    let m = Method::from_bytes(method.as_bytes()).map_err(|e| DavError::new(500, e.to_string()))?;
    let guard = Guard { allow_private: conn.allow_private, require_https: None };
    let mut req = inboxora_net::safe_request(m, url.as_str(), guard)
        .map_err(|e| DavError::new(502, format!("Could not reach the WebDAV server: {e}")))?
        .header("Authorization", basic_auth(&conn.username, &conn.password))
        .timeout(timeout);
    for (k, v) in headers {
        req = req.header(*k, v.as_str());
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    match req.send().await {
        Ok(r) => Ok(r),
        Err(e) if e.is_timeout() => Err(DavError::new(504, "WebDAV server did not respond (timed out)")),
        Err(_) => Err(DavError::new(502, "Could not reach the WebDAV server")),
    }
}

async fn send_ok(
    conn: &Conn,
    method: &str,
    url: &url::Url,
    headers: &[(&str, String)],
    body: Option<reqwest::Body>,
) -> R<reqwest::Response> {
    let res = send(conn, method, url, headers, body, Duration::from_secs(60)).await?;
    let status = res.status().as_u16();
    if (200..300).contains(&status) { Ok(res) } else { Err(status_error(status)) }
}

const PROPFIND_BODY: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:propfind xmlns:d=\"DAV:\"><d:prop>\
<d:resourcetype/><d:getcontentlength/><d:getlastmodified/><d:getcontenttype/><d:displayname/>\
</d:prop></d:propfind>";

async fn propfind(conn: &Conn, url: &url::Url, depth: &str) -> R<String> {
    let headers = [("Depth", depth.to_owned()), ("Content-Type", "application/xml; charset=utf-8".to_owned())];
    let res = send(conn, "PROPFIND", url, &headers, Some(PROPFIND_BODY.into()), Duration::from_secs(30)).await?;
    let status = res.status().as_u16();
    if status != 207 {
        return Err(if (200..300).contains(&status) {
            DavError::new(502, "The server did not answer like a WebDAV server")
        } else {
            status_error(status)
        });
    }
    res.text().await.map_err(|_| DavError::new(502, "Could not read the WebDAV server's answer"))
}

/// Check the server answers PROPFIND on its root with these credentials.
pub async fn probe(conn: &Conn) -> R<()> {
    let root = target(&conn.base, "/", true)?;
    let xml = propfind(conn, &root, "0").await?;
    dav_xml::parse(&xml).map(|_| ()).map_err(|_| DavError::new(502, "The server did not answer like a WebDAV server"))
}

fn number(v: Option<&Value>) -> Option<u64> {
    let t = dav_xml::text_of(v);
    let t = if t.is_empty() { v.map(dav_xml::js_string).unwrap_or_default() } else { t };
    t.trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u64)
}

fn text(v: Option<&Value>) -> Option<String> {
    let t = dav_xml::text_of(v);
    let t = if t.is_empty() && matches!(v, Some(Value::Number(_))) {
        v.map(dav_xml::js_string).unwrap_or_default()
    } else {
        t
    };
    let t = t.trim().to_owned();
    (!t.is_empty()).then_some(t)
}

/// The entries of a multistatus answer, relative to the server root, without `listed` itself.
pub fn parse_listing(xml: &str, base: &url::Url, listed: &str) -> R<Vec<Entry>> {
    let parsed = dav_xml::parse(xml).map_err(|_| DavError::new(502, "Could not parse the WebDAV listing"))?;
    let root = root_path(base);
    let listed = normalize(listed, true)?;
    let mut out = Vec::new();
    for response in dav_xml::to_array(parsed.get("multistatus").and_then(|m| m.get("response"))) {
        let href = dav_xml::text_of(response.get("href"));
        let href =
            if href.is_empty() { response.get("href").map(dav_xml::js_string).unwrap_or_default() } else { href };
        if href.is_empty() {
            continue;
        }
        let Ok(abs) = base.join(href.trim()) else { continue };
        let path = decoded(abs.path());
        let rel = match path.strip_prefix(root.as_str()) {
            Some(r) => r.to_owned(),
            // The root itself, listed without its trailing slash.
            None if format!("{path}/") == root => String::new(),
            None => continue,
        };
        let props = dav_xml::props_of(response);
        let is_dir = matches!(props.get("resourcetype"), Some(Value::Object(m)) if m.contains_key("collection"));
        let Ok(rel) = normalize(&rel, is_dir) else { continue };
        if rel == listed || rel == "/" {
            continue;
        }
        let name = rel.trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_owned();
        if name.is_empty() {
            continue;
        }
        out.push(Entry {
            name,
            path: rel,
            is_dir,
            size: if is_dir { None } else { number(props.get("getcontentlength")) },
            modified: text(props.get("getlastmodified"))
                .and_then(|m| chrono::DateTime::parse_from_rfc2822(&m).ok())
                .map(|d| inboxora_core::json::js_iso(&d.with_timezone(&chrono::Utc))),
            content_type: if is_dir { None } else { text(props.get("getcontenttype")) },
        });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

/// The folder's children.
pub async fn list(conn: &Conn, rel: &str) -> R<Vec<Entry>> {
    let url = target(&conn.base, rel, true)?;
    let xml = propfind(conn, &url, "1").await?;
    parse_listing(&xml, &conn.base, rel)
}

/// A file's GET response, for streaming to the browser.
pub async fn download(conn: &Conn, rel: &str) -> R<reqwest::Response> {
    if segments(rel)?.is_empty() {
        return Err(DavError::new(400, "Invalid path"));
    }
    let url = target(&conn.base, rel, false)?;
    let res = send(conn, "GET", &url, &[], None, Duration::from_secs(600)).await?;
    let status = res.status().as_u16();
    if (200..300).contains(&status) { Ok(res) } else { Err(status_error(status)) }
}

/// A file's bytes and content type, refusing anything above `limit`.
pub async fn read_bytes(conn: &Conn, rel: &str, limit: usize) -> R<(Vec<u8>, Option<String>)> {
    let mut res = download(conn, rel).await?;
    let content_type = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_owned())
        .filter(|v| !v.is_empty());
    if res.content_length().is_some_and(|l| l as usize > limit) {
        return Err(DavError::new(413, "Attachments must not exceed 25 MB in total"));
    }
    let mut buf = Vec::new();
    while let Some(chunk) =
        res.chunk().await.map_err(|_| DavError::new(502, "Download from the WebDAV server failed"))?
    {
        if buf.len() + chunk.len() > limit {
            return Err(DavError::new(413, "Attachments must not exceed 25 MB in total"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((buf, content_type))
}

/// Upload a file; without `overwrite` an existing file is a conflict.
pub async fn upload(
    conn: &Conn,
    rel: &str,
    body: reqwest::Body,
    content_type: Option<String>,
    overwrite: bool,
) -> R<()> {
    if segments(rel)?.is_empty() {
        return Err(DavError::new(400, "Invalid path"));
    }
    let url = target(&conn.base, rel, false)?;
    let mut headers = vec![("Content-Type", content_type.unwrap_or_else(|| "application/octet-stream".into()))];
    if !overwrite {
        headers.push(("If-None-Match", "*".into()));
    }
    let res = send(conn, "PUT", &url, &headers, Some(body), Duration::from_secs(1800)).await?;
    let status = res.status().as_u16();
    if (200..300).contains(&status) { Ok(()) } else { Err(status_error(status)) }
}

/// Create a folder.
pub async fn mkdir(conn: &Conn, rel: &str) -> R<()> {
    if segments(rel)?.is_empty() {
        return Err(DavError::new(400, "Invalid path"));
    }
    let url = target(&conn.base, rel, true)?;
    send_ok(conn, "MKCOL", &url, &[], None).await.map(|_| ())
}

/// Delete a file or folder (folders recursively, as WebDAV DELETE does).
pub async fn delete(conn: &Conn, rel: &str, dir: bool) -> R<()> {
    if segments(rel)?.is_empty() {
        return Err(DavError::new(400, "The root folder cannot be deleted"));
    }
    let url = target(&conn.base, rel, dir)?;
    send_ok(conn, "DELETE", &url, &[], None).await.map(|_| ())
}

/// MOVE or COPY `from` to `to` on the same server.
pub async fn transfer(conn: &Conn, method: &str, from: &str, to: &str, dir: bool, overwrite: bool) -> R<()> {
    let (src, dst) = (segments(from)?, segments(to)?);
    if src.is_empty() || dst.is_empty() {
        return Err(DavError::new(400, "Invalid path"));
    }
    if dir && dst.starts_with(&src) {
        return Err(DavError::new(400, "A folder cannot be moved into itself"));
    }
    let source = target(&conn.base, from, dir)?;
    let dest = target(&conn.base, to, dir)?;
    let mut headers = vec![("Destination", dest.to_string()), ("Overwrite", if overwrite { "T" } else { "F" }.into())];
    if dir {
        headers.push(("Depth", "infinity".into()));
    }
    send_ok(conn, method, &source, &headers, None).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> url::Url {
        url::Url::parse("https://dav.example.com/remote.php/dav/files/me").unwrap()
    }

    #[test]
    fn targets_stay_under_the_root() {
        assert_eq!(target(&base(), "/", true).unwrap().as_str(), "https://dav.example.com/remote.php/dav/files/me/");
        assert_eq!(
            target(&base(), "/Docs/a b#.pdf", false).unwrap().as_str(),
            "https://dav.example.com/remote.php/dav/files/me/Docs/a%20b%23.pdf"
        );
        assert_eq!(
            target(&base(), "Docs", true).unwrap().as_str(),
            "https://dav.example.com/remote.php/dav/files/me/Docs/"
        );
        assert!(target(&base(), "/../etc", false).is_err());
        assert_eq!(normalize("//a/./b/", true).unwrap(), "/a/b/");
    }

    #[test]
    fn listing_is_relative_and_skips_the_folder_itself() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
 <d:response><d:href>/remote.php/dav/files/me/Docs/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/files/me/Docs/a%20b.pdf</d:href>
  <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>1234</d:getcontentlength>
  <d:getlastmodified>Tue, 20 Oct 2026 10:00:00 GMT</d:getlastmodified><d:getcontenttype>application/pdf</d:getcontenttype></d:prop>
  <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>https://dav.example.com/remote.php/dav/files/me/Docs/Sub/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#;
        let entries = parse_listing(xml, &base(), "/Docs").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/Docs/Sub/");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "a b.pdf");
        assert_eq!(entries[1].size, Some(1234));
        assert_eq!(entries[1].content_type.as_deref(), Some("application/pdf"));
        assert_eq!(entries[1].modified.as_deref(), Some("2026-10-20T10:00:00.000Z"));
    }

    /// Round-trip against a real server: `WEBDAV_TEST_URL=http://127.0.0.1:8765/dav/
    /// WEBDAV_TEST_USER=… WEBDAV_TEST_PASSWORD=… cargo test -p inboxora-server webdav_live -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a live WebDAV server"]
    async fn webdav_live_round_trip() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        let conn = Conn {
            base: url::Url::parse(&env("WEBDAV_TEST_URL")).unwrap(),
            username: env("WEBDAV_TEST_USER"),
            password: env("WEBDAV_TEST_PASSWORD"),
            allow_private: true,
        };
        probe(&conn).await.unwrap();
        let bad = Conn { password: "wrong".into(), ..conn.clone() };
        assert_eq!(probe(&bad).await.unwrap_err().status, 400);

        let _ = delete(&conn, "/it-test", true).await;
        mkdir(&conn, "/it-test").await.unwrap();
        assert_eq!(mkdir(&conn, "/it-test").await.unwrap_err().status, 409);
        upload(&conn, "/it-test/hello wörld.txt", "hi there".into(), Some("text/plain".into()), false).await.unwrap();
        let again = upload(&conn, "/it-test/hello wörld.txt", "x".into(), None, false).await;
        assert!(again.is_err(), "upload without overwrite must not replace");
        upload(&conn, "/it-test/hello wörld.txt", "hi again".into(), None, true).await.unwrap();

        let listed = list(&conn, "/it-test").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, "/it-test/hello wörld.txt");
        assert_eq!(listed[0].size, Some(8));
        let (bytes, _) = read_bytes(&conn, "/it-test/hello wörld.txt", 1024).await.unwrap();
        assert_eq!(bytes, b"hi again");
        assert_eq!(read_bytes(&conn, "/it-test/hello wörld.txt", 4).await.unwrap_err().status, 413);

        mkdir(&conn, "/it-test/sub").await.unwrap();
        transfer(&conn, "COPY", "/it-test/hello wörld.txt", "/it-test/sub/copy.txt", false, false).await.unwrap();
        transfer(&conn, "MOVE", "/it-test/sub", "/it-test/moved", true, false).await.unwrap();
        assert!(transfer(&conn, "MOVE", "/it-test/moved", "/it-test/moved/inner", true, false).await.is_err());
        let root = list(&conn, "/it-test/").await.unwrap();
        assert_eq!(root.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["/it-test/moved/", "/it-test/hello wörld.txt"]);
        assert_eq!(list(&conn, "/it-test/moved").await.unwrap()[0].name, "copy.txt");

        delete(&conn, "/it-test", true).await.unwrap();
        assert_eq!(list(&conn, "/it-test").await.unwrap_err().status, 404);
    }
}
