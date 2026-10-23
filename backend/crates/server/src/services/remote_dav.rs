//! Read-only remote collections — iCalendar feeds, read-only CalDAV calendars and CardDAV
//! address books — are never stored. Their resources are fetched from the remote server and
//! kept in Redis for `MAIL_CACHE_TTL_SECONDS`; this module turns those snapshots into rows
//! shaped like `calendar_events` / `contacts` so read paths treat them like stored rows.
//! (Two-way CalDAV calendars stay in `calendar_events`.)

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use fred::prelude::Pool;
use inboxora_core::pg::{Db, Q, Val};
use inboxora_core::{crypto, params};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::dav_sync::{self, LiveCard, LiveEvent, Source};
use crate::engine::cache::ContentCache;

static CACHE: OnceLock<ContentCache> = OnceLock::new();
static DISABLED: LazyLock<ContentCache> = LazyLock::new(|| ContentCache::new(None));
/// After a failed fetch a collection is not retried for a minute, so an unreachable remote
/// does not stall every calendar or contacts view for the request timeout.
static FAILED: LazyLock<Mutex<HashMap<Uuid, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Parsed rows of each live collection, kept for the cache TTL so a request does not pull and
/// re-parse whole snapshots (vCards carry their photos).
struct Memo {
    at: Instant,
    rows: std::sync::Arc<Vec<Map<String, Value>>>,
}

static ROWS: LazyLock<Mutex<HashMap<Uuid, Memo>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Contact photos by address, for avatars (bounded).
static PHOTOS: LazyLock<Mutex<(HashMap<String, Option<String>>, std::collections::VecDeque<String>)>> =
    LazyLock::new(|| Mutex::new((HashMap::new(), std::collections::VecDeque::new())));
const PHOTO_CAP: usize = 256;

fn ttl() -> Duration {
    let secs = inboxora_core::config::env("MAIL_CACHE_TTL_SECONDS").and_then(|v| v.parse().ok()).filter(|v| *v > 0);
    Duration::from_secs(secs.unwrap_or(600))
}

fn memo_get(id: Uuid) -> Option<std::sync::Arc<Vec<Map<String, Value>>>> {
    let ttl = ttl();
    let mut m = ROWS.lock();
    m.retain(|_, e| e.at.elapsed() < ttl);
    m.get(&id).map(|e| e.rows.clone())
}

fn memo_put(id: Uuid, rows: Vec<Map<String, Value>>) -> std::sync::Arc<Vec<Map<String, Value>>> {
    let rows = std::sync::Arc::new(rows);
    ROWS.lock().insert(id, Memo { at: Instant::now(), rows: rows.clone() });
    rows
}

fn invalidate(id: Uuid) {
    ROWS.lock().remove(&id);
}

pub fn init(redis: Pool) {
    let _ = CACHE.set(ContentCache::new(Some(redis)));
}

fn cache() -> &'static ContentCache {
    CACHE.get().unwrap_or(&DISABLED)
}

fn calendar_key(id: Uuid) -> String {
    format!("dav:cal:{id}")
}

fn book_key(id: Uuid) -> String {
    format!("dav:book:{id}")
}

pub async fn store_calendar(id: Uuid, events: &[LiveEvent]) {
    FAILED.lock().remove(&id);
    invalidate(id);
    cache().put(&calendar_key(id), &events).await;
}

pub async fn store_book(id: Uuid, cards: &[LiveCard]) {
    FAILED.lock().remove(&id);
    invalidate(id);
    PHOTOS.lock().0.clear();
    cache().put(&book_key(id), &cards).await;
}

