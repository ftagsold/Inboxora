//! Mail content is never persisted in PostgreSQL: the `messages` table is a header index (ids,
//! folder/UID, date, flags, threading links). Display fields (subject, addresses, snippet) and
//! bodies are fetched live from IMAP and kept in Redis, which runs memory-only.
//!
//! Display fields never expire: a message's content is fixed for its row, so the key lives until
//! the row is deleted (the tombstone janitor in `preload.rs` drops it) and every folder is
//! preloaded. Bodies expire after `MAIL_BODY_TTL_SECONDS` (7 days).
//!
//! Every miss is grouped by account and folder and served with one `UID FETCH` per group;
//! freshly synced messages are written through, so a new-mail list rarely touches IMAP.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fred::prelude::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::connect::{Account, load_account};
use super::parse::{Addr, EnrichContext, Parsed, clean, enrich, parse_message};
use super::policy::BODY_PREFETCH_PARTS;
use super::{EResult, Engine, EngineError};
use inboxora_imap::FetchQuery;

const PREFIX: &str = "ibx:mc:";
/// Version of the cached display fields; bump when `Meta` changes shape (old keys are swept).
pub const META_VERSION: &str = "v1";
/// One IMAP round trip per (account, folder) group must finish within this budget; rows it
/// could not hydrate are served without display fields rather than stalling the list.
const HYDRATE_TIMEOUT: Duration = Duration::from_secs(15);
/// UIDs per header FETCH command. Each message needs its structure, headers and the start of
/// its text parsed; some servers take over a second per message, so a command stays small
/// enough to finish inside the socket timeout (each chunk is cached as it lands).
const FETCH_CHUNK: usize = 100;
/// Bytes of each candidate text part fetched for a snippet (a multiple of 4 keeps base64 whole).
const SNIPPET_BYTES: u32 = 4096;

/// Redis-backed, best-effort: every error is logged and treated as a miss.
pub struct ContentCache {
    redis: Option<Pool>,
    body_ttl: i64,
    /// One header fetch per (account, folder) at a time; a second request for the same group
    /// waits and then reads what the first one cached.
    groups: parking_lot::Mutex<HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>>,
}

impl ContentCache {
    pub fn new(redis: Option<Pool>) -> Self {
        let body_ttl = inboxora_core::config::env("MAIL_BODY_TTL_SECONDS")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(7 * 24 * 3600);
        Self { redis, body_ttl, groups: parking_lot::Mutex::new(HashMap::new()) }
    }

    fn group_lock(&self, account_id: &str, folder: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.groups.lock().entry((account_id.to_owned(), folder.to_owned())).or_default().clone()
    }

    pub fn redis(&self) -> Option<&Pool> {
        self.redis.as_ref()
    }

    /// Display fields are kept until their row is deleted; everything else expires.
    fn expiry(&self, key: &str) -> Option<Expiration> {
        (!key.starts_with("h:")).then_some(Expiration::EX(self.body_ttl))
    }

    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let redis = self.redis.as_ref()?;
        match redis.get::<Option<String>, _>(format!("{PREFIX}{key}")).await {
            Ok(Some(s)) => serde_json::from_str(&s).ok(),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("[mail-cache] get failed: {e}");
                None
            }
        }
    }

    pub async fn get_many<T: DeserializeOwned>(&self, keys: &[String]) -> Vec<Option<T>> {
        let Some(redis) = self.redis.as_ref().filter(|_| !keys.is_empty()) else {
            return keys.iter().map(|_| None).collect();
        };
        let full: Vec<String> = keys.iter().map(|k| format!("{PREFIX}{k}")).collect();
        match redis.mget::<Vec<Option<String>>, _>(full).await {
            Ok(v) => v.into_iter().map(|s| s.and_then(|s| serde_json::from_str(&s).ok())).collect(),
            Err(e) => {
                tracing::warn!("[mail-cache] mget failed: {e}");
                keys.iter().map(|_| None).collect()
            }
        }
    }

    pub async fn put<T: Serialize>(&self, key: &str, value: &T) {
        let Some(redis) = &self.redis else { return };
        let Ok(s) = serde_json::to_string(value) else { return };
        if let Err(e) = redis.set::<(), _, _>(format!("{PREFIX}{key}"), s, self.expiry(key), None, false).await {
            tracing::warn!("[mail-cache] set failed: {e}");
        }
    }

    pub async fn put_many<T: Serialize>(&self, entries: &[(String, T)]) {
        let Some(redis) = self.redis.as_ref().filter(|_| !entries.is_empty()) else { return };
        let pipe = redis.next().pipeline();
        for (k, v) in entries {
            if let Ok(s) = serde_json::to_string(v) {
                let _: Result<(), _> = pipe.set(format!("{PREFIX}{k}"), s, self.expiry(k), None, false).await;
            }
        }
        if let Err(e) = pipe.all::<Vec<fred::types::Value>>().await {
            tracing::warn!("[mail-cache] pipelined set failed: {e}");
        }
    }

    pub async fn forget(&self, keys: Vec<String>) {
        let Some(redis) = self.redis.as_ref().filter(|_| !keys.is_empty()) else { return };
        let full: Vec<String> = keys.into_iter().map(|k| format!("{PREFIX}{k}")).collect();
        if let Err(e) = redis.del::<i64, _>(full).await {
            tracing::warn!("[mail-cache] del failed: {e}");
        }
    }
}

