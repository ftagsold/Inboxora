//! Remote calendar and address-book synchronisation (ports of `externalCalendarSync.ts`,
//! `externalCalendarWriteback.ts` and `carddavCollectionSync.ts`).
//!
//! Each source has at most one sync in flight. Removing a source cancels its in-flight sync
//! (its remote request is abandoned and no further rows are written) and waits for it to
//! finish before the caller deletes anything, so a late sync can never resurrect a removed
//! source's calendars or contacts.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use inboxora_core::pg::{Db, Q, Row, Val};
use inboxora_core::{Error, crypto, params};
use inboxora_mail::sanitize::encode_uri_component;
use inboxora_net::Guard;
use inboxora_pim::ical::{ParsedEvent, calendar_resources, parse_calendar_event};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::dav_client::{self, ConnectionPolicy, Creds, WriteBack};
use super::dav_xml;

type SyncResult = std::result::Result<Value, String>;

// ── per-source sync registry ─────────────────────────────────────────────────────────

struct Flight {
    removed: Arc<AtomicBool>,
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

#[derive(Default)]
struct Inner {
    syncing: HashSet<String>,
    timers: HashMap<String, JoinHandle<()>>,
    in_flight: HashMap<String, Flight>,
    stopped: HashSet<String>,
}

/// One registry per sync kind (calendar side / address-book side), like the Node modules.
pub struct Registry {
    inner: Mutex<Inner>,
    removed_message: &'static str,
}

/// What a running sync checks between steps.
#[derive(Clone)]
pub struct Ctx {
    removed: Arc<AtomicBool>,
    cancel: watch::Receiver<bool>,
    message: &'static str,
}

/// A cancel channel nobody ever signals, for fetches outside a source's sync.
static NEVER: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::channel(false).0);

impl Ctx {
    /// A context no removal can cancel (live reads on behalf of a request).
    fn detached(message: &'static str) -> Self {
        Ctx { removed: Arc::new(AtomicBool::new(false)), cancel: NEVER.subscribe(), message }
    }

    /// `throwIfRemoved(state)`.
    fn check(&self) -> std::result::Result<(), String> {
        if self.removed.load(Ordering::SeqCst) { Err(self.message.to_owned()) } else { Ok(()) }
    }

    /// Run `fut` unless the source is removed first (the Node AbortController).
    async fn guard<T>(
        &self,
        fut: impl Future<Output = std::result::Result<T, String>>,
    ) -> std::result::Result<T, String> {
        let mut cancel = self.cancel.clone();
        tokio::select! {
            r = fut => r,
            _ = cancel.wait_for(|c| *c) => Err(self.message.to_owned()),
        }
    }
}

impl Registry {
    fn new(removed_message: &'static str) -> Self {
        Registry { inner: Mutex::new(Inner::default()), removed_message }
    }

    /// Claim the source for one sync: `None` when a sync is already running.
    fn begin(&self, id: &str) -> Option<(Ctx, watch::Sender<bool>)> {
        let mut g = self.inner.lock();
        if !g.syncing.insert(id.to_owned()) {
            return None;
        }
        let removed = Arc::new(AtomicBool::new(false));
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        g.in_flight.insert(id.to_owned(), Flight { removed: removed.clone(), cancel: cancel_tx, done: done_rx });
        Some((Ctx { removed, cancel: cancel_rx, message: self.removed_message }, done_tx))
    }

    fn finish(&self, id: &str, done: &watch::Sender<bool>) {
        let mut g = self.inner.lock();
        g.syncing.remove(id);
        g.in_flight.remove(id);
        let _ = done.send(true);
    }

    fn is_stopped(&self, id: &str) -> bool {
        self.inner.lock().stopped.contains(id)
    }

    fn set_timer(&self, id: &str, handle: Option<JoinHandle<()>>) {
        let mut g = self.inner.lock();
        if let Some(prev) = g.timers.remove(id) {
            prev.abort();
        }
        if let Some(h) = handle {
            g.timers.insert(id.to_owned(), h);
        }
    }

    /// `stop…(id)`: cancel the schedule and any in-flight sync, and wait for it to end.
    pub async fn stop(&self, id: &str) {
        let done = {
            let mut g = self.inner.lock();
            let timer = g.timers.remove(id);
            let had_timer = timer.is_some();
            if let Some(t) = timer {
                t.abort();
            }
            let flight = g.in_flight.get(id).map(|f| (f.removed.clone(), f.cancel.clone(), f.done.clone()));
            if !had_timer && flight.is_none() {
                return;
            }
            g.stopped.insert(id.to_owned());
            let Some((removed, cancel, done)) = flight else { return };
            removed.store(true, Ordering::SeqCst);
            let _ = cancel.send(true);
            done
        };
        let mut done = done;
        let _ = done.wait_for(|d| *d).await;
    }

    /// `release…(id)`.
    pub fn release(&self, id: &str) {
        self.inner.lock().stopped.remove(id);
    }
}

pub static CALENDARS: LazyLock<Registry> = LazyLock::new(|| Registry::new("Calendar source removed"));
pub static ADDRESS_BOOKS: LazyLock<Registry> = LazyLock::new(|| Registry::new("DAV server removed"));

/// A `calendar_import_sources` row as the sync code reads it.
#[derive(Debug, Clone, Default)]
pub struct Source {
    pub id: String,
    pub user_id: String,
    pub kind: String,
    pub url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub display_name: Option<String>,
    pub color: Option<String>,
    pub interval_seconds: Option<i64>,
    pub writable: bool,
    pub collections: Option<String>,
    pub address_book_collections: Option<String>,
}