/// A deterministic UUID for a resource of a live collection (stable across fetches).
pub fn stable_id(collection: Uuid, key: &str) -> Uuid {
    let digest = crypto::sha256_hex(format!("{collection}\u{0}{key}"));
    let mut b = [0u8; 16];
    for (i, byte) in b.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digest[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    Uuid::from_bytes(b)
}

/// `source:<id>` (a single feed) or `source:<id>:<collection href hash>`.
fn parse_external(url: &str) -> Option<(String, Option<String>)> {
    let rest = url.strip_prefix("source:")?;
    Some(match rest.split_once(':') {
        Some((id, hash)) => (id.to_owned(), Some(hash.to_owned())),
        None => (rest.to_owned(), None),
    })
}

async fn load_source(db: &Db, id: &str, user_id: &str) -> Option<Source> {
    if !inboxora_mail::util::is_uuid(id) {
        return None;
    }
    db.opt(
        "SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2 AND enabled = true",
        params![id, user_id],
    )
    .await
    .ok()
    .flatten()
    .map(|r| Source::from_row(&r))
}

fn recently_failed(id: Uuid) -> bool {
    let mut f = FAILED.lock();
    match f.get(&id) {
        Some(t) if t.elapsed() < RETRY_AFTER => true,
        Some(_) => {
            f.remove(&id);
            false
        }
        None => false,
    }
}

fn note_failure(id: Uuid) {
    let mut f = FAILED.lock();
    f.retain(|_, t| t.elapsed() < RETRY_AFTER);
    f.insert(id, Instant::now());
}

// ── calendars ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LiveCalendar {
    pub id: Uuid,
    pub user_id: String,
    pub name: Option<String>,
    pub color: Option<String>,
    pub source: String,
    pub external_url: String,
}

/// The user's live calendars (optionally only `selected` ones).
pub async fn live_calendars(db: &Db, user_id: &str, selected: Option<&[String]>) -> Vec<LiveCalendar> {
    let rows = db
        .rows(
            "SELECT id, user_id, name, color, source, external_url FROM calendars \
             WHERE user_id = $1 AND owner_user_id = $1 AND source IN ('ical_url', 'caldav') AND read_only \
               AND external_url LIKE 'source:%' AND ($2::uuid[] IS NULL OR id = ANY($2::uuid[]))",
            params![user_id, selected.map(<[String]>::to_vec)],
        )
        .await
        .unwrap_or_default();
    rows.iter()
        .map(|r| LiveCalendar {
            id: r.get("id"),
            user_id: r.get::<_, Uuid>("user_id").to_string(),
            name: r.get("name"),
            color: r.get("color"),
            source: r.get("source"),
            external_url: r.get("external_url"),
        })
        .collect()
}

/// A live calendar's resources: the cached snapshot, else fetched (then cached).
pub async fn calendar_resources(db: &Db, cal: &LiveCalendar) -> Vec<LiveEvent> {
    let key = calendar_key(cal.id);
    if let Some(events) = cache().get::<Vec<LiveEvent>>(&key).await {
        return events;
    }
    if recently_failed(cal.id) {
        return Vec::new();
    }
    let Some((source_id, hash)) = parse_external(&cal.external_url) else { return Vec::new() };
    let Some(source) = load_source(db, &source_id, &cal.user_id).await else { return Vec::new() };
    match dav_sync::fetch_live_calendar(db, &source, hash.as_deref()).await {
        Ok(events) => {
            store_calendar(cal.id, &events).await;
            events
        }
        Err(e) => {
            tracing::warn!("Live calendar {} could not be fetched: {e}", cal.id);
            note_failure(cal.id);
            Vec::new()
        }
    }
}