pub fn meta_key(id: &Uuid) -> String {
    format!("h:{META_VERSION}:{id}")
}

/// The full Redis key of a cache entry (for SCAN-based maintenance).
pub fn full_key(key: &str) -> String {
    format!("{PREFIX}{key}")
}

pub fn prefix() -> &'static str {
    PREFIX
}

pub fn body_key(id: &Uuid) -> String {
    format!("b:{id}")
}

/// The display fields of a message — what used to be columns of `messages`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub message_id: Option<String>,
    pub subject: String,
    pub from_name: String,
    pub from_email: String,
    pub sender_name: Option<String>,
    pub sender_email: Option<String>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub reply_to: Vec<Addr>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub snippet: String,
    pub has_attachments: bool,
    pub list_unsubscribe: Option<String>,
    pub list_unsubscribe_post: Option<String>,
    pub delivery_addresses: Vec<String>,
}

impl Meta {
    pub fn from_parsed(p: &Parsed) -> Self {
        let header = |k: &str| {
            p.headers.get(k).map(|v| clean(&inboxora_mail::headers::decode_mime_words(v))).filter(|s| !s.is_empty())
        };
        Meta {
            message_id: p.message_id.as_deref().map(clean).filter(|s| !s.is_empty()),
            subject: clean(&p.subject),
            from_name: clean(&p.from_name),
            from_email: clean(&p.from_email),
            sender_name: p.sender_name.as_deref().map(clean),
            sender_email: p.sender_email.as_deref().map(clean),
            to: p.to.clone(),
            cc: p.cc.clone(),
            reply_to: p.reply_to.clone(),
            in_reply_to: p.in_reply_to.as_deref().map(clean).filter(|s| !s.is_empty()),
            references: p.references.as_deref().map(clean).filter(|s| !s.is_empty()),
            snippet: clean(&p.snippet),
            has_attachments: p.has_attachments,
            list_unsubscribe: header("list-unsubscribe"),
            list_unsubscribe_post: header("list-unsubscribe-post"),
            delivery_addresses: p.delivery_addresses.clone(),
        }
    }

    /// Write the fields into a row under the column names the API has always returned.
    pub fn apply(&self, row: &mut Map<String, Value>) {
        row.insert("subject".into(), json!(self.subject));
        row.insert("from_name".into(), json!(self.from_name));
        row.insert("from_email".into(), json!(self.from_email));
        row.insert("sender_name".into(), json!(self.sender_name));
        row.insert("sender_email".into(), json!(self.sender_email));
        row.insert("to_addresses".into(), json!(self.to));
        row.insert("cc_addresses".into(), json!(self.cc));
        row.insert("reply_to".into(), json!(self.reply_to));
        row.insert("in_reply_to".into(), json!(self.in_reply_to));
        row.insert("thread_references".into(), json!(self.references));
        row.insert("snippet".into(), json!(self.snippet));
        row.insert("list_unsubscribe".into(), json!(self.list_unsubscribe));
        row.insert("list_unsubscribe_post".into(), json!(self.list_unsubscribe_post));
        row.insert("delivery_addresses".into(), json!(self.delivery_addresses));
        if !row.contains_key("has_attachments") {
            row.insert("has_attachments".into(), json!(self.has_attachments));
        }
    }
}

/// A rendered body as the read endpoints return it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CachedBody {
    /// Sanitised HTML with `<head>` removed and links normalised.
    pub html: Option<String>,
    pub text: Option<String>,
    pub attachments: Value,
}

/// Where a message row lives on the IMAP server.
#[derive(Debug, Clone)]
pub struct Location {
    pub id: Uuid,
    pub account_id: String,
    pub folder: String,
    pub uid: i64,
}