impl Source {
    pub fn from_row(r: &Row) -> Self {
        let text = |c: &str| r.try_get::<_, Option<String>>(c).ok().flatten();
        Source {
            id: r.try_get::<_, uuid::Uuid>("id").map(|u| u.to_string()).unwrap_or_default(),
            user_id: r.try_get::<_, uuid::Uuid>("user_id").map(|u| u.to_string()).unwrap_or_default(),
            kind: text("kind").unwrap_or_default(),
            url: text("url"),
            username: text("username"),
            password: text("password"),
            display_name: text("display_name"),
            color: text("color"),
            interval_seconds: r.try_get::<_, Option<i32>>("interval_seconds").ok().flatten().map(i64::from),
            writable: r.try_get::<_, Option<bool>>("writable").ok().flatten().unwrap_or(false),
            collections: text("collections"),
            address_book_collections: text("address_book_collections"),
        }
    }
}

/// Decrypt a stored JSON collection list (`parseSourceCollections` / `parseAddressBookCollections`).
pub fn parse_collections(stored: Option<&str>) -> Option<Vec<Value>> {
    let stored = stored.filter(|s| !s.is_empty())?;
    let plain = crypto::decrypt(stored)?;
    if plain.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(&plain) {
        Ok(Value::Array(list)) => Some(list),
        _ => None,
    }
}

fn str_field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn truthy(v: Option<&Value>) -> bool {
    inboxora_core::json::truthy(v)
}

fn decrypt_or_empty(v: Option<&str>) -> Option<String> {
    crypto::decrypt(v.unwrap_or(""))
}

// ── external calendars ───────────────────────────────────────────────────────────────

struct Collection {
    href: String,
    href_hash: String,
    display_name: Option<String>,
    color: Option<String>,
    legacy: bool,
}

fn collections_to_sync(source: &Source) -> Vec<Collection> {
    let discovered = if source.kind == "caldav" { parse_collections(source.collections.as_deref()) } else { None };
    if let Some(list) = discovered {
        return list
            .iter()
            .filter(|c| truthy(c.get("selected")))
            .map(|c| Collection {
                href: str_field(c, "href").unwrap_or_default().to_owned(),
                href_hash: str_field(c, "hrefHash").unwrap_or_default().to_owned(),
                display_name: str_field(c, "displayName").map(str::to_owned),
                color: str_field(c, "color").map(str::to_owned),
                legacy: truthy(c.get("legacy")),
            })
            .collect();
    }
    let Some(url) = decrypt_or_empty(source.url.as_deref()).filter(|u| !u.is_empty()) else { return Vec::new() };
    vec![Collection {
        href_hash: inboxora_mail::util::href_hash(&url),
        href: url,
        display_name: Some(source.display_name.clone().unwrap_or_default()),
        color: source.color.clone(),
        legacy: true,
    }]
}

/// `calendarPayloads(raw)`: one resource per event for an iCalendar URL feed.
fn calendar_payloads(raw: &str) -> std::result::Result<Vec<String>, String> {
    static EVENT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)BEGIN:VEVENT(?:\r\n|\n|\r)[\s\S]*?END:VEVENT").unwrap());
    static TZ: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)BEGIN:VTIMEZONE(?:\r\n|\n|\r)[\s\S]*?END:VTIMEZONE").unwrap());
    if inboxora_mail::text::js_trim(raw).is_empty() {
        return Err("Remote calendar did not contain any VEVENT components".into());
    }
    if let Ok(r) = calendar_resources(raw) {
        return Ok(r);
    }
    let events: Vec<&str> = EVENT.find_iter(raw).map(|m| m.as_str()).collect();
    if events.is_empty() {
        return Err("Remote calendar contains an unsupported event".into());
    }
    let zones: Vec<&str> = TZ.find_iter(raw).map(|m| m.as_str()).collect();
    let context = if zones.is_empty() { String::new() } else { format!("{}\r\n", zones.join("\r\n")) };
    Ok(events.iter().map(|b| format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{context}{b}\r\nEND:VCALENDAR\r\n")).collect())
}

fn basic_auth(username: &str, password: &str) -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}")))
}

/// One remote request (`remoteFetch`'s `url` and `options`).
struct RemoteRequest<'a> {
    url: &'a str,
    method: &'a str,
    headers: &'a [(&'a str, &'a str)],
    body: Option<&'a str>,
}