/// A `calendar_events`-shaped row (with the calendar source columns) for one resource.
fn event_row(cal: &LiveCalendar, e: &LiveEvent) -> Option<Map<String, Value>> {
    let parsed = inboxora_pim::ical::parse_calendar_event(&e.raw)?;
    let iso = |t: &chrono::DateTime<chrono::Utc>| json!(inboxora_core::json::js_iso(t));
    let recurring = {
        let upper = e.raw.to_uppercase();
        ["RRULE", "RDATE", "RECURRENCE-ID"]
            .iter()
            .any(|k| upper.contains(&format!("{k}:")) || upper.contains(&format!("{k};")))
    };
    let mut m = Map::new();
    m.insert("id".into(), json!(stable_id(cal.id, &parsed.uid).to_string()));
    m.insert("user_id".into(), json!(cal.user_id));
    m.insert("calendar_id".into(), json!(cal.id.to_string()));
    m.insert("uid".into(), json!(parsed.uid));
    m.insert("etag".into(), json!(e.etag.clone().unwrap_or_else(|| crypto::sha256_hex(&e.raw))));
    m.insert("raw_ical".into(), json!(e.raw));
    m.insert("summary".into(), json!(parsed.summary));
    m.insert("description".into(), json!(parsed.description));
    m.insert("location".into(), json!(parsed.location));
    m.insert("url".into(), json!(parsed.url));
    m.insert("organizer".into(), json!(parsed.organizer));
    m.insert("attendees".into(), json!(parsed.attendees));
    m.insert("starts_at".into(), iso(&parsed.starts_at));
    m.insert("ends_at".into(), iso(&parsed.ends_at));
    m.insert("all_day".into(), json!(parsed.all_day));
    m.insert("timezone".into(), json!(parsed.time_zone));
    m.insert("recurring".into(), json!(recurring));
    m.insert("cancellation_outbox_id".into(), Value::Null);
    m.insert("invite_account_id".into(), Value::Null);
    m.insert("invitation_sequence".into(), json!(0));
    m.insert("remote_href".into(), json!(e.href));
    m.insert("remote_etag".into(), json!(e.etag));
    m.insert("source_message_id".into(), Value::Null);
    m.insert("source_folder".into(), Value::Null);
    m.insert("source_account_id".into(), Value::Null);
    m.insert("calendar_name".into(), json!(cal.name));
    m.insert("calendar_color".into(), json!(cal.color));
    m.insert("source".into(), json!(cal.source));
    m.insert("read_only".into(), json!(true));
    Some(m)
}

/// Event rows of the user's live calendars (optionally only `selected` ones).
pub async fn event_rows(db: &Db, user_id: &str, selected: Option<&[String]>) -> Vec<Map<String, Value>> {
    let calendars = live_calendars(db, user_id, selected).await;
    let per_calendar = futures_util::future::join_all(calendars.iter().map(|cal| async move {
        if let Some(rows) = memo_get(cal.id) {
            return rows;
        }
        let events = calendar_resources(db, cal).await;
        memo_put(cal.id, events.iter().filter_map(|e| event_row(cal, e)).collect())
    }))
    .await;
    per_calendar.iter().flat_map(|rows| rows.iter().cloned()).collect()
}

/// One live event by its stable id.
pub async fn event_by_id(db: &Db, user_id: &str, id: &str) -> Option<Map<String, Value>> {
    event_rows(db, user_id, None).await.into_iter().find(|r| r.get("id").and_then(Value::as_str) == Some(id))
}

// ── address books ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LiveBook {
    pub id: Uuid,
    pub user_id: String,
    pub external_url: String,
}

pub async fn live_books(db: &Db, user_id: &str, only: Option<&str>) -> Vec<LiveBook> {
    db.rows(
        "SELECT id, user_id, external_url FROM address_books WHERE user_id = $1 AND source = 'carddav' \
         AND external_url LIKE 'source:%' AND ($2::uuid IS NULL OR id = $2::uuid)",
        params![user_id, only.filter(|b| inboxora_mail::util::is_uuid(b))],
    )
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| LiveBook {
        id: r.get("id"),
        user_id: r.get::<_, Uuid>("user_id").to_string(),
        external_url: r.get("external_url"),
    })
    .collect()
}