impl Location {
    pub fn from_row(row: &Map<String, Value>) -> Option<Self> {
        Some(Location {
            id: row.get("id")?.as_str()?.parse().ok()?,
            account_id: row.get("account_id")?.as_str()?.to_owned(),
            folder: row.get("folder")?.as_str()?.to_owned(),
            uid: inboxora_core::json::int(row.get("uid"))?,
        })
    }
}

/// Sanitise and normalise HTML once, before it is cached.
pub fn prepare_html(raw: &str) -> Option<String> {
    use inboxora_mail::sanitize::{rewrite_anchor_hrefs, rewrite_ebay_imageser_urls, sanitize_email, strip_email_head};
    let mut h = strip_email_head(&sanitize_email(raw));
    if h.contains("svcs.ebay.com/imageser") {
        h = rewrite_ebay_imageser_urls(&h);
    }
    h = rewrite_anchor_hrefs(&h);
    let h = clean(&h);
    (!h.is_empty()).then_some(h)
}

pub fn meta_fetch_query() -> FetchQuery {
    FetchQuery {
        uid: true,
        envelope: true,
        body_structure: true,
        headers: true,
        body_parts: BODY_PREFETCH_PARTS.iter().map(|s| (*s).to_owned()).collect(),
        // A snippet needs the start of the text part; whole parts can be attachments.
        body_part_max: Some(SNIPPET_BYTES),
        ..Default::default()
    }
}

impl Engine {
    /// Display fields for message rows, keyed by row id. Misses are fetched from IMAP; rows the
    /// server no longer has (or that could not be fetched in time) are absent from the map.
    pub async fn message_meta(self: &Arc<Self>, rows: &[Location]) -> HashMap<Uuid, Meta> {
        let keys: Vec<String> = rows.iter().map(|r| meta_key(&r.id)).collect();
        let cached: Vec<Option<Meta>> = self.cache.get_many(&keys).await;
        let mut out = HashMap::with_capacity(rows.len());
        let mut missing: HashMap<(String, String), Vec<&Location>> = HashMap::new();
        for (row, hit) in rows.iter().zip(cached) {
            match hit {
                Some(m) => {
                    out.insert(row.id, m);
                }
                None => missing.entry((row.account_id.clone(), row.folder.clone())).or_default().push(row),
            }
        }
        if missing.is_empty() {
            return out;
        }
        let fetches = missing.into_iter().map(|((account_id, folder), locs)| {
            let engine = self.clone();
            async move {
                let run = engine.fetch_meta_group(&account_id, &folder, &locs, super::limits::Prio::Ui);
                match tokio::time::timeout(HYDRATE_TIMEOUT, run).await {
                    Ok(Ok(found)) => found,
                    Ok(Err(e)) => {
                        tracing::warn!("[mail-cache] header fetch failed for {account_id}/{folder}: {e}");
                        Vec::new()
                    }
                    Err(_) => {
                        tracing::warn!("[mail-cache] header fetch timed out for {account_id}/{folder}");
                        Vec::new()
                    }
                }
            }
        });
        let fetched: Vec<(Uuid, Meta)> = futures_util::future::join_all(fetches).await.into_iter().flatten().collect();
        out.extend(fetched);
        out
    }

    /// `message_meta` that answers within `budget`: misses still being fetched are left out of
    /// the map (the fetch carries on in the background and caches its result).
    pub async fn message_meta_within(self: &Arc<Self>, rows: &[Location], budget: Duration) -> HashMap<Uuid, Meta> {
        let keys: Vec<String> = rows.iter().map(|r| meta_key(&r.id)).collect();
        let cached: Vec<Option<Meta>> = self.cache.get_many(&keys).await;
        let mut out = HashMap::with_capacity(rows.len());
        let mut missing: Vec<Location> = Vec::new();
        for (row, hit) in rows.iter().zip(cached) {
            match hit {
                Some(m) => {
                    out.insert(row.id, m);
                }
                None => missing.push(row.clone()),
            }
        }
        if missing.is_empty() {
            return out;
        }
        let engine = self.clone();
        let task = tokio::spawn(async move { engine.message_meta(&missing).await });
        if let Ok(Ok(found)) = tokio::time::timeout(budget, task).await {
            out.extend(found);
        }
        out
    }