/// `remoteFetch(source, url, options, policy, signal, secretSink)`.
async fn remote_fetch(
    source: &Source,
    r: RemoteRequest<'_>,
    policy: ConnectionPolicy,
    ctx: &Ctx,
    secrets: &mut Vec<String>,
) -> std::result::Result<String, String> {
    let RemoteRequest { url, method, headers, body } = r;
    let mut auth = None;
    if source.kind == "caldav" {
        let password = decrypt_or_empty(source.password.as_deref()).filter(|p| !p.is_empty());
        let Some(password) = password else { return Err("Stored calendar source password is unavailable".into()) };
        auth = Some(basic_auth(source.username.as_deref().unwrap_or(""), &password));
    }
    if url.is_empty() {
        return Err("Stored calendar source URL is unavailable".into());
    }
    secrets.push(url.to_owned());
    let m = inboxora_net::reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    let mut req =
        inboxora_net::safe_request(m, url, Guard { allow_private: policy.allow_private_hosts, require_https: None })
            .map_err(|e| e.to_string())?
            .timeout(Duration::from_secs(30));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    if let Some(b) = body {
        req = req.body(b.to_owned());
    }
    ctx.guard(async {
        let res = req.send().await.map_err(|e| {
            if e.is_timeout() {
                "The operation was aborted due to timeout".to_owned()
            } else {
                "fetch failed".to_owned()
            }
        })?;
        let status = res.status().as_u16();
        if !(200..300).contains(&status) && status != 207 {
            return Err(format!("Remote calendar request failed ({status})"));
        }
        let bytes = res.bytes().await.map_err(|_| "fetch failed".to_owned())?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
    .await
}

struct Payload {
    ical: String,
    href: Option<String>,
    etag: Option<String>,
}

async fn fetch_events(
    source: &Source,
    url: &str,
    policy: ConnectionPolicy,
    ctx: &Ctx,
    secrets: &mut Vec<String>,
) -> std::result::Result<(Vec<Payload>, Option<String>), String> {
    if source.kind == "ical_url" {
        let req = RemoteRequest { url, method: "GET", headers: &[("Accept", "text/calendar")], body: None };
        let doc = remote_fetch(source, req, policy, ctx, secrets).await?;
        let payloads =
            calendar_payloads(&doc)?.into_iter().map(|ical| Payload { ical, href: None, etag: None }).collect();
        return Ok((payloads, Some(doc)));
    }
    let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?><C:calendar-query xmlns=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><prop><getetag/><C:calendar-data/></prop><C:filter><C:comp-filter name=\"VCALENDAR\"><C:comp-filter name=\"VEVENT\"/></C:comp-filter></C:filter></C:calendar-query>";
    let headers = [("Content-Type", "application/xml; charset=utf-8"), ("Depth", "1")];
    let req = RemoteRequest { url, method: "REPORT", headers: &headers, body: Some(body) };
    let raw = remote_fetch(source, req, policy, ctx, secrets).await?;
    let xml = dav_xml::parse(&raw)?;
    dav_xml::require_complete_multistatus(&raw, &xml)?;
    let mut payloads = Vec::new();
    let responses = dav_xml::to_array(xml.get("multistatus").and_then(|m| m.get("response")));
    for response in responses {
        let props = dav_xml::props_of_sync(response);
        let ical = dav_xml::decode_dav_char_refs(&dav_xml::text_of(props.get("calendar-data")));
        if ical.is_empty() {
            continue;
        }
        let href_text = dav_xml::text_of(response.get("href"));
        let href = (!href_text.is_empty())
            .then(|| url::Url::parse(url).and_then(|b| b.join(&href_text)).ok().map(|u| u.to_string()))
            .flatten();
        let etag = Some(dav_xml::text_of(props.get("getetag")).replace('"', "")).filter(|e| !e.is_empty());
        payloads.push(Payload { ical, href, etag });
    }
    Ok((payloads, None))
}

fn db_err(e: Error) -> String {
    e.to_string()
}

/// `calendarFor(source, collection, state)`: the local calendar a collection syncs into.
async fn calendar_for(db: &Db, source: &Source, c: &Collection, ctx: &Ctx) -> std::result::Result<uuid::Uuid, String> {
    let external =
        if c.legacy { format!("source:{}", source.id) } else { format!("source:{}:{}", source.id, c.href_hash) };
    let found = db
        .opt(
            "SELECT id FROM calendars WHERE user_id = $1 AND owner_user_id = $1 AND external_url = $2",
            params![&source.user_id, &external],
        )
        .await
        .map_err(db_err)?;
    ctx.check()?;
    if let Some(r) = found {
        return Ok(r.get("id"));
    }
    let base = if c.legacy {
        source.display_name.clone()
    } else {
        c.display_name.clone().filter(|n| !n.is_empty()).or_else(|| source.display_name.clone())
    };
    let color = if c.legacy {
        source.color.clone()
    } else {
        c.color.clone().filter(|n| !n.is_empty()).or_else(|| source.color.clone())
    };
    for attempt in 0..20 {
        ctx.check()?;
        let name = if attempt == 0 {
            base.clone()
        } else {
            Some(format!("{} ({})", base.as_deref().unwrap_or("null"), attempt + 1))
        };
        let inserted = db
            .opt(
                "INSERT INTO calendars (user_id, owner_user_id, name, color, source, external_url, read_only) VALUES ($1, $1, $2, $3, $4, $5, $6) RETURNING id",
                params![&source.user_id, name, color.clone(), &source.kind, &external, !source.writable],
            )
            .await;
        match inserted {
            Ok(Some(r)) => return Ok(r.get("id")),
            Ok(None) => return Err("Could not create a calendar".into()),
            Err(e) if e.is_unique_violation() => {}
            Err(e) => return Err(db_err(e)),
        }
    }
    Err(format!("Could not create a calendar for \"{}\"", base.as_deref().unwrap_or("null")))
}

const UPSERT_EVENT: &str = "INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at, all_day, timezone, description, location, url, organizer, attendees, remote_href, remote_etag) \
     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15::jsonb,$16,$17) \
     ON CONFLICT (calendar_id, uid, recurrence_id) DO UPDATE SET raw_ical = EXCLUDED.raw_ical, \
       etag = EXCLUDED.etag, summary = EXCLUDED.summary, starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, \
       all_day = EXCLUDED.all_day, timezone = EXCLUDED.timezone, description = EXCLUDED.description, \
       location = EXCLUDED.location, url = EXCLUDED.url, organizer = EXCLUDED.organizer, attendees = EXCLUDED.attendees, \
       remote_href = EXCLUDED.remote_href, remote_etag = EXCLUDED.remote_etag, updated_at = NOW() \
     WHERE (calendar_events.etag, calendar_events.remote_href, calendar_events.remote_etag) \
       IS DISTINCT FROM (EXCLUDED.etag, EXCLUDED.remote_href, EXCLUDED.remote_etag)";

fn event_params(
    calendar_id: uuid::Uuid,
    user_id: &str,
    e: &ParsedEvent,
    href: Option<String>,
    etag: Option<String>,
) -> Vec<Val> {
    vec![
        Val::from(calendar_id),
        Val::from(user_id),
        Val::from(&e.uid),
        Val::from(&e.raw),
        Val::from(crypto::sha256_hex(&e.raw)),
        Val::from(e.summary.clone()),
        Val::from(e.starts_at),
        Val::from(e.ends_at),
        Val::from(e.all_day),
        Val::from(e.time_zone.clone()),
        Val::from(e.description.clone()),
        Val::from(e.location.clone()),
        Val::from(e.url.clone()),
        Val::from(e.organizer.clone()),
        Val::from(inboxora_core::json::stringify(&json!(e.attendees))),
        Val::from(href),
        Val::from(etag),
    ]
}

async fn sync_collection(
    db: &Db,
    source: &Source,
    c: &Collection,
    policy: ConnectionPolicy,
    ctx: &Ctx,
    secrets: &mut Vec<String>,
) -> std::result::Result<(usize, Vec<Value>), String> {
    static UID: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:^|\r\n|\n|\r)UID(?:;[^:]*)?:([^\r\n]*)").unwrap());
    let (payloads, document) = fetch_events(source, &c.href, policy, ctx, secrets).await?;
    ctx.check()?;
    let mut events = Vec::new();
    let mut skipped = Vec::new();
    for (index, p) in payloads.into_iter().enumerate() {
        match parse_calendar_event(&p.ical) {
            Some(e) => events.push((e, p.href, p.etag)),
            None => {
                let uid = UID
                    .captures(&p.ical)
                    .map(|m| inboxora_mail::text::js_trim(&m[1]).to_owned())
                    .filter(|u| !u.is_empty())
                    .unwrap_or_else(|| format!("event-{}", index + 1));
                skipped.push(json!({ "uid": uid, "reason": "unsupported or malformed VEVENT" }));
            }
        }
    }
    if events.is_empty() && !skipped.is_empty() {
        return Err("Remote calendar contains an unsupported event".into());
    }
    let calendar_id = calendar_for(db, source, c, ctx).await?;
    if is_live(source) {
        // Served live: drop rows a former two-way sync left behind and warm the cache.
        db.exec("DELETE FROM calendar_events WHERE calendar_id = $1", params![calendar_id]).await.map_err(db_err)?;
        let _ = document;
        let live: Vec<LiveEvent> = events
            .iter()
            .map(|(e, href, etag)| LiveEvent { raw: e.raw.clone(), href: href.clone(), etag: etag.clone() })
            .collect();
        super::remote_dav::store_calendar(calendar_id, &live).await;
        return Ok((events.len(), skipped));
    }
    if let Some(doc) = document {
        ctx.check()?;
        let _ = doc;
    }
    let mut seen: Vec<String> = Vec::new();
    for (e, href, etag) in &events {
        ctx.check()?;
        seen.push(e.uid.clone());
        db.exec(UPSERT_EVENT, &event_params(calendar_id, &source.user_id, e, href.clone(), etag.clone()))
            .await
            .map_err(db_err)?;
    }
    ctx.check()?;
    let mut retained = seen;
    retained.extend(skipped.iter().filter_map(|s| s["uid"].as_str().map(str::to_owned)));
    if retained.is_empty() {
        retained.push(String::new());
    }
    let two_way = source.kind == "caldav" && source.writable;
    db.exec(
        "DELETE FROM calendar_events WHERE calendar_id = $1 AND uid <> ALL($2::text[]) AND created_at < NOW() - INTERVAL '2 minutes' \
         AND (NOT $3::boolean OR remote_href IS NOT NULL)",
        params![calendar_id, retained, two_way],
    )
    .await
    .map_err(db_err)?;
    if two_way && !source.user_id.is_empty() {
        upload_pending_events(db, calendar_id, &source.user_id).await.map_err(db_err)?;
    }
    Ok((events.len(), skipped))
}

/// Push local-only events of a two-way calendar to the remote.
async fn upload_pending_events(db: &Db, calendar_id: uuid::Uuid, user_id: &str) -> inboxora_core::Result<()> {
    let pending = db
        .rows(
            "SELECT uid, raw_ical FROM calendar_events WHERE calendar_id = $1 AND user_id = $2 AND recurrence_id = '' AND remote_href IS NULL ORDER BY created_at LIMIT 50",
            params![calendar_id, user_id],
        )
        .await?;
    if pending.is_empty() {
        return Ok(());
    }
    let Some(calendar) = WritableCalendar::load(db, &calendar_id.to_string(), user_id).await? else { return Ok(()) };
    for event in &pending {
        let uid: String = event.get("uid");
        let raw: Option<String> = event.get("raw_ical");
        if let Outcome::Error(e) = push_event_to_remote(db, &calendar, &uid, raw.as_deref(), Action::Put, None).await {
            tracing::warn!("Uploading a calendar event to the remote calendar failed: {e}");
        }
    }
    Ok(())
}

async fn sync_source(db: &Db, source: &Source) -> Value {
    let Some((ctx, done)) = CALENDARS.begin(&source.id) else {
        return json!({ "ok": false, "error": "A sync is already in progress" });
    };
    let mut secrets: Vec<String> = Vec::new();
    let run = async {
        let collections = collections_to_sync(source);
        if collections.is_empty() {
            return Err("Stored calendar source URL is unavailable".to_owned());
        }
        let policy = dav_client::connection_policy(db).await.map_err(db_err)?;
        let mut count = 0;
        let mut skipped: Vec<Value> = Vec::new();
        for c in &collections {
            let (n, s) = sync_collection(db, source, c, policy, &ctx, &mut secrets).await?;
            count += n;
            skipped.extend(s);
        }
        ctx.check()?;
        if !skipped.is_empty() {
            let samples: Vec<Value> = skipped.iter().take(3).cloned().collect();
            let warning = inboxora_core::json::stringify(
                &json!({ "code": "unsupported_events", "count": skipped.len(), "samples": samples }),
            );
            db.exec(
                "UPDATE calendar_import_sources SET last_sync_at = NOW(), last_error = $2 WHERE id = $1",
                params![&source.id, warning],
            )
            .await
            .map_err(db_err)?;
            return Ok(json!({ "ok": true, "eventCount": count, "skipped": skipped }));
        }
        db.exec(
            "UPDATE calendar_import_sources SET last_sync_at = NOW(), last_error = NULL WHERE id = $1",
            params![&source.id],
        )
        .await
        .map_err(db_err)?;
        Ok(json!({ "ok": true, "eventCount": count }))
    };
    let result: SyncResult = run.await;
    let out = match result {
        Ok(v) => v,
        Err(_) if ctx.removed.load(Ordering::SeqCst) => json!({ "ok": false, "error": "Calendar source removed" }),
        Err(message) => {
            let mut safe = if message.is_empty() { "Calendar source sync failed".to_owned() } else { message };
            for secret in source.url.iter().chain(secrets.iter()).filter(|s| !s.is_empty()) {
                safe = safe.replace(secret.as_str(), "[redacted]");
            }
            if let Err(e) = db
                .exec(
                    "UPDATE calendar_import_sources SET last_sync_at = NOW(), last_error = $2 WHERE id = $1",
                    params![&source.id, &safe],
                )
                .await
            {
                tracing::error!("Calendar source sync: could not record the error: {e}");
            }
            json!({ "ok": false, "error": safe })
        }
    };
    CALENDARS.finish(&source.id, &done);
    out
}

async fn run_calendar_sync(db: &Db, source: &Source) -> Value {
    if source.id.is_empty() {
        return json!({ "ok": false, "error": "Calendar source is incomplete" });
    }
    if CALENDARS.is_stopped(&source.id) {
        return json!({ "ok": false, "error": "Calendar source removed" });
    }
    sync_source(db, source).await
}

/// `syncCalendarSource(userId, sourceId)`.
pub async fn sync_calendar_source(db: &Db, user_id: &str, source_id: &str) -> inboxora_core::Result<Value> {
    let row = db
        .opt(
            "SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2 AND enabled = true",
            params![source_id, user_id],
        )
        .await?;
    let Some(row) = row else { return Ok(json!({ "ok": false, "error": "Calendar source not found" })) };
    Ok(run_calendar_sync(db, &Source::from_row(&row)).await)
}

fn spawn_interval<F, Fut>(seconds: i64, mut tick: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let period = Duration::from_secs(seconds.max(1) as u64);
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            // Each tick runs detached, like setInterval: a slow sync never delays the schedule.
            tokio::spawn(tick());
        }
    })
}