/// A live address book's vCards: the cached snapshot, else fetched (then cached).
pub async fn book_cards(db: &Db, book: &LiveBook) -> Vec<LiveCard> {
    let key = book_key(book.id);
    if let Some(cards) = cache().get::<Vec<LiveCard>>(&key).await {
        return cards;
    }
    if recently_failed(book.id) {
        return Vec::new();
    }
    let Some((source_id, Some(hash))) = parse_external(&book.external_url) else { return Vec::new() };
    let Some(source) = load_source(db, &source_id, &book.user_id).await else { return Vec::new() };
    match dav_sync::fetch_live_book(db, &source, &hash).await {
        Ok(cards) => {
            store_book(book.id, &cards).await;
            cards
        }
        Err(e) => {
            tracing::warn!("Live address book {} could not be fetched: {e}", book.id);
            note_failure(book.id);
            Vec::new()
        }
    }
}

/// `contacts`-shaped rows of one live address book. Photos are reduced to a marker; see
/// `contact_photo`.
async fn book_rows(db: &Db, book: &LiveBook) -> std::sync::Arc<Vec<Map<String, Value>>> {
    if let Some(rows) = memo_get(book.id) {
        return rows;
    }
    let cards = book_cards(db, book).await;
    let mut out = Vec::with_capacity(cards.len());
    {
        let book = &book;
        // A primary address belongs to one contact per book (the stored sync kept the first).
        let mut seen = HashSet::new();
        for card in cards {
            let mut m = dav_sync::contact_fields(&card.vcard, &card.href);
            let uid = m.get("uid").and_then(Value::as_str).unwrap_or_default().to_owned();
            if let Some(e) = m.get("primary_email").and_then(Value::as_str).filter(|e| !e.is_empty())
                && !seen.insert(e.to_owned())
            {
                m.insert("primary_email".into(), Value::Null);
            }
            let filename = card
                .href
                .rsplit('/')
                .find(|s| !s.is_empty())
                .map(|f| crate::routes::dav::decode_uri_component(f).unwrap_or_else(|| f.to_owned()));
            m.insert("id".into(), json!(stable_id(book.id, &uid).to_string()));
            m.insert("address_book_id".into(), json!(book.id.to_string()));
            m.insert("user_id".into(), json!(book.user_id));
            m.insert("is_auto".into(), json!(false));
            m.insert("send_count".into(), json!(0));
            m.insert("dav_filename".into(), json!(filename));
            if m.get("photo_data").is_some_and(|p| !p.is_null()) {
                m.insert("photo_data".into(), json!("live"));
            }
            out.push(m);
        }
    }
    memo_put(book.id, out)
}

/// `contacts`-shaped rows for the user's live address books (optionally one book).
pub async fn contact_rows(db: &Db, user_id: &str, only: Option<&str>) -> Vec<Map<String, Value>> {
    let books = live_books(db, user_id, only).await;
    let per_book = futures_util::future::join_all(books.iter().map(|b| book_rows(db, b))).await;
    per_book.iter().flat_map(|rows| rows.iter().cloned()).collect()
}

/// The vCards of the user's live address books (for export and CardDAV).
pub async fn contact_cards(db: &Db, user_id: &str, only: Option<&str>) -> Vec<(Uuid, LiveCard)> {
    let books = live_books(db, user_id, only).await;
    let per_book =
        futures_util::future::join_all(books.iter().map(|b| async move { (b.id, book_cards(db, b).await) })).await;
    per_book.into_iter().flat_map(|(id, cards)| cards.into_iter().map(move |c| (id, c))).collect()
}

