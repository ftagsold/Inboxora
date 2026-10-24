//! Outbound CardDAV/CalDAV clients (ports of `carddavClient.ts`, `caldavClient.ts`,
//! `connectionPolicy.ts` and the URL helpers in `davSourceUrl.ts`). Every request goes through
//! the SSRF-guarded client in `inboxora-net`, and the host is validated before connecting.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use base64::Engine;
use inboxora_core::pg::{Db, Q};
use inboxora_core::{Result, params};
use inboxora_net::reqwest::Method;
use inboxora_net::reqwest::header::HeaderMap;
use inboxora_net::{Guard, NetError};
use parking_lot::Mutex;
use regex::Regex;

use super::dav_xml::{self, AddressBook, Card, DiscoveredCalendar};

// ── connection policy ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectionPolicy {
    pub allow_private_hosts: bool,
    pub allow_insecure_tls: bool,
    pub allow_nonstandard_ports: bool,
}

static POLICY: LazyLock<Mutex<Option<(ConnectionPolicy, Instant)>>> = LazyLock::new(|| Mutex::new(None));

/// `ALLOW_PRIVATE_HOSTS` (`true`/`false`): when set it overrides the admin setting. Unset, the
/// admin setting applies, and without one private hosts are allowed (self-hosted mail servers
/// usually sit on the same network). `ALLOW_PRIVATE_IMAP_HOSTS=true` is accepted as an alias.
pub fn private_hosts_from_env() -> Option<bool> {
    let parse = |v: String| match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    };
    inboxora_core::config::env("ALLOW_PRIVATE_HOSTS")
        .and_then(parse)
        .or_else(|| (inboxora_core::config::env("ALLOW_PRIVATE_IMAP_HOSTS").as_deref() == Some("true")).then_some(true))
}

/// Private hosts are allowed unless an admin turned them off (or the env var says otherwise).
pub const DEFAULT_ALLOW_PRIVATE_HOSTS: bool = true;

/// `getConnectionPolicy()`: admin network settings, cached for 30 s.
pub async fn connection_policy(db: &Db) -> Result<ConnectionPolicy> {
    if let Some((p, at)) = *POLICY.lock()
        && at.elapsed() < Duration::from_secs(30)
    {
        return Ok(p);
    }
    let rows = db
        .rows(
            "SELECT key, value FROM system_settings WHERE key IN ('allow_private_hosts', 'allow_insecure_tls', 'allow_nonstandard_ports')",
            params![],
        )
        .await?;
    let mut p = ConnectionPolicy { allow_private_hosts: DEFAULT_ALLOW_PRIVATE_HOSTS, ..Default::default() };
    for r in &rows {
        let on = r.get::<_, Option<String>>("value").as_deref() == Some("true");
        match r.get::<_, String>("key").as_str() {
            "allow_private_hosts" => p.allow_private_hosts = on,
            "allow_insecure_tls" => p.allow_insecure_tls = on,
            "allow_nonstandard_ports" => p.allow_nonstandard_ports = on,
            _ => {}
        }
    }
    if let Some(forced) = private_hosts_from_env() {
        p.allow_private_hosts = forced;
    }
    *POLICY.lock() = Some((p, Instant::now()));
    Ok(p)
}

/// `invalidateConnectionPolicyCache()`.
pub fn invalidate_connection_policy() {
    *POLICY.lock() = None;
}

// ── davSourceUrl.ts ──────────────────────────────────────────────────────────────────

/// WHATWG `url.hostname` (IPv6 literals keep their brackets).
pub fn hostname(url: &url::Url) -> String {
    match url.host() {
        Some(url::Host::Ipv6(v6)) => format!("[{v6}]"),
        Some(h) => h.to_string(),
        None => String::new(),
    }
}