/// `scheduleCalendarSource(source)`.
pub fn schedule_calendar_source(db: &Db, source: &Source) {
    let Some(seconds) = source.interval_seconds.filter(|s| *s > 0) else { return };
    if source.id.is_empty() {
        return;
    }
    let (db2, s2) = (db.clone(), source.clone());
    let handle = spawn_interval(seconds, move || {
        let (db, s) = (db2.clone(), s2.clone());
        async move {
            run_calendar_sync(&db, &s).await;
        }
    });
    CALENDARS.set_timer(&source.id, Some(handle));
}

/// `startExternalCalendarScheduler()`.
pub async fn start_calendar_scheduler(db: &Db) -> inboxora_core::Result<()> {
    for row in db.rows("SELECT * FROM calendar_import_sources WHERE enabled = true", params![]).await? {
        schedule_calendar_source(db, &Source::from_row(&row));
    }
    Ok(())
}

// ── write-back ───────────────────────────────────────────────────────────────────────

/// The calendar columns a push needs (`writableCalendar()`'s selection).
#[derive(Debug, Clone)]
pub struct WritableCalendar {
    pub id: String,
    pub user_id: String,
    pub source: String,
    pub read_only: bool,
    pub external_url: Option<String>,
}

impl WritableCalendar {
    pub fn from_row(r: &Row) -> Self {
        WritableCalendar {
            id: r.get::<_, uuid::Uuid>("id").to_string(),
            user_id: r.get::<_, uuid::Uuid>("user_id").to_string(),
            source: r.get("source"),
            read_only: r.get("read_only"),
            external_url: r.get("external_url"),
        }
    }