/// The `contacts` relation with the user's live address books appended, for `FROM {rel} c`.
/// The snapshot travels as the JSON parameter `$param`; photos are reduced to a marker (the
/// photo endpoint reads them from the snapshot) and vCard bodies are left out.
pub async fn contacts_relation(db: &Db, user_id: &str, param: usize) -> (String, Val) {
    let rows: Vec<Value> = contact_rows(db, user_id, None)
        .await
        .into_iter()
        .map(|mut m| {
            m.remove("vcard");
            Value::Object(m)
        })
        .collect();
    let sql = format!(
        "(SELECT id, address_book_id, user_id, uid, vcard, etag, display_name, first_name, last_name, primary_email, emails, \
           phones, organization, notes, photo_data, is_auto, send_count, last_sent, created_at, updated_at, birthday, anniversary, \
           title, role, nickname, urls, addresses, instant_messages, categories, contact_dates, dav_filename FROM contacts \
         UNION ALL \
         SELECT id, address_book_id, user_id, uid, vcard, etag, display_name, first_name, last_name, primary_email, emails, \
           phones, organization, notes, photo_data, is_auto, send_count, last_sent, created_at, updated_at, birthday, anniversary, \
           title, role, nickname, urls, addresses, instant_messages, categories, contact_dates, dav_filename \
         FROM jsonb_to_recordset(${param}::jsonb) AS r(id uuid, address_book_id uuid, user_id uuid, uid text, vcard text, etag text, \
           display_name text, first_name text, last_name text, primary_email text, emails jsonb, phones jsonb, organization text, \
           notes text, photo_data text, is_auto boolean, send_count integer, last_sent timestamptz, created_at timestamptz, \
           updated_at timestamptz, birthday date, anniversary date, title text, role text, nickname text, urls jsonb, \
           addresses jsonb, instant_messages jsonb, categories jsonb, contact_dates jsonb, dav_filename text))"
    );
    (sql, Val::from(Value::Array(rows).to_string()))
}

/// The photo of a live contact with this primary address.
pub async fn contact_photo(db: &Db, user_id: &str, email: &str) -> Option<String> {
    let email = email.trim().to_lowercase();
    let key = format!("{user_id}\u{0}{email}");
    if let Some(hit) = PHOTOS.lock().0.get(&key) {
        return hit.clone();
    }
    let has = contact_rows(db, user_id, None).await.iter().any(|m| {
        m.get("primary_email").and_then(Value::as_str) == Some(email.as_str())
            && m.get("photo_data").is_some_and(|p| !p.is_null())
    });
    let photo = if has {
        contact_cards(db, user_id, None).await.into_iter().find_map(|(_, card)| {
            let m = dav_sync::contact_fields(&card.vcard, &card.href);
            (m.get("primary_email").and_then(Value::as_str) == Some(email.as_str()))
                .then(|| m.get("photo_data").and_then(Value::as_str).map(str::to_owned))
                .flatten()
        })
    } else {
        None
    };
    let mut p = PHOTOS.lock();
    if p.0.len() >= PHOTO_CAP
        && let Some(oldest) = p.1.pop_front()
    {
        p.0.remove(&oldest);
    }
    if p.0.insert(key.clone(), photo.clone()).is_none() {
        p.1.push_back(key);
    }
    photo
}