/// `validateSourceUrl(raw)`: http(s) only, no credentials, SSRF-checked against the policy.
pub async fn validate_source_url(
    db: &Db,
    raw: &str,
) -> Result<std::result::Result<(url::Url, ConnectionPolicy), String>> {
    let Ok(mut parsed) = url::Url::parse(raw) else { return Ok(Err("Invalid source URL".into())) };
    if parsed.scheme() == "webcal" {
        let swapped = raw.split_once(':').map(|(_, rest)| format!("https:{rest}")).unwrap_or_default();
        match url::Url::parse(&swapped) {
            Ok(u) => parsed = u,
            Err(_) => return Ok(Err("Invalid source URL".into())),
        }
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Ok(Err("Source URL must not include credentials".into()));
    }
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Ok(Err("Source URL must use http(s)".into()));
    }
    let policy = connection_policy(db).await?;
    if let Some(e) = inboxora_net::validate_host(&hostname(&parsed), policy.allow_private_hosts).await {
        return Ok(Err(e.into()));
    }
    if parsed.scheme() == "http" {
        if !policy.allow_private_hosts {
            return Ok(Err("Source URL must use HTTPS".into()));
        }
        if inboxora_net::validate_host(&hostname(&parsed), false).await.is_none() {
            return Ok(Err("HTTPS is required for a public source".into()));
        }
    }
    Ok(Ok((parsed, policy)))
}

/// `resolveCaldavUrl(rawUrl, username)`: a blank URL falls back to the username's domain.
pub fn resolve_caldav_url(raw: Option<&str>, username: Option<&str>) -> std::result::Result<String, String> {
    static EMAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^\s@]+@([^\s@]+\.[^\s@]+)$").unwrap());
    if let Some(u) = raw.filter(|u| !u.is_empty()) {
        return Ok(u.to_owned());
    }
    let domain = username.and_then(|u| EMAIL.captures(inboxora_mail::text::js_trim(u)).map(|c| c[1].to_lowercase()));
    domain
        .map(|d| format!("https://{d}/"))
        .ok_or_else(|| "Enter a server URL, or leave it blank and use your email address as the username".into())
}

/// `clampIntervalSeconds(value)`: 15–86400, defaulting to 15.
pub fn clamp_interval_seconds(value: Option<&serde_json::Value>) -> i64 {
    let parsed = value.map(dav_xml::js_string).and_then(|s| inboxora_pim::ical::js_parse_int(&s)).filter(|v| *v != 0);
    parsed.unwrap_or(15).clamp(15, 86400)
}

/// `validatedCollectionHref(href, policy)` → (normalised href, hrefHash).
pub async fn validated_collection_href(
    href: Option<&serde_json::Value>,
    policy: ConnectionPolicy,
) -> std::result::Result<(String, String), String> {
    let Some(href) = href.and_then(|h| h.as_str()).filter(|h| !h.is_empty()) else {
        return Err("Each item to import needs an href".into());
    };
    let Ok(u) = url::Url::parse(href) else { return Err("Invalid collection href".into()) };
    if let Some(e) = inboxora_net::validate_host(&hostname(&u), policy.allow_private_hosts).await {
        return Err(e.into());
    }
    let normalized = u.to_string();
    let hash = inboxora_mail::util::href_hash(&normalized);
    Ok((normalized, hash))
}

// ── HTTP ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Creds {
    pub username: String,
    pub password: String,
    pub allow_private: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Flavor {
    CardDav,
    CalDav,
}

impl Flavor {
    fn name(self) -> &'static str {
        match self {
            Flavor::CardDav => "CardDAV",
            Flavor::CalDav => "CalDAV",
        }
    }
}

pub struct DavResponse {
    pub status: u16,
    pub reason: &'static str,
    pub text: String,
    pub headers: HeaderMap,
}

fn basic_auth(username: &str, password: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}")))
}