    pub async fn load(db: &Db, id: &str, user_id: &str) -> inboxora_core::Result<Option<Self>> {
        let row = db
            .opt(
                "SELECT id, user_id, source, read_only, external_url FROM calendars WHERE id = $1 AND user_id = $2",
                params![id, user_id],
            )
            .await?;
        Ok(row.as_ref().map(Self::from_row))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Put,
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Skipped,
    Ok,
    Conflict(String),
    Error(String),
}

async fn apply_remote_conflict(
    db: &Db,
    calendar_id: &str,
    user_id: &str,
    uid: &str,
    href: &str,
    remote: &dav_client::Remote,
) -> std::result::Result<(), String> {
    let Some(ical) = remote.ical.as_deref() else {
        db.exec(
            "DELETE FROM calendar_events WHERE calendar_id = $1 AND uid = $2 AND recurrence_id = $3",
            params![calendar_id, uid, ""],
        )
        .await
        .map_err(db_err)?;
        return Ok(());
    };
    let Some(event) = parse_calendar_event(ical) else { return Ok(()) };
    let id: uuid::Uuid = calendar_id.parse().map_err(|_| "Invalid calendar id".to_owned())?;
    let encrypted = crypto::encrypt(href)?;
    db.exec(UPSERT_EVENT, &event_params(id, user_id, &event, Some(encrypted), remote.etag.clone()))
        .await
        .map_err(db_err)?;
    Ok(())
}

/// `pushEventToRemote`: mirror a local change to a writable CalDAV calendar.
pub async fn push_event_to_remote(
    db: &Db,
    calendar: &WritableCalendar,
    uid: &str,
    raw_ical: Option<&str>,
    action: Action,
    remote: Option<(Option<String>, Option<String>)>,
) -> Outcome {
    static EXTERNAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^source:([^:]+)(?::(.+))?$").unwrap());
    if calendar.source != "caldav" || calendar.read_only {
        return Outcome::Skipped;
    }
    let Some(external) = calendar.external_url.as_deref().filter(|e| !e.is_empty()) else { return Outcome::Skipped };
    let Some(m) = EXTERNAL.captures(external) else { return Outcome::Skipped };
    let (source_id, hash) = (m[1].to_owned(), m.get(2).map(|h| h.as_str().to_owned()));
    let run = async {
        let row = db
            .opt(
                "SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2",
                params![&source_id, &calendar.user_id],
            )
            .await
            .map_err(db_err)?;
        let Some(source) = row.as_ref().map(Source::from_row) else { return Ok::<_, String>(Outcome::Skipped) };
        if source.kind != "caldav" || !source.writable {
            return Ok(Outcome::Skipped);
        }
        let collection_href = match &hash {
            Some(h) => parse_collections(source.collections.as_deref())
                .unwrap_or_default()
                .iter()
                .find(|c| str_field(c, "hrefHash") == Some(h.as_str()))
                .and_then(|c| str_field(c, "href").map(str::to_owned))
                .filter(|h| !h.is_empty()),
            None => decrypt_or_empty(source.url.as_deref()).filter(|u| !u.is_empty()),
        };
        let Some(collection_href) = collection_href else {
            return Ok(Outcome::Error("The remote calendar location is unavailable".into()));
        };
        let password = decrypt_or_empty(source.password.as_deref()).filter(|p| !p.is_empty());
        let (Some(username), Some(password)) = (source.username.clone().filter(|u| !u.is_empty()), password) else {
            return Ok(Outcome::Error("Stored calendar source credentials are unavailable".into()));
        };
        let policy = dav_client::connection_policy(db).await.map_err(db_err)?;
        let creds = Creds { username, password, allow_private: policy.allow_private_hosts };
        let (remote_href, remote_etag) = match remote {
            Some(r) => r,
            None => {
                let row = db
                    .opt(
                        "SELECT remote_href, remote_etag FROM calendar_events WHERE calendar_id = $1 AND uid = $2 AND recurrence_id = $3",
                        params![&calendar.id, uid, ""],
                    )
                    .await
                    .map_err(db_err)?;
                let href = row
                    .as_ref()
                    .and_then(|r| r.get::<_, Option<String>>("remote_href"))
                    .filter(|h| !h.is_empty())
                    .and_then(|h| crypto::decrypt(&h));
                let etag =
                    row.as_ref().and_then(|r| r.get::<_, Option<String>>("remote_etag")).filter(|e| !e.is_empty());
                (href, etag)
            }
        };
        if action == Action::Delete {
            let Some(href) = remote_href else { return Ok(Outcome::Ok) };
            return Ok(match dav_client::delete_calendar_event(&href, remote_etag.as_deref(), &creds).await? {
                WriteBack::Ok { .. } => Outcome::Ok,
                WriteBack::Conflict { remote } => {
                    apply_remote_conflict(db, &calendar.id, &calendar.user_id, uid, &href, &remote).await?;
                    Outcome::Conflict(if remote.ical.is_some() {
                        "The event changed on the server, so your delete was not applied.".into()
                    } else {
                        "The event was already removed on the server.".into()
                    })
                }
                WriteBack::Error(e) => Outcome::Error(e),
            });
        }
        let href = remote_href.clone().unwrap_or_else(|| format!("{collection_href}{}.ics", encode_uri_component(uid)));
        // Known resources update with If-Match; new ones (or an unknown etag) create with If-None-Match.
        let if_match = if remote_href.is_some() { remote_etag.as_deref() } else { None };
        Ok(match dav_client::put_calendar_event(&href, raw_ical.unwrap_or(""), if_match, &creds).await? {
            WriteBack::Ok { etag } => {
                let encrypted = crypto::encrypt(&href)?;
                db.exec(
                    "UPDATE calendar_events SET remote_href = $1, remote_etag = $2 WHERE calendar_id = $3 AND uid = $4 AND recurrence_id = $5",
                    params![encrypted, etag, &calendar.id, uid, ""],
                )
                .await
                .map_err(db_err)?;
                Outcome::Ok
            }
            WriteBack::Conflict { remote } => {
                apply_remote_conflict(db, &calendar.id, &calendar.user_id, uid, &href, &remote).await?;
                Outcome::Conflict("The server had a newer version of this event, so your change was not saved. The event now shows the server’s version.".into())
            }
            WriteBack::Error(e) => Outcome::Error(e),
        })
    };
    match run.await {
        Ok(o) => o,
        Err(e) => Outcome::Error(if e.is_empty() { "The remote calendar could not be reached".into() } else { e }),
    }
}

// ── live collections ─────────────────────────────────────────────────────────────────

/// Sources served live (never stored): iCalendar feeds and read-only CalDAV. Two-way CalDAV
/// calendars keep their events in `calendar_events` so edits and invitations work offline.
pub fn is_live(source: &Source) -> bool {
    source.kind == "ical_url" || !source.writable
}

/// One event resource of a live calendar.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LiveEvent {
    pub raw: String,
    pub href: Option<String>,
    pub etag: Option<String>,
}