/// Whether `id` is a contact of a live (read-only) address book.
pub async fn is_live_contact(db: &Db, user_id: &str, id: &str) -> bool {
    contact_rows(db, user_id, None).await.iter().any(|m| m.get("id").and_then(Value::as_str) == Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const FEED: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\n\
BEGIN:VEVENT\r\nUID:one@test\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20261024T090000Z\r\nDTEND:20261024T100000Z\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:two@test\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20261025T090000Z\r\nDTEND:20261025T100000Z\r\nSUMMARY:Review\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

    /// One-shot HTTP server answering every request with the feed.
    async fn serve_feed() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/calendar\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{FEED}",
                    FEED.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        addr
    }

    const CARDS: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:carddav\">\
<D:response><D:href>/books/a/ada.vcf</D:href><D:propstat><D:prop><D:getetag>\"e1\"</D:getetag><C:address-data>BEGIN:VCARD\r\nVERSION:3.0\r\nUID:ada\r\nFN:Ada Lovelace\r\nEMAIL;TYPE=INTERNET:ada@example.test\r\nBDAY:1815-12-10\r\nEND:VCARD\r\n</C:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\
<D:response><D:href>/books/a/alan.vcf</D:href><D:propstat><D:prop><D:getetag>\"e2\"</D:getetag><C:address-data>BEGIN:VCARD\r\nVERSION:3.0\r\nUID:alan\r\nFN:Alan Turing\r\nEMAIL:alan@example.test\r\nEND:VCARD\r\n</C:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\
</D:multistatus>";

    /// An HTTP server answering every request with `status` and `body`.
    async fn serve(status: &'static str, content_type: &'static str, body: &'static str) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        addr
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn carddav_books_are_served_live_not_stored() {
        let Ok(host) = std::env::var("TEST_DB_HOST") else { return };
        let cfg = |db: &str| inboxora_core::config::DbConfig {
            host: host.clone(),
            port: std::env::var("TEST_DB_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432),
            database: db.into(),
            user: std::env::var("TEST_DB_USER").unwrap_or_else(|_| "mailflow".into()),
            password: std::env::var("TEST_DB_PASSWORD").ok(),
            pool_max: 4,
            statement_timeout_ms: 30_000,
        };
        let admin = Db::connect(&cfg("postgres")).unwrap();
        admin.batch("DROP DATABASE IF EXISTS live_card_test WITH (FORCE)").await.unwrap();
        admin.batch("CREATE DATABASE live_card_test").await.unwrap();
        let db = Db::connect(&cfg("live_card_test")).unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
        inboxora_core::migrations::run(&db, &dir).await.unwrap();
        let _ = inboxora_core::crypto::init_key(&"ab".repeat(32));
        db.batch("INSERT INTO system_settings (key, value) VALUES ('allow_private_hosts', 'true') ON CONFLICT (key) DO UPDATE SET value = 'true'")
            .await
            .unwrap();
        let user: Uuid = db
            .opt(
                "INSERT INTO users (username, password_hash) VALUES ('cards@example.test', 'x') RETURNING id",
                params![],
            )
            .await
            .unwrap()
            .unwrap()
            .get("id");
        let base = format!("http://{}", serve("207 Multi-Status", "application/xml", CARDS).await);
        let href = format!("{base}/books/a/");
        let hash = inboxora_mail::util::href_hash(&href);
        let books = json!([{ "href": href, "hrefHash": hash, "displayName": "Remote", "selected": true }]);
        let source: Uuid = db
            .opt(
                "INSERT INTO calendar_import_sources (user_id, kind, url, url_fingerprint, username, password, display_name, interval_seconds, address_book_collections) \
                 VALUES ($1, 'caldav', $2, $3, 'u', $4, 'Server', 3600, $5) RETURNING id",
                params![
                    user,
                    crypto::encrypt(&base).unwrap(),
                    crypto::sha256_hex(&base),
                    crypto::encrypt("pw").unwrap(),
                    crypto::encrypt(&books.to_string()).unwrap()
                ],
            )
            .await
            .unwrap()
            .unwrap()
            .get("id");
        let user = user.to_string();
        let r = dav_sync::sync_carddav_side(&db, &user, &source.to_string()).await.unwrap();
        assert_eq!(r["ok"], json!(true), "{r}");
        let stored = db.opt("SELECT COUNT(*)::bigint AS n FROM contacts", params![]).await.unwrap().unwrap();
        assert_eq!(stored.get::<_, i64>("n"), 0, "nothing is stored");
        let (relation, live) = contacts_relation(&db, &user, 2).await;
        let found = db
            .rows(
                &format!("SELECT display_name, primary_email, birthday::text AS bday FROM {relation} c WHERE user_id = $1 ORDER BY display_name"),
                &[Val::from(&user), live],
            )
            .await
            .unwrap();
        let names: Vec<String> = found.iter().map(|r| r.get::<_, String>("display_name")).collect();
        assert_eq!(names, ["Ada Lovelace", "Alan Turing"]);
        assert_eq!(found[0].get::<_, Option<String>>("primary_email").as_deref(), Some("ada@example.test"));
        assert_eq!(found[0].get::<_, Option<String>>("bday").as_deref(), Some("1815-12-10"));
        let ada = contact_rows(&db, &user, None).await.into_iter().find(|m| m["uid"] == "ada").unwrap();
        assert!(is_live_contact(&db, &user, ada["id"].as_str().unwrap()).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ical_feeds_are_served_live_not_stored() {
        let Ok(host) = std::env::var("TEST_DB_HOST") else { return };
        let cfg = |db: &str| inboxora_core::config::DbConfig {
            host: host.clone(),
            port: std::env::var("TEST_DB_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432),
            database: db.into(),
            user: std::env::var("TEST_DB_USER").unwrap_or_else(|_| "mailflow".into()),
            password: std::env::var("TEST_DB_PASSWORD").ok(),
            pool_max: 4,
            statement_timeout_ms: 30_000,
        };
        let admin = Db::connect(&cfg("postgres")).unwrap();
        admin.batch("DROP DATABASE IF EXISTS live_dav_test WITH (FORCE)").await.unwrap();
        admin.batch("CREATE DATABASE live_dav_test").await.unwrap();
        let db = Db::connect(&cfg("live_dav_test")).unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
        inboxora_core::migrations::run(&db, &dir).await.unwrap();
        let _ = inboxora_core::crypto::init_key(&"ab".repeat(32));
        if let Ok(url) = std::env::var("TEST_REDIS_URL") {
            init(inboxora_core::redis::connect(&url, 2).await.unwrap());
        }
        db.batch("INSERT INTO system_settings (key, value) VALUES ('allow_private_hosts', 'true') ON CONFLICT (key) DO UPDATE SET value = 'true'")
            .await
            .unwrap();
        let user: Uuid = db
            .opt(
                "INSERT INTO users (username, password_hash) VALUES ('live@example.test', 'x') RETURNING id",
                params![],
            )
            .await
            .unwrap()
            .unwrap()
            .get("id");
        let url = format!("http://{}/feed.ics", serve_feed().await);
        let source: Uuid = db
            .opt(
                "INSERT INTO calendar_import_sources (user_id, kind, url, url_fingerprint, display_name, interval_seconds) \
                 VALUES ($1, 'ical_url', $2, $3, 'Feed', 3600) RETURNING id",
                params![user, crypto::encrypt(&url).unwrap(), crypto::sha256_hex(&url)],
            )
            .await
            .unwrap()
            .unwrap()
            .get("id");
        let user = user.to_string();
        let r = dav_sync::sync_calendar_source(&db, &user, &source.to_string()).await.unwrap();
        assert_eq!(r["ok"], json!(true), "{r}");
        let stored = db.opt("SELECT COUNT(*)::bigint AS n FROM calendar_events", params![]).await.unwrap().unwrap();
        assert_eq!(stored.get::<_, i64>("n"), 0, "nothing is stored");
        let rows = event_rows(&db, &user, None).await;
        let mut summaries: Vec<&str> = rows.iter().filter_map(|r| r.get("summary").and_then(Value::as_str)).collect();
        summaries.sort_unstable();
        assert_eq!(summaries, ["Review", "Standup"]);
        let review = rows.iter().find(|r| r["summary"] == "Review").unwrap();
        assert_eq!(review["recurring"], json!(true));
        let id = review["id"].as_str().unwrap().to_owned();
        assert_eq!(
            event_rows(&db, &user, None).await.iter().find(|r| r["summary"] == "Review").unwrap()["id"],
            json!(id)
        );
        // A cold process (empty memo) fetches from the remote again.
        ROWS.lock().clear();
        assert!(event_by_id(&db, &user, &id).await.is_some(), "stable ids across fetches");
        let projected = crate::services::calendar::project_resources(
            rows,
            chrono::DateTime::parse_from_rfc3339("2026-10-20T00:00:00Z").unwrap().into(),
            chrono::DateTime::parse_from_rfc3339("2026-11-20T00:00:00Z").unwrap().into(),
            Some(&user),
            false,
        )
        .await;
        assert_eq!(projected.events.len(), 4, "one single event and three weekly occurrences");
    }
}