/// `assertHostAllowed(url, allowPrivate)`.
async fn assert_host_allowed(url: &str, allow_private: bool) -> std::result::Result<(), String> {
    let u = url::Url::parse(url).map_err(|_| "Invalid server URL".to_owned())?;
    match inboxora_net::validate_host(&hostname(&u), allow_private).await {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

/// The message `fetch()` rejects with: guard pre-checks keep theirs, transport failures (including
/// a connection the guard refused) are undici's generic `fetch failed`.
fn fetch_error(e: &NetError) -> String {
    e.to_string()
}

/// One DAV request (`dav()` in either client); 401 is always an authentication error.
pub async fn dav(
    flavor: Flavor,
    method: &str,
    url: &str,
    creds: &Creds,
    depth: Option<&str>,
    body: Option<String>,
    extra: &[(&str, String)],
) -> std::result::Result<DavResponse, String> {
    assert_host_allowed(url, creds.allow_private).await?;
    let m = Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    let guard = Guard { allow_private: creds.allow_private, require_https: None };
    let mut req = inboxora_net::safe_request(m, url, guard)
        .map_err(|e| format!("Could not reach the {} server: {}", flavor.name(), fetch_error(&e)))?
        .header("Authorization", basic_auth(&creds.username, &creds.password))
        .header("Content-Type", "application/xml; charset=utf-8")
        .timeout(Duration::from_secs(30));
    for (k, v) in extra {
        req = req.header(*k, v.as_str());
    }
    if let Some(d) = depth {
        req = req.header("Depth", d);
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    let res = match req.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return Err(format!("{} server did not respond (timed out)", flavor.name())),
        Err(_) => return Err(format!("Could not reach the {} server: fetch failed", flavor.name())),
    };
    let status = res.status();
    if status.as_u16() == 401 {
        return Err("Authentication failed — check the username and app password".into());
    }
    let headers = res.headers().clone();
    let text = match res.bytes().await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) if e.is_timeout() => return Err(format!("{} server did not respond (timed out)", flavor.name())),
        Err(_) => return Err(format!("Could not reach the {} server: fetch failed", flavor.name())),
    };
    Ok(DavResponse { status: status.as_u16(), reason: status.canonical_reason().unwrap_or(""), text, headers })
}

/// carddavClient's `dav()`: non-2xx (other than 207) is an error.
async fn carddav(
    method: &str,
    url: &str,
    creds: &Creds,
    depth: Option<&str>,
    body: String,
) -> std::result::Result<String, String> {
    let r = dav(Flavor::CardDav, method, url, creds, depth, Some(body), &[]).await?;
    if !(200..300).contains(&r.status) && r.status != 207 {
        return Err(format!("CardDAV request failed ({} {})", r.status, r.reason));
    }
    Ok(r.text)
}

fn origin(url: &str) -> String {
    url::Url::parse(url).map(|u| u.origin().ascii_serialization()).unwrap_or_default()
}

const DISCOVERY_AUTH: &str = "Authentication failed";

async fn propfind_href(
    flavor: Flavor,
    url: &str,
    prop: &str,
    key: &str,
    creds: &Creds,
) -> std::result::Result<Option<String>, String> {
    let ns = match flavor {
        Flavor::CardDav => "urn:ietf:params:xml:ns:carddav",
        Flavor::CalDav => "urn:ietf:params:xml:ns:caldav",
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<propfind xmlns=\"DAV:\" xmlns:C=\"{ns}\"><prop>{prop}</prop></propfind>"
    );
    let text = match flavor {
        Flavor::CardDav => carddav("PROPFIND", url, creds, Some("0"), body).await?,
        Flavor::CalDav => {
            let r = dav(flavor, "PROPFIND", url, creds, Some("0"), Some(body), &[]).await?;
            if r.status != 207 {
                return Ok(None);
            }
            r.text
        }
    };
    dav_xml::extract_href(&text, key, url)
}