/// One vCard of a live address book.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LiveCard {
    pub vcard: String,
    pub href: String,
    pub etag: String,
}

fn redact(message: String, source: &Source, secrets: &[String]) -> String {
    let mut safe = if message.is_empty() { "Remote calendar request failed".to_owned() } else { message };
    for secret in source.url.iter().chain(secrets.iter()).filter(|s| !s.is_empty()) {
        safe = safe.replace(secret.as_str(), "[redacted]");
    }
    safe
}

/// Fetch a live calendar collection (`href_hash` `None` = the source's single legacy feed).
pub async fn fetch_live_calendar(
    db: &Db,
    source: &Source,
    href_hash: Option<&str>,
) -> std::result::Result<Vec<LiveEvent>, String> {
    let c = collections_to_sync(source)
        .into_iter()
        .find(|c| match href_hash {
            Some(h) => c.href_hash == h,
            None => c.legacy,
        })
        .ok_or_else(|| "Calendar collection is no longer selected".to_owned())?;
    let policy = dav_client::connection_policy(db).await.map_err(db_err)?;
    let ctx = Ctx::detached("Calendar source removed");
    let mut secrets = Vec::new();
    let (payloads, _) =
        fetch_events(source, &c.href, policy, &ctx, &mut secrets).await.map_err(|e| redact(e, source, &secrets))?;
    Ok(payloads.into_iter().map(|p| LiveEvent { raw: p.ical, href: p.href, etag: p.etag }).collect())
}