    /// Fill list rows from the cache within `budget`; rows still missing their display fields
    /// carry `meta_pending: true` (the client fetches them with `POST /mail/meta`).
    pub async fn hydrate_list_values(self: &Arc<Self>, rows: &mut [Value], budget: Duration) {
        let locs: Vec<Location> = rows.iter().filter_map(|r| r.as_object().and_then(Location::from_row)).collect();
        let meta = self.message_meta_within(&locs, budget).await;
        for r in rows.iter_mut() {
            let Some(row) = r.as_object_mut() else { continue };
            let id = row.get("id").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
            match id.and_then(|id| meta.get(&id)) {
                Some(m) => {
                    m.apply(row);
                    row.insert("meta_pending".into(), json!(false));
                }
                None => {
                    Meta::default().apply(row);
                    row.insert("meta_pending".into(), json!(true));
                }
            }
        }
    }

    pub async fn fetch_meta_group(
        self: &Arc<Self>,
        account_id: &str,
        folder: &str,
        locs: &[&Location],
        prio: super::limits::Prio,
    ) -> EResult<Vec<(Uuid, Meta)>> {
        let group = self.cache.group_lock(account_id, folder);
        let _flight = group.lock().await;
        // Another request may have fetched part of this group while we waited.
        let keys: Vec<String> = locs.iter().map(|l| meta_key(&l.id)).collect();
        let cached: Vec<Option<Meta>> = self.cache.get_many(&keys).await;
        let mut out = Vec::with_capacity(locs.len());
        let mut todo: Vec<&Location> = Vec::new();
        for (l, hit) in locs.iter().zip(cached) {
            match hit {
                Some(m) => out.push((l.id, m)),
                None => todo.push(l),
            }
        }
        if todo.is_empty() {
            return Ok(out);
        }
        let account =
            load_account(&self.db, account_id).await?.ok_or_else(|| EngineError("Account not found".into()))?;
        let by_uid: HashMap<u32, Uuid> =
            todo.iter().filter_map(|l| u32::try_from(l.uid).ok().map(|u| (u, l.id))).collect();
        let mut uids: Vec<u32> = by_uid.keys().copied().collect();
        uids.sort_unstable();
        let query = meta_fetch_query();
        let mut lease = self.lease_with(&account, prio).await?;
        let r: EResult<Vec<(Uuid, Meta)>> = async {
            let client = lease.client();
            client.select(folder).await?;
            let mut fetched = Vec::with_capacity(uids.len());
            for chunk in uids.chunks(FETCH_CHUNK) {
                let mut batch = Vec::with_capacity(chunk.len());
                for msg in client.fetch(&inboxora_imap::uid_set(chunk), &query, true, None).await? {
                    let Some(id) = msg.uid.and_then(|u| by_uid.get(&u)) else { continue };
                    batch.push((*id, meta_of(&account, folder, &msg)));
                }
                // Cache each chunk as it lands, so a waiting request (or a later page) sees it.
                let entries: Vec<(String, Meta)> = batch.iter().map(|(id, m)| (meta_key(id), m.clone())).collect();
                self.cache.put_many(&entries).await;
                fetched.extend(batch);
            }
            Ok(fetched)
        }
        .await;
        match r {
            Ok(fetched) => {
                out.extend(fetched);
                Ok(out)
            }
            Err(e) => {
                self.note_conn_failure(&account, lease.client(), &e.0).await;
                lease.evict();
                Err(e)
            }
        }
    }

    /// `(from_email, from_name)` per row: cached headers when present, else one envelope-only
    /// FETCH per folder (cheap enough for aggregate views over many messages).
    pub async fn message_senders(self: &Arc<Self>, rows: &[Location]) -> HashMap<Uuid, (String, String)> {
        let keys: Vec<String> = rows.iter().map(|r| meta_key(&r.id)).collect();
        let cached: Vec<Option<Meta>> = self.cache.get_many(&keys).await;
        let mut out = HashMap::with_capacity(rows.len());
        let mut missing: HashMap<(String, String), Vec<&Location>> = HashMap::new();
        for (row, hit) in rows.iter().zip(cached) {
            match hit {
                Some(m) => {
                    out.insert(row.id, (m.from_email, m.from_name));
                }
                None => missing.entry((row.account_id.clone(), row.folder.clone())).or_default().push(row),
            }
        }
        for ((account_id, folder), locs) in missing {
            let run = async {
                let account = load_account(&self.db, &account_id)
                    .await?
                    .ok_or_else(|| EngineError("Account not found".into()))?;
                let by_uid: HashMap<u32, Uuid> =
                    locs.iter().filter_map(|l| u32::try_from(l.uid).ok().map(|u| (u, l.id))).collect();
                let mut uids: Vec<u32> = by_uid.keys().copied().collect();
                uids.sort_unstable();
                let query = FetchQuery { uid: true, envelope: true, ..Default::default() };
                let mut lease = self.lease(&account).await?;
                let r: EResult<Vec<(Uuid, (String, String))>> = async {
                    let client = lease.client();
                    client.select(&folder).await?;
                    let mut found = Vec::new();
                    for chunk in uids.chunks(FETCH_CHUNK * 5) {
                        for msg in client.fetch(&inboxora_imap::uid_set(chunk), &query, true, None).await? {
                            let Some(id) = msg.uid.and_then(|u| by_uid.get(&u)) else { continue };
                            let p = parse_message(&msg);
                            found.push((*id, (clean(&p.from_email), clean(&p.from_name))));
                        }
                    }
                    Ok(found)
                }
                .await;
                if r.is_err() {
                    lease.evict();
                }
                r
            };
            match tokio::time::timeout(HYDRATE_TIMEOUT * 2, run).await {
                Ok(Ok(found)) => out.extend(found),
                Ok(Err(e)) => tracing::warn!("[mail-cache] sender fetch failed for {account_id}/{folder}: {e}"),
                Err(_) => tracing::warn!("[mail-cache] sender fetch timed out for {account_id}/{folder}"),
            }
        }
        out
    }