async fn resolve_principal(flavor: Flavor, server_url: &str, creds: &Creds) -> std::result::Result<String, String> {
    let well_known = match flavor {
        Flavor::CardDav => "carddav",
        Flavor::CalDav => "caldav",
    };
    let candidates = [server_url.to_owned(), format!("{}/.well-known/{well_known}", origin(server_url))];
    let mut last: Option<String> = None;
    for base in &candidates {
        match propfind_href(flavor, base, "<current-user-principal/>", "current-user-principal", creds).await {
            Ok(Some(p)) => return Ok(p),
            Ok(None) => {}
            Err(e) if e.contains(DISCOVERY_AUTH) => return Err(e),
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(e) => Err(e),
        None => Ok(server_url.to_owned()),
    }
}

/// `discoverAddressBooks`.
pub async fn discover_address_books(server_url: &str, creds: &Creds) -> std::result::Result<Vec<AddressBook>, String> {
    assert_host_allowed(server_url, creds.allow_private).await?;
    let principal = resolve_principal(Flavor::CardDav, server_url, creds).await?;
    let home = propfind_href(Flavor::CardDav, &principal, "<C:addressbook-home-set/>", "addressbook-home-set", creds)
        .await?
        .unwrap_or(principal);
    let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<propfind xmlns=\"DAV:\" xmlns:cs=\"http://calendarserver.org/ns/\"><prop>\n  <resourcetype/><displayname/><cs:getctag/></prop></propfind>";
    let text = carddav("PROPFIND", &home, creds, Some("1"), body.into()).await?;
    let books = dav_xml::parse_address_books(&text, &home)?;
    if books.is_empty() {
        return Err("No address books found for this account".into());
    }
    Ok(books)
}

/// `fetchAddressBookCards`.
pub async fn fetch_address_book_cards(url: &str, creds: &Creds) -> std::result::Result<Vec<Card>, String> {
    assert_host_allowed(url, creds.allow_private).await?;
    let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<C:addressbook-query xmlns=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:carddav\"><prop>\n  <getetag/><C:address-data/></prop></C:addressbook-query>";
    let text = carddav("REPORT", url, creds, Some("1"), body.into()).await?;
    dav_xml::parse_cards(&text, url)
}

/// `discoverCalendars`.
pub async fn discover_calendars(
    server_url: &str,
    creds: &Creds,
) -> std::result::Result<Vec<DiscoveredCalendar>, String> {
    assert_host_allowed(server_url, creds.allow_private).await?;
    let principal = resolve_principal(Flavor::CalDav, server_url, creds).await?;
    let home = propfind_href(Flavor::CalDav, &principal, "<C:calendar-home-set/>", "calendar-home-set", creds)
        .await?
        .unwrap_or(principal);
    let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<propfind xmlns=\"DAV:\" xmlns:IC=\"http://apple.com/ns/ical/\"><prop>\n  <resourcetype/><displayname/><IC:calendar-color/></prop></propfind>";
    let r = dav(Flavor::CalDav, "PROPFIND", &home, creds, Some("1"), Some(body.into()), &[]).await?;
    if r.status != 207 {
        return Err(format!("CalDAV server rejected calendar discovery ({})", r.status));
    }
    let calendars = dav_xml::parse_calendars(&r.text, &home)?;
    if calendars.is_empty() {
        return Err("No calendars found for this account".into());
    }
    Ok(calendars)
}

/// The remote's current version of a resource after a conflict.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Remote {
    pub ical: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WriteBack {
    Ok { etag: Option<String> },
    Conflict { remote: Remote },
    Error(String),
}

fn etag_header(h: &HeaderMap) -> Option<String> {
    h.get("etag").and_then(|v| v.to_str().ok()).map(|v| v.replace('"', "")).filter(|v| !v.is_empty())
}

async fn refetch(href: &str, creds: &Creds) -> Remote {
    match dav(Flavor::CalDav, "GET", href, creds, None, None, &[]).await {
        Ok(r) if r.status == 200 => Remote { ical: Some(r.text), etag: etag_header(&r.headers) },
        _ => Remote::default(),
    }
}

/// `putCalendarEvent`: create (`If-None-Match: *`) or update (`If-Match`) a remote event.
pub async fn put_calendar_event(
    href: &str,
    ical: &str,
    if_match: Option<&str>,
    creds: &Creds,
) -> std::result::Result<WriteBack, String> {
    let mut headers = vec![("Content-Type", "text/calendar; charset=utf-8".to_owned())];
    match if_match {
        None => headers.insert(0, ("If-None-Match", "*".into())),
        Some(e) => headers.insert(0, ("If-Match", format!("\"{e}\""))),
    }
    let r = dav(Flavor::CalDav, "PUT", href, creds, None, Some(ical.to_owned()), &headers).await?;
    match r.status {
        200 | 201 | 204 => match etag_header(&r.headers) {
            Some(etag) => Ok(WriteBack::Ok { etag: Some(etag) }),
            None => Ok(WriteBack::Ok { etag: refetch(href, creds).await.etag }),
        },
        409 | 412 => Ok(WriteBack::Conflict { remote: refetch(href, creds).await }),
        s => Ok(WriteBack::Error(format!("Remote calendar rejected the write ({s})"))),
    }
}

/// `deleteCalendarEvent`.
pub async fn delete_calendar_event(
    href: &str,
    if_match: Option<&str>,
    creds: &Creds,
) -> std::result::Result<WriteBack, String> {
    let headers: Vec<(&str, String)> =
        if_match.filter(|e| !e.is_empty()).map(|e| vec![("If-Match", format!("\"{e}\""))]).unwrap_or_default();
    let r = dav(Flavor::CalDav, "DELETE", href, creds, None, None, &headers).await?;
    match r.status {
        200 | 204 | 404 => Ok(WriteBack::Ok { etag: None }),
        409 | 412 => Ok(WriteBack::Conflict { remote: refetch(href, creds).await }),
        s => Ok(WriteBack::Error(format!("Remote calendar rejected the delete ({s})"))),
    }
}

/// Create (`If-None-Match: *`) or update (`If-Match`) one vCard on a CardDAV server.
pub async fn put_card(
    href: &str,
    vcard: &str,
    if_match: Option<&str>,
    creds: &Creds,
) -> std::result::Result<(), String> {
    let mut headers = vec![("Content-Type", "text/vcard; charset=utf-8".to_owned())];
    match if_match.filter(|e| !e.is_empty()) {
        None => headers.insert(0, ("If-None-Match", "*".into())),
        Some(e) => headers.insert(0, ("If-Match", format!("\"{e}\""))),
    }
    let r = dav(Flavor::CardDav, "PUT", href, creds, None, Some(vcard.to_owned()), &headers).await?;
    match r.status {
        200 | 201 | 204 => Ok(()),
        409 | 412 => Err("The contact was changed on the CardDAV server meanwhile — reload and try again".into()),
        s => Err(format!("CardDAV server rejected the write ({s})")),
    }
}

/// Delete one vCard from a CardDAV server; already gone counts as deleted.
pub async fn delete_card(href: &str, if_match: Option<&str>, creds: &Creds) -> std::result::Result<(), String> {
    let headers: Vec<(&str, String)> =
        if_match.filter(|e| !e.is_empty()).map(|e| vec![("If-Match", format!("\"{e}\""))]).unwrap_or_default();
    let r = dav(Flavor::CardDav, "DELETE", href, creds, None, None, &headers).await?;
    match r.status {
        200 | 204 | 404 => Ok(()),
        409 | 412 => Err("The contact was changed on the CardDAV server meanwhile — reload and try again".into()),
        s => Err(format!("CardDAV server rejected the delete ({s})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_hosts_env_override() {
        use inboxora_core::config::set_env;
        set_env("ALLOW_PRIVATE_IMAP_HOSTS", None);
        set_env("ALLOW_PRIVATE_HOSTS", None);
        assert_eq!(private_hosts_from_env(), None, "unset: admin setting, else the default");
        for (v, want) in [("true", true), ("1", true), ("YES", true), ("false", false), ("0", false), ("off", false)] {
            set_env("ALLOW_PRIVATE_HOSTS", Some(v.into()));
            assert_eq!(private_hosts_from_env(), Some(want), "{v}");
        }
        set_env("ALLOW_PRIVATE_HOSTS", Some("maybe".into()));
        assert_eq!(private_hosts_from_env(), None, "unparsable value is ignored");
        set_env("ALLOW_PRIVATE_HOSTS", None);
        set_env("ALLOW_PRIVATE_IMAP_HOSTS", Some("true".into()));
        assert_eq!(private_hosts_from_env(), Some(true), "legacy alias");
        set_env("ALLOW_PRIVATE_IMAP_HOSTS", None);
    }

    #[test]
    fn caldav_url_falls_back_to_the_email_domain() {
        assert_eq!(resolve_caldav_url(Some("https://x.test/"), None).unwrap(), "https://x.test/");
        assert_eq!(resolve_caldav_url(None, Some(" Ann@Example.ORG ")).unwrap(), "https://example.org/");
        assert!(resolve_caldav_url(Some(""), Some("ann")).is_err());
    }

    #[test]
    fn intervals_clamp_like_node() {
        use serde_json::json;
        assert_eq!(clamp_interval_seconds(None), 15);
        assert_eq!(clamp_interval_seconds(Some(&json!(3))), 15);
        assert_eq!(clamp_interval_seconds(Some(&json!("600s"))), 600);
        assert_eq!(clamp_interval_seconds(Some(&json!(10_000_000))), 86400);
        assert_eq!(clamp_interval_seconds(Some(&json!(0))), 15);
        assert_eq!(clamp_interval_seconds(Some(&json!(-20))), 15);
    }
}