/// Fetch a live CardDAV address book by its collection hash.
pub async fn fetch_live_book(db: &Db, source: &Source, href_hash: &str) -> std::result::Result<Vec<LiveCard>, String> {
    let collections = parse_collections(source.address_book_collections.as_deref()).unwrap_or_default();
    let href = collections
        .iter()
        .find(|c| str_field(c, "hrefHash") == Some(href_hash))
        .and_then(|c| str_field(c, "href"))
        .ok_or_else(|| "Address book is no longer selected".to_owned())?
        .to_owned();
    let policy = dav_client::connection_policy(db).await.map_err(db_err)?;
    let Some(username) = source.username.clone() else { return Err("CardDAV username is missing".to_owned()) };
    let Some(password) = decrypt_or_empty(source.password.as_deref()) else {
        return Err("CardDAV password could not be decrypted".to_owned());
    };
    let creds = Creds { username, password, allow_private: policy.allow_private_hosts };
    let cards = dav_client::fetch_address_book_cards(&href, &creds).await?;
    Ok(cards.into_iter().map(|c| LiveCard { vcard: c.vcard, href: c.href, etag: c.etag }).collect())
}

/// The contacts-table fields of a vCard (shared by stored and live address books).
pub fn contact_fields(vcard: &str, href: &str) -> serde_json::Map<String, Value> {
    let c = contact_from_vcard(vcard, href);
    let p = &c.parsed;
    let mut m = serde_json::Map::new();
    m.insert("uid".into(), json!(c.uid));
    m.insert("vcard".into(), json!(c.vcard));
    m.insert("etag".into(), json!(crypto::md5_hex(&c.vcard)));
    m.insert("display_name".into(), json!(c.display_name));
    m.insert("first_name".into(), json!(p.first_name));
    m.insert("last_name".into(), json!(p.last_name));
    m.insert("primary_email".into(), json!(c.primary_email));
    m.insert("emails".into(), serde_json::to_value(&p.emails).unwrap_or_default());
    m.insert("phones".into(), serde_json::to_value(&p.phones).unwrap_or_default());
    m.insert("organization".into(), json!(p.organization));
    m.insert("notes".into(), json!(p.notes));
    m.insert("birthday".into(), json!(p.birthday));
    m.insert("anniversary".into(), json!(p.anniversary));
    m.insert("contact_dates".into(), serde_json::to_value(&p.contact_dates).unwrap_or_default());
    m.insert("photo_data".into(), json!(p.photo_data));
    m.insert("title".into(), json!(p.title));
    m.insert("role".into(), json!(p.role));
    m.insert("nickname".into(), json!(p.nickname));
    m.insert("urls".into(), serde_json::to_value(&p.urls).unwrap_or_default());
    m.insert("instant_messages".into(), serde_json::to_value(&p.instant_messages).unwrap_or_default());
    m.insert("categories".into(), serde_json::to_value(&p.categories).unwrap_or_default());
    m.insert("addresses".into(), serde_json::to_value(&p.addresses).unwrap_or_default());
    m
}

// ── CardDAV address books ────────────────────────────────────────────────────────────

async fn ensure_book(
    db: &Db,
    user_id: &str,
    source_id: &str,
    href: &str,
    name: &str,
) -> std::result::Result<uuid::Uuid, String> {
    let external = format!("source:{source_id}:{}", inboxora_mail::util::href_hash(href));
    if let Some(r) = db
        .opt("SELECT id FROM address_books WHERE user_id = $1 AND external_url = $2", params![user_id, &external])
        .await
        .map_err(db_err)?
    {
        return Ok(r.get("id"));
    }
    for attempt in 0..20 {
        let n = if attempt == 0 { name.to_owned() } else { format!("{name} ({})", attempt + 1) };
        match db
            .opt("INSERT INTO address_books (user_id, name, source, external_url) VALUES ($1, $2, 'carddav', $3) RETURNING id", params![user_id, n, &external])
            .await
        {
            Ok(Some(r)) => return Ok(r.get("id")),
            Ok(None) => break,
            Err(e) if e.is_unique_violation() => {}
            Err(e) => return Err(db_err(e)),
        }
    }
    Err(format!("Could not create a local address book for \"{name}\""))
}

struct RemoteContact {
    uid: String,
    primary_email: Option<String>,
    display_name: Option<String>,
    parsed: inboxora_pim::vcard::ParsedVCard,
    vcard: String,
}

fn contact_from_vcard(vcard: &str, href: &str) -> RemoteContact {
    let parsed = inboxora_pim::vcard::parse(vcard);
    let uid = parsed.uid.clone().filter(|u| !u.is_empty()).unwrap_or_else(|| crypto::md5_hex(href));
    let raw_primary = parsed
        .emails
        .iter()
        .find(|e| e.primary)
        .map(|e| e.value.clone())
        .filter(|v| !v.is_empty())
        .or_else(|| parsed.emails.first().map(|e| e.value.clone()).filter(|v| !v.is_empty()));
    RemoteContact {
        uid,
        display_name: parsed.display_name.clone().filter(|d| !d.is_empty()).or_else(|| raw_primary.clone()),
        primary_email: raw_primary.map(|p| inboxora_mail::text::js_trim(&p.to_lowercase()).to_owned()),
        parsed,
        vcard: vcard.to_owned(),
    }
}

async fn sync_book(
    db: &Db,
    user_id: &str,
    source_id: &str,
    href: &str,
    name: &str,
    creds: &Creds,
    ctx: &Ctx,
) -> std::result::Result<usize, String> {
    let raw = ctx.guard(dav_client::fetch_address_book_cards(href, creds)).await?;
    ctx.check()?;
    let cards: Vec<RemoteContact> = raw.iter().map(|c| contact_from_vcard(&c.vcard, &c.href)).collect();
    if cards.iter().any(|c| !c.parsed.invalid_dates.is_empty() || !c.parsed.invalid_date_labels.is_empty()) {
        return Err("Remote CardDAV vCard contains an invalid contact date".into());
    }
    let book_id = ensure_book(db, user_id, source_id, href, name).await?;
    ctx.check()?;
    // Address books are served live: drop rows an earlier sync stored and warm the cache.
    db.exec("DELETE FROM contacts WHERE address_book_id = $1", params![book_id]).await.map_err(db_err)?;
    let live: Vec<LiveCard> =
        raw.iter().map(|c| LiveCard { vcard: c.vcard.clone(), href: c.href.clone(), etag: c.etag.clone() }).collect();
    super::remote_dav::store_book(book_id, &live).await;
    Ok(cards.len())
}