    /// Fill display fields into JSON rows that carry `id`, `account_id`, `folder` and `uid`.
    pub async fn hydrate_rows(self: &Arc<Self>, rows: &mut [Map<String, Value>]) {
        let locs: Vec<Location> = rows.iter().filter_map(Location::from_row).collect();
        let meta = self.message_meta(&locs).await;
        for row in rows.iter_mut() {
            let id = row.get("id").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
            match id.and_then(|id| meta.get(&id)) {
                Some(m) => m.apply(row),
                None => Meta::default().apply(row),
            }
        }
    }

    /// `hydrate_rows` for `Vec<Value>` result sets.
    pub async fn hydrate_values(self: &Arc<Self>, rows: &mut [Value]) {
        let mut maps: Vec<Map<String, Value>> =
            rows.iter_mut().map(|v| std::mem::take(v).as_object().cloned().unwrap_or_default()).collect();
        self.hydrate_rows(&mut maps).await;
        for (slot, m) in rows.iter_mut().zip(maps) {
            *slot = Value::Object(m);
        }
    }

    /// Cache the display fields of a message that was just fetched (sync, append). Messages
    /// fetched without body parts have no snippet yet, so they are left to the next miss.
    pub async fn remember_meta(&self, id: Uuid, parsed: &Parsed, has_snippet_parts: bool) {
        if has_snippet_parts || !parsed.snippet.is_empty() {
            self.cache.put(&meta_key(&id), &Meta::from_parsed(parsed)).await;
        }
    }

    /// The rendered body of a message: Redis, else IMAP (then cached).
    pub async fn message_body(self: &Arc<Self>, loc: &Location) -> EResult<CachedBody> {
        let key = body_key(&loc.id);
        if let Some(b) = self.cache.get::<CachedBody>(&key).await {
            return Ok(b);
        }
        let account =
            load_account(&self.db, &loc.account_id).await?.ok_or_else(|| EngineError("Account not found".into()))?;
        let b = self.fetch_message_body(&account, loc.uid, &loc.folder).await?;
        let body = CachedBody {
            html: b.html.as_deref().and_then(prepare_html),
            text: b.text.as_deref().map(clean).filter(|t| !t.is_empty()),
            attachments: Value::Array(b.attachments.iter().map(super::mime::AttachmentRef::to_json).collect()),
        };
        if body.html.is_some() || body.text.is_some() || !b.attachments.is_empty() {
            self.cache.put(&key, &body).await;
        }
        Ok(body)
    }

    /// Plain text of a message for rules, AI and series detection (body text, else HTML text).
    pub async fn message_text(self: &Arc<Self>, loc: &Location) -> EResult<String> {
        let b = self.message_body(loc).await?;
        Ok(match (b.text, b.html) {
            (Some(t), _) => t,
            (None, Some(h)) => inboxora_mail::util::html_to_plain_text(&h),
            (None, None) => String::new(),
        })
    }
}

fn meta_of(account: &Account, folder: &str, msg: &inboxora_imap::Message) -> Meta {
    let mut parsed = parse_message(msg);
    enrich(
        &mut parsed,
        &EnrichContext {
            account_email: account.str("email_address"),
            account_name: account.str("name"),
            sender_name: account.str("sender_name"),
            folder_path: folder,
            sent_folder_path: account.mapping("sent"),
        },
    );
    Meta::from_parsed(&parsed)
}