async fn sync_address_books(db: &Db, source: &Source) -> Value {
    let Some((ctx, done)) = ADDRESS_BOOKS.begin(&source.id) else {
        return json!({ "ok": false, "error": "A sync is already in progress" });
    };
    let id = source.id.clone();
    let run = async {
        let collections: Vec<Value> = parse_collections(source.address_book_collections.as_deref())
            .unwrap_or_default()
            .into_iter()
            .filter(|c| truthy(c.get("selected")))
            .collect();
        if collections.is_empty() {
            db.exec("UPDATE calendar_import_sources SET address_book_last_sync_at = NOW(), address_book_last_error = NULL WHERE id = $1", params![&id])
                .await
                .map_err(db_err)?;
            return Ok(json!({ "ok": true, "bookCount": 0, "contactCount": 0 }));
        }
        let policy = dav_client::connection_policy(db).await.map_err(db_err)?;
        let Some(username) = source.username.clone() else { return Err("CardDAV username is missing".to_owned()) };
        let Some(password) = decrypt_or_empty(source.password.as_deref()) else {
            return Err("CardDAV password could not be decrypted".to_owned());
        };
        let creds = Creds { username, password, allow_private: policy.allow_private_hosts };
        let mut contacts = 0;
        let mut seen_urls: Vec<String> = Vec::new();
        for c in &collections {
            ctx.check()?;
            let href = str_field(c, "href").unwrap_or_default();
            let name = str_field(c, "displayName").unwrap_or_default();
            contacts += sync_book(db, &source.user_id, &id, href, name, &creds, &ctx).await?;
            seen_urls.push(format!("source:{id}:{}", str_field(c, "hrefHash").unwrap_or("undefined")));
        }
        ctx.check()?;
        if seen_urls.is_empty() {
            seen_urls.push(String::new());
        }
        db.exec(
            "DELETE FROM address_books WHERE user_id = $1 AND source = 'carddav' AND external_url LIKE $2 AND external_url <> ALL($3::text[])",
            params![&source.user_id, format!("source:{id}:%"), seen_urls],
        )
        .await
        .map_err(db_err)?;
        db.exec("UPDATE calendar_import_sources SET address_book_last_sync_at = NOW(), address_book_last_error = NULL WHERE id = $1", params![&id])
            .await
            .map_err(db_err)?;
        Ok(json!({ "ok": true, "bookCount": collections.len(), "contactCount": contacts }))
    };
    let result: SyncResult = run.await;
    let out = match result {
        Ok(v) => v,
        Err(_) if ctx.removed.load(Ordering::SeqCst) => json!({ "ok": false, "error": "DAV server removed" }),
        Err(message) => {
            if let Err(e) = db
                .exec("UPDATE calendar_import_sources SET address_book_last_sync_at = NOW(), address_book_last_error = $2 WHERE id = $1", params![&id, &message])
                .await
            {
                tracing::error!("CardDAV sync: could not record the error: {e}");
            }
            json!({ "ok": false, "error": message })
        }
    };
    ADDRESS_BOOKS.finish(&id, &done);
    out
}

async fn run_carddav_sync(db: &Db, source: &Source) -> Value {
    if source.id.is_empty() {
        return json!({ "ok": false, "error": "DAV server is incomplete" });
    }
    if ADDRESS_BOOKS.is_stopped(&source.id) {
        return json!({ "ok": false, "error": "DAV server removed" });
    }
    sync_address_books(db, source).await
}

/// `syncCarddavSide(userId, sourceId)`.
pub async fn sync_carddav_side(db: &Db, user_id: &str, source_id: &str) -> inboxora_core::Result<Value> {
    let row = db
        .opt(
            "SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2 AND enabled = true",
            params![source_id, user_id],
        )
        .await?;
    let Some(row) = row else { return Ok(json!({ "ok": false, "error": "DAV server not found" })) };
    Ok(run_carddav_sync(db, &Source::from_row(&row)).await)
}

/// `scheduleCarddavSide(source)`.
pub fn schedule_carddav_side(db: &Db, source: &Source) {
    if source.id.is_empty() {
        return;
    }
    ADDRESS_BOOKS.set_timer(&source.id, None);
    let Some(seconds) = source.interval_seconds.filter(|s| *s > 0) else { return };
    if source.address_book_collections.as_deref().is_none_or(str::is_empty) {
        return;
    }
    let (db2, s2) = (db.clone(), source.clone());
    let handle = spawn_interval(seconds, move || {
        let (db, s) = (db2.clone(), s2.clone());
        async move {
            run_carddav_sync(&db, &s).await;
        }
    });
    ADDRESS_BOOKS.set_timer(&source.id, Some(handle));
}

/// `startCarddavSchedulers()`.
pub async fn start_carddav_schedulers(db: &Db) -> inboxora_core::Result<()> {
    for row in db
        .rows(
            "SELECT * FROM calendar_import_sources WHERE enabled = true AND address_book_collections IS NOT NULL",
            params![],
        )
        .await?
    {
        schedule_carddav_side(db, &Source::from_row(&row));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ical_url_payloads_split_events_with_their_zones() {
        assert_eq!(calendar_payloads("  ").unwrap_err(), "Remote calendar did not contain any VEVENT components");
        let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a\r\nDTSTART:20260101T000000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(calendar_payloads(raw).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stopping_a_source_cancels_and_waits_for_its_sync() {
        let reg = Registry::new("gone");
        let (ctx, done) = reg.begin("s1").unwrap();
        assert!(reg.begin("s1").is_none(), "one sync per source");
        let worker = tokio::spawn(async move {
            let r = ctx.guard(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok::<_, String>(())
            });
            r.await
        });
        let stopper = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            reg.stop("s1").await;
        };
        let finisher = async {
            let r = worker.await.unwrap();
            assert_eq!(r.unwrap_err(), "gone");
            reg.finish("s1", &done);
        };
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(stopper, finisher) }).await.unwrap();
        assert!(reg.is_stopped("s1"));
        reg.release("s1");
        assert!(!reg.is_stopped("s1"));
    }
}
