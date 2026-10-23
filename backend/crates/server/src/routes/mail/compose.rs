//! Outgoing mail: `POST /send` with durable idempotency, Sent-copy handling and recipient
//! learning, and the draft save/delete routes (ports of `routes/send.ts` and `routes/draft.ts`).

use std::sync::LazyLock;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use base64::Engine as _;
use fred::prelude::*;
use fred::types::Value as RValue;
use inboxora_core::pg::Q;
use inboxora_core::{Result, crypto, params};
use inboxora_mail::sanitize::{sanitize_compose_body, sanitize_signature, strip_tags};
use inboxora_mail::text::{collapse_ws, js_len, js_substring};
use inboxora_mail::util::embed_inline_data_images;
use regex::Regex;
use serde_json::{Map, Value, json};

use super::{account_for_user, attachments_of, is_uuid};
use crate::engine::EngineRef;
use crate::engine::connect::Account;
use crate::engine::ops::{RecordFields, SentLookup};
use crate::http::{Auth, Body, Qs};
use crate::routes::util::{err, json as respond, ok};
use crate::services::mail_compose::{Compose, Part, account_transport, split_addresses};
use crate::services::mail_utils::resolve_sent_folder;
use crate::state::AppState;

const MAX_ATTACHMENT_BYTES: u64 = 26_214_400;
const LEASE_SECONDS: i64 = 300;
const INFLIGHT_PREFIX: &str = "__inflight__:";

// ── shared helpers ──────────────────────────────────────────────────────────────────────────

/// `sanitizeHeaderValue`: single-line header text.
fn header_value(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => {
            s.chars().filter(|c| !matches!(c, '\r' | '\n' | '\0')).collect::<String>().trim().to_owned()
        }
        _ => String::new(),
    }
}

/// `parseAddress`: `{ name, email }` from `Name <email>`, `<email>` or a bare address.
fn parse_address(s: &str) -> Value {
    static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+?)\s*<([^>]+)>\s*$").expect("regex"));
    static BARE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*<([^>]+)>\s*$").expect("regex"));
    if let Some(c) = NAMED.captures(s) {
        let name = c[1].trim();
        let name = name.strip_prefix('"').unwrap_or(name);
        let name = name.strip_suffix('"').unwrap_or(name);
        return json!({ "name": name.trim(), "email": c[2].trim().to_lowercase() });
    }
    if let Some(c) = BARE.captures(s) {
        return json!({ "name": "", "email": c[1].trim().to_lowercase() });
    }
    json!({ "name": "", "email": s.trim().to_lowercase() })
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn paragraphs(text: &str) -> String {
    text.split('\n')
        .map(|l| {
            let e = escape(l);
            format!("<p style=\"margin:0\">{}</p>", if e.is_empty() { "&nbsp;" } else { &e })
        })
        .collect()
}

/// `String(v ?? '')`.
fn js_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(v) => inboxora_mail::text::js_string(v),
    }
}

/// A non-empty string field (`x || null`).
fn nonempty(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// `<hex@domain>` Message-ID from the sending address.
fn message_id(from_email: &str) -> String {
    let domain = from_email.split('@').nth(1).filter(|d| !d.is_empty()).unwrap_or("mailflow.local");
    format!("<{}@{domain}>", crypto::random_hex(16))
}

/// `Name <email>` as a parseable mailbox.
fn mailbox(name: Option<&str>, email: &str) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!("\"{}\" <{email}>", n.replace('\\', "\\\\").replace('"', "\\\"")),
        None => email.to_owned(),
    }
}

/// `Buffer.from(s, 'base64')`: lenient, ignoring characters outside the alphabet.
fn decode_base64(s: &str) -> Vec<u8> {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .take_while(|c| *c != '=')
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/')
        .collect();
    let usable = cleaned.len() - cleaned.len() % 4 + if cleaned.len() % 4 > 1 { cleaned.len() % 4 } else { 0 };
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned[..usable.min(cleaned.len())]).unwrap_or_default()
}

pub struct Sender {
    pub alias_id: Option<String>,
    pub name: Option<String>,
    pub email: String,
    pub reply_to: Option<String>,
    pub signature: Option<String>,
}

/// `resolveSenderIdentity`: the account itself, or an explicitly selected alias.
pub async fn resolve_sender(
    s: &AppState,
    account: &Account,
    alias: Option<&str>,
) -> std::result::Result<Sender, (u16, String)> {
    let str_ = |k: &str| account.str(k).filter(|v| !v.is_empty()).map(str::to_owned);
    let fallback = Sender {
        alias_id: None,
        name: str_("sender_name").or_else(|| str_("name")),
        email: account.str("email_address").unwrap_or("").to_owned(),
        reply_to: None,
        signature: account.str("signature").map(str::to_owned),
    };
    let Some(alias) = alias.filter(|a| !a.is_empty()) else { return Ok(fallback) };
    if !is_uuid(alias) {
        return Err((400, format!("invalid input syntax for type uuid: \"{alias}\"")));
    }
    let row =
        s.db.json_opt(
            "SELECT name, email, reply_to, signature FROM account_aliases WHERE id = $1 AND account_id = $2",
            params![alias, account.id()],
        )
        .await
        .map_err(|e| (400, e.to_string()))?
        .ok_or((409, "Selected sender alias is unavailable".to_owned()))?;
    let get = |k: &str| row.get(k).and_then(Value::as_str).map(str::to_owned);
    Ok(Sender {
        alias_id: Some(alias.to_owned()),
        name: get("name"),
        email: get("email").unwrap_or_default(),
        reply_to: get("reply_to").filter(|r| !r.is_empty()),
        signature: if row.get("signature").is_none_or(Value::is_null) { fallback.signature } else { get("signature") },
    })
}

// ── send ────────────────────────────────────────────────────────────────────────────────────

/// `sanitizeSmtpError`: a user-facing message without server internals.
fn smtp_error_message(msg: &str) -> &'static str {
    static RULES: LazyLock<[(Regex, &str); 5]> = LazyLock::new(|| {
        let r = |p: &str| Regex::new(p).expect("regex");
        [
            (
                r(
                    r"(?i)ECONNREFUSED|ENOTFOUND|ETIMEDOUT|ECONNRESET|EHOSTUNREACH|connection refused|timed out|failed to lookup|no route to host|connection reset",
                ),
                "Could not connect to the mail server. Check your SMTP settings.",
            ),
            (
                r(
                    r"(?i)535|534|530|invalid.?login|authentication.?fail|bad.*credentials|username.*password|password.*username",
                ),
                "Authentication failed. Check your email account credentials.",
            ),
            (
                r(r"(?i)throttl|rate.?limit|too many|4\.2\.|4\.7\.94"),
                "The mail server is rate limiting sends. Please try again shortly.",
            ),
            (r(r"(?i)550|5\.[13]\.|reject|blacklist|spam|not.?accept"), "Message was rejected by the mail server."),
            (
                r(r"(?i)TLS|SSL|certificate|handshake"),
                "Secure connection to the mail server failed. Check your TLS settings.",
            ),
        ]
    });
    RULES.iter().find(|(re, _)| re.is_match(msg)).map_or("Failed to send message. Please try again.", |(_, m)| m)
}

/// A structured 4xx/5xx SMTP reply: the server refused, so DATA was not accepted.
fn explicit_rejection(msg: &str) -> bool {
    static CODE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(permanent|transient) error \(([45]\d\d)\)").expect("regex"));
    CODE.is_match(msg)
}

/// `normalizeRecipients`: header-injection-safe recipient strings.
fn normalize_recipients(v: Option<&Value>, field: &str) -> std::result::Result<Vec<String>, String> {
    let list = match v {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(Value::Array(a)) => a,
        Some(_) => return Err(format!("{field} must be an array")),
    };
    list.iter()
        .enumerate()
        .map(|(i, a)| {
            let Some(s) = a.as_str().filter(|s| !s.trim().is_empty()) else {
                return Err(format!("{field}[{i}] is empty or not a string"));
            };
            let t = s.trim();
            if t.contains(['\r', '\n', '\0']) {
                return Err(format!("{field}[{i}] contains invalid characters"));
            }
            match t.rfind('@') {
                Some(at) if at >= 1 && at != t.len() - 1 => Ok(t.to_owned()),
                _ => Err(format!("{field}[{i}] is not a valid email address")),
            }
        })
        .collect()
}

fn send_text_to_html(text: &str) -> String {
    format!("<div style=\"font-family:sans-serif;font-size:14px;line-height:1.6\">{}</div>", paragraphs(text))
}

fn body_to_plain(body: &str, html: bool) -> String {
    if html { strip_tags(body) } else { body.to_owned() }
}

fn snippet(text: &str) -> String {
    js_substring(collapse_ws(text).trim(), 200)
}

fn priority_headers(p: &str) -> Vec<(String, String)> {
    let (x, ms) = match p {
        "high" => ("1 (Highest)", "High"),
        "low" => ("5 (Lowest)", "Low"),
        _ => return vec![],
    };
    vec![("X-Priority".into(), x.into()), ("X-Msmail-Priority".into(), ms.into()), ("Importance".into(), ms.into())]
}

enum Claim {
    Claimed,
    Inflight,
    Uncertain,
    Completed(Value),
    Mismatch,
}

async fn claim_intent(s: &AppState, user: &str, key: &str, fingerprint: &str, token: &str) -> Result<Claim> {
    if s.db
        .opt(
            "INSERT INTO send_idempotency (user_id, idempotency_key, request_fingerprint, status, intent_token) VALUES ($1, $2, $3, 'pending', $4::uuid) \
             ON CONFLICT (user_id, idempotency_key) DO NOTHING RETURNING status",
            params![user, key, fingerprint, token],
        )
        .await?
        .is_some()
    {
        return Ok(Claim::Claimed);
    }
    let Some(row) = s
        .db
        .json_opt("SELECT status, request_fingerprint, result FROM send_idempotency WHERE user_id = $1 AND idempotency_key = $2", params![user, key])
        .await?
    else {
        return Ok(Claim::Inflight);
    };
    let st = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("");
    Ok(if st("request_fingerprint") != fingerprint {
        Claim::Mismatch
    } else if st("status") == "completed" {
        Claim::Completed(row.get("result").cloned().unwrap_or(Value::Null))
    } else if st("status") == "uncertain" {
        Claim::Uncertain
    } else {
        Claim::Inflight
    })
}

#[derive(Clone)]
struct Intent {
    db: inboxora_core::pg::Db,
    user: String,
    key: String,
    token: String,
}

impl Intent {
    async fn uncertain(&self) {
        let _ = self
            .db
            .exec(
                "UPDATE send_idempotency SET status = 'uncertain', updated_at = NOW() WHERE user_id = $1 AND idempotency_key = $2 AND intent_token = $3::uuid AND status = 'pending'",
                params![&self.user, &self.key, &self.token],
            )
            .await;
    }
    async fn complete(&self, result: &Value) {
        let r = self
            .db
            .exec(
                "UPDATE send_idempotency SET status = 'completed', result = $4::jsonb, updated_at = NOW() WHERE user_id = $1 AND idempotency_key = $2 \
                 AND intent_token = $3::uuid AND status IN ('pending', 'uncertain')",
                params![&self.user, &self.key, &self.token, result.to_string()],
            )
            .await;
        if r.is_err() {
            self.uncertain().await;
        }
    }
    async fn release(&self) {
        let r = self
            .db
            .exec(
                "DELETE FROM send_idempotency WHERE user_id = $1 AND idempotency_key = $2 AND intent_token = $3::uuid",
                params![&self.user, &self.key, &self.token],
            )
            .await;
        if r.is_err() {
            self.uncertain().await;
        }
    }
}

const RENEW: &str =
    "if redis.call('GET', KEYS[1]) == ARGV[1] then return redis.call('EXPIRE', KEYS[1], ARGV[2]) end return 0";
const RELEASE: &str = "if redis.call('GET', KEYS[1]) == ARGV[1] then return redis.call('DEL', KEYS[1]) end return 0";
const COMPLETE: &str = "if redis.call('GET', KEYS[1]) == ARGV[1] then return redis.call('SET', KEYS[1], ARGV[2], 'EX', ARGV[3]) end return 0";

async fn lease_script(
    redis: &Pool,
    script: &str,
    key: &str,
    args: Vec<String>,
) -> std::result::Result<bool, fred::error::Error> {
    let v: RValue = redis.eval(script, vec![key.to_owned()], args).await?;
    Ok(match v {
        RValue::Integer(1) => true,
        RValue::String(s) => &*s == "OK",
        _ => false,
    })
}

/// Background recipient learning (`send_count`, `is_auto = false`).
async fn learn_recipients(db: inboxora_core::pg::Db, user: String, recipients: Vec<String>) {
    let run = async {
        let book: uuid::Uuid = db
            .opt(
                "INSERT INTO address_books (user_id, name) VALUES ($1, 'Personal') ON CONFLICT (user_id, name) DO UPDATE SET updated_at = NOW() RETURNING id",
                params![&user],
            )
            .await?
            .ok_or_else(|| inboxora_core::Error::internal("address book upsert returned no row"))?
            .get("id");
        let now = chrono::Utc::now();
        let mut books: Vec<uuid::Uuid> = Vec::new();
        for addr in &recipients {
            let parsed = parse_address(addr);
            let email = parsed["email"].as_str().unwrap_or("").to_lowercase();
            if email.is_empty() {
                continue;
            }
            let name = parsed["name"].as_str().filter(|n| !n.is_empty()).map_or_else(|| email.clone(), str::to_owned);
            let uid = uuid::Uuid::new_v4().to_string();
            let emails = json!([{ "value": email, "type": "other", "primary": true }]);
            let vcard = inboxora_pim::vcard::generate(&json!({ "uid": uid, "displayName": name, "emails": emails }));
            let etag = crypto::md5_hex(&vcard);
            match db
                .opt(
                    "INSERT INTO contacts (address_book_id, user_id, uid, vcard, etag, display_name, primary_email, emails, is_auto, send_count, last_sent) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, false, 1, $9) \
                     ON CONFLICT (address_book_id, primary_email) WHERE primary_email IS NOT NULL DO UPDATE SET \
                       send_count = contacts.send_count + 1, last_sent = $9, is_auto = false, \
                       display_name = CASE WHEN contacts.is_auto THEN $6 ELSE contacts.display_name END, \
                       vcard = COALESCE(contacts.vcard, EXCLUDED.vcard), etag = COALESCE(contacts.etag, EXCLUDED.etag), updated_at = NOW() \
                     RETURNING address_book_id",
                    params![book, &user, uid, vcard, etag, name, email, emails.to_string(), now],
                )
                .await
            {
                Ok(Some(r)) => {
                    let b: uuid::Uuid = r.get("address_book_id");
                    if !books.contains(&b) {
                        books.push(b);
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("Contact upsert errors: {e}"),
            }
        }
        if books.is_empty() {
            books.push(book);
        }
        for b in books {
            db.exec(
                "UPDATE address_books SET sync_token = gen_random_uuid()::text, updated_at = NOW() WHERE id = $1",
                params![b],
            )
            .await?;
        }
        Ok::<_, inboxora_core::Error>(())
    };
    if let Err(e) = run.await {
        tracing::warn!("Contact upsert setup error: {e}");
    }
}

/// `ensureServerAutoSavedSentCopy`: wait for the provider's Sent copy; APPEND once only when
/// every observation confirmed absence.
async fn ensure_auto_saved_sent(
    engine: EngineRef,
    account: Account,
    folder: String,
    mid: String,
    raw: Vec<u8>,
    meta: RecordFields,
) -> bool {
    let mut verification_failed = false;
    for delay in [3, 10, 20] {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        match engine.find_sent_message_by_message_id(&account, &folder, &mid).await {
            Ok(SentLookup::Found(uid)) => {
                let _ = engine.upsert_sent_record(&account, &folder, i64::from(uid), &meta).await;
                return true;
            }
            Ok(SentLookup::Ambiguous) => verification_failed = true,
            Ok(SentLookup::Missing) => {}
            Err(e) => {
                verification_failed = true;
                tracing::warn!("Post-send Sent-copy verification failed: {e}");
            }
        }
    }
    if verification_failed {
        return false;
    }
    match engine.find_sent_message_by_message_id(&account, &folder, &mid).await {
        Ok(SentLookup::Found(uid)) => {
            let _ = engine.upsert_sent_record(&account, &folder, i64::from(uid), &meta).await;
            return true;
        }
        Ok(SentLookup::Missing) => {}
        Ok(SentLookup::Ambiguous) => return false,
        Err(e) => {
            tracing::warn!("Post-send final Sent-copy verification failed: {e}");
            return false;
        }
    }
    match engine.append_to_folder(&account, &folder, &raw, &["\\Seen"]).await {
        Ok((uid, _)) => {
            if let Some(u) = uid.filter(|u| *u > 0) {
                let _ = engine.upsert_sent_record(&account, &folder, u, &meta).await;
            }
            true
        }
        Err(e) => {
            tracing::error!(
                "Post-send Sent-copy fallback APPEND failed for {}/{folder}: {e}",
                inboxora_mail::util::redact_email(account.str("email_address").unwrap_or(""))
            );
            false
        }
    }
}

fn after_delay(engine: &EngineRef, account: &Account, folder: &str, secs: u64) {
    let (e, a, f) = (engine.clone(), account.clone(), folder.to_owned());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        e.sync_folder_on_demand(&a, &f).await;
    });
}

struct Forwarded {
    filename: String,
    content: Vec<u8>,
    content_type: String,
}

/// Resolve forwarded attachments (`forwardedAttachments`) over IMAP, bounded at 4 at a time.
async fn fetch_forwarded(
    s: &AppState,
    user: &str,
    refs: &[(String, String)],
    uploaded: u64,
) -> std::result::Result<Vec<Forwarded>, (u16, String)> {
    let internal = |e: inboxora_core::Error| (500, e.to_string());
    let mut ids: Vec<String> = Vec::new();
    for (m, _) in refs {
        if !ids.contains(m) {
            ids.push(m.clone());
        }
    }
    let mut rows = s
        .db
        .json_rows(
            "SELECT m.id, m.uid, m.folder, m.account_id FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.id = ANY($1::uuid[]) AND a.user_id = $2",
            params![ids, user],
        )
        .await
        .map_err(internal)?;
    for row in rows.iter_mut() {
        let Some(m) = row.as_object_mut() else { continue };
        let Some(loc) = crate::engine::cache::Location::from_row(m) else { continue };
        let body = s.mail.message_body(&loc).await.map_err(|e| (502, e.0))?;
        m.insert("attachments".into(), body.attachments);
    }
    let by_id = |id: &str| rows.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(id));
    let mut declared = 0u64;
    let mut plan: Vec<(Value, Value)> = Vec::new();
    for (mid, part) in refs {
        let msg = by_id(mid).ok_or((404, "Forwarded message not found".to_owned()))?;
        let atts = attachments_of(msg.get("attachments"));
        let att = atts
            .as_array()
            .and_then(|a| a.iter().find(|x| x.get("part").and_then(Value::as_str) == Some(part.as_str())))
            .cloned()
            .ok_or((404, "Attachment not found in message".to_owned()))?;
        declared += match att.get("size") {
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0).max(0.0) as u64,
            Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0).max(0.0) as u64,
            _ => 0,
        };
        plan.push((msg.clone(), att));
    }
    if uploaded + declared > MAX_ATTACHMENT_BYTES {
        return Err((400, "Total attachment size exceeds 25 MB".into()));
    }
    let mut accounts: Vec<Account> = Vec::new();
    for (msg, _) in &plan {
        let id = msg.get("account_id").and_then(Value::as_str).unwrap_or("");
        if !accounts.iter().any(|a| a.id() == id)
            && let Some(a) =
                s.db.json_opt("SELECT * FROM email_accounts WHERE id = $1", params![id]).await.map_err(internal)?
        {
            accounts.push(Account::from_map(a));
        }
    }
    let mut out = Vec::new();
    for batch in plan.chunks(4) {
        let fetched = futures_util::future::join_all(batch.iter().map(|(msg, att)| {
            let account =
                accounts.iter().find(|a| Some(a.id()) == msg.get("account_id").and_then(Value::as_str)).cloned();
            let engine = s.mail.clone();
            async move {
                let account = account.ok_or((404, "Account not found".to_owned()))?;
                let uid = inboxora_core::json::int(msg.get("uid")).unwrap_or(0);
                let folder = msg.get("folder").and_then(Value::as_str).unwrap_or("");
                let part = att.get("part").and_then(Value::as_str).unwrap_or("");
                let filename =
                    att.get("filename").and_then(Value::as_str).filter(|f| !f.is_empty()).unwrap_or("attachment");
                let content = engine
                    .fetch_attachment(&account, uid, folder, part)
                    .await
                    .map_err(|e| (500, e.to_string()))?
                    .ok_or_else(|| (502, format!("Could not fetch attachment: {filename}")))?;
                Ok::<_, (u16, String)>(Forwarded {
                    filename: header_value(Some(&json!(filename))),
                    content,
                    content_type: att
                        .get("type")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                        .unwrap_or("application/octet-stream")
                        .to_owned(),
                })
            }
        }))
        .await;
        for f in fetched {
            out.push(f?);
        }
    }
    let total: u64 = out.iter().map(|f| f.content.len() as u64).sum();
    if uploaded + total > MAX_ATTACHMENT_BYTES {
        return Err((400, "Total attachment size exceeds 25 MB".into()));
    }
    Ok(out)
}

/// `Math.ceil(content.length * 0.75)` summed over string payloads.
fn uploaded_bytes(attachments: Option<&Value>) -> u64 {
    attachments.and_then(Value::as_array).map_or(0, |a| {
        a.iter()
            .filter_map(|x| x.get("content").and_then(Value::as_str))
            .map(|c| (js_len(c) as f64 * 0.75).ceil() as u64)
            .sum()
    })
}

pub async fn send(State(s): State<AppState>, a: Auth, headers: HeaderMap, b: Body) -> Result<Response> {
    let idem_key: Option<String> =
        headers.get("x-idempotency-key").and_then(|v| v.to_str().ok()).map(|k| k.chars().take(128).collect());
    if b.get("sendAt").is_some_and(|v| !v.is_null()) {
        return crate::services::scheduled_send::schedule(&s, &a.user_id, b.0).await;
    }
    send_as(&s, a.user_id, idem_key, b).await
}

/// Send `b` (a `POST /send` body) as `user`; also used by the scheduled-send worker, which
/// passes a stable idempotency key so a crash mid-send can never dispatch the mail twice.
pub async fn send_as(s: &AppState, user: String, idem_key: Option<String>, b: Body) -> Result<Response> {
    let priority = b.str("priority").unwrap_or("normal");
    let Some(account_id) =
        b.get("accountId").filter(|v| inboxora_core::json::truthy(Some(v))).map(inboxora_mail::text::js_string)
    else {
        return Ok(err(400, "accountId required"));
    };
    if b.get("bodyIsHtml").is_some_and(|v| !v.is_boolean()) {
        return Ok(err(400, "bodyIsHtml must be a boolean"));
    }
    if b.get("editedSignatureIsHtml").is_some_and(|v| !v.is_boolean()) {
        return Ok(err(400, "editedSignatureIsHtml must be a boolean"));
    }
    let redis_key = idem_key.as_ref().map(|k| format!("send_idem:{user}:{k}"));

    if let Some(att) = b.get("attachments") {
        let Some(list) = att.as_array() else { return Ok(err(400, "attachments must be an array")) };
        if list.len() > 100 {
            return Ok(err(400, "Too many attachments (max 100)"));
        }
        if uploaded_bytes(Some(att)) > MAX_ATTACHMENT_BYTES {
            return Ok(err(400, "Total attachment size exceeds 25 MB"));
        }
        for (i, x) in list.iter().enumerate() {
            if x.get("filename").and_then(Value::as_str).is_none_or(|f| f.trim().is_empty()) {
                return Ok(err(400, &format!("attachments[{i}].filename is required")));
            }
            if !x.get("content").is_some_and(Value::is_string) {
                return Ok(err(400, &format!("attachments[{i}].content must be a base64 string")));
            }
        }
    }
    let mut forwarded_refs: Vec<(String, String)> = Vec::new();
    if let Some(fwd) = b.get("forwardedAttachments") {
        let Some(list) = fwd.as_array() else { return Ok(err(400, "forwardedAttachments must be an array")) };
        if list.len() > 100 {
            return Ok(err(400, "Too many forwarded attachments (max 100)"));
        }
        for (i, x) in list.iter().enumerate() {
            let Some(mid) = x.get("messageId").and_then(Value::as_str).filter(|m| is_uuid(m)) else {
                return Ok(err(400, &format!("forwardedAttachments[{i}].messageId is invalid")));
            };
            let Some(part) = x.get("part").and_then(Value::as_str).filter(|p| !p.trim().is_empty()) else {
                return Ok(err(400, &format!("forwardedAttachments[{i}].part is required")));
            };
            forwarded_refs.push((mid.to_owned(), part.to_owned()));
        }
    }
    let recipients = (|| {
        Ok::<_, String>((
            normalize_recipients(b.get("to"), "to")?,
            normalize_recipients(b.get("cc"), "cc")?,
            normalize_recipients(b.get("bcc"), "bcc")?,
        ))
    })();
    let (to, cc, bcc) = match recipients {
        Ok(r) => r,
        Err(m) => return Ok(err(400, &m)),
    };
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Ok(err(400, "At least one recipient is required"));
    }
    let subject = header_value(b.get("subject"));

    let Some(account) = account_for_user(s, &account_id, &user).await? else {
        return Ok(err(404, "Account not found"));
    };
    let prefs = s.db.opt("SELECT preferences FROM users WHERE id = $1", params![&user]).await?;
    let plaintext = prefs
        .and_then(|r| r.get::<_, Option<Value>>("preferences"))
        .is_some_and(|p| p.get("plaintextEmail") == Some(&Value::Bool(true)));
    let body_is_html = b.get("bodyIsHtml").and_then(Value::as_bool);
    let input_html = body_is_html == Some(true);
    let output_html = body_is_html.unwrap_or(!plaintext);
    let alias = nonempty(b.get("aliasId"));
    let sender = match resolve_sender(s, &account, alias.as_deref()).await {
        Ok(v) => v,
        Err((st, m)) => return Ok(err(st, &m)),
    };

    let edited = b.get("editedSignature");
    let sig_is_html = b.get("editedSignatureIsHtml") != Some(&Value::Bool(false));
    let signature: Option<String> = match edited {
        Some(v) => nonempty(Some(v)).map(|e| if sig_is_html { sanitize_signature(&e) } else { send_text_to_html(&e) }),
        None => sender.signature.clone().filter(|s| !s.is_empty()),
    };
    let signature_text: Option<String> = if edited.is_some() && !sig_is_html {
        nonempty(edited)
    } else {
        signature.as_deref().map(|sig| strip_tags(sig).trim().to_owned())
    };

    let forwarded = if forwarded_refs.is_empty() {
        vec![]
    } else {
        match fetch_forwarded(s, &user, &forwarded_refs, uploaded_bytes(b.get("attachments"))).await {
            Ok(f) => f,
            Err((st, m)) => {
                return Ok(err(st, if m.is_empty() { "Failed to fetch forwarded attachments" } else { &m }));
            }
        }
    };

    let fingerprint = crypto::sha256_hex(
        json!({
            "accountId": account_id, "aliasId": alias, "to": to, "cc": cc, "bcc": bcc, "subject": subject,
            "body": b.get("body"), "inputBodyIsHtml": input_html, "outputBodyIsHtml": output_html,
            "quotedBody": b.get("quotedBody"), "quotedBodyHtml": b.get("quotedBodyHtml"),
            "inReplyTo": b.get("inReplyTo"), "references": b.get("references"),
            "attachments": b.get("attachments"), "forwardedAttachments": b.get("forwardedAttachments"),
            "editedSignature": edited, "editedSignatureIsHtml": edited.map(|_| sig_is_html), "priority": priority,
        })
        .to_string(),
    );
    if let Some(rk) = &redis_key {
        let cached: Option<String> = match s.redis.get(rk).await {
            Ok(v) => v,
            Err(_) => return Ok(err(503, "Sending is temporarily unavailable. Please try again shortly.")),
        };
        if let Some(c) = &cached {
            if c.starts_with(INFLIGHT_PREFIX) {
                return Ok(err(409, "This message is already being sent."));
            }
            if let Ok(replay) = serde_json::from_str::<Value>(c)
                && replay.get("version") == Some(&json!(1))
                && let Some(fp) = replay.get("fingerprint").and_then(Value::as_str)
                && let Some(result) = replay.get("result")
            {
                if fp != fingerprint {
                    return Ok(err(409, "This idempotency key belongs to a different message."));
                }
                return Ok(ok(result.clone()));
            }
        }
    }

    let (transport, account_map) = match account_transport(&s.db, (*account.0).clone()).await {
        Ok(v) => v,
        Err((st, m)) => return Ok(err(st, &m)),
    };
    let account = Account::from_map(account_map);

    let body = js_text(b.get("body"));
    let quoted = nonempty(b.get("quotedBody")).unwrap_or_default();
    let plain = body_to_plain(&body, input_html);
    let text = match (&signature, &signature_text) {
        (Some(_), t) => format!("{plain}\n\n-- \n{}{quoted}", t.as_deref().unwrap_or_default()),
        (None, _) => format!("{plain}{quoted}"),
    };
    let mid = message_id(&sender.email);
    let mut compose = Compose {
        from: mailbox(sender.name.as_deref(), &sender.email),
        to: to.clone(),
        cc: cc.clone(),
        bcc: bcc.clone(),
        reply_to: sender.reply_to.as_deref().map(split_addresses).unwrap_or_default(),
        subject: subject.clone(),
        text: Some(text),
        message_id: Some(mid.clone()),
        headers: priority_headers(priority),
        ..Default::default()
    };
    if output_html {
        let html_body = if input_html { sanitize_compose_body(&body) } else { send_text_to_html(&body) };
        let sig = signature
            .as_deref()
            .map(|s| format!("<div style=\"margin-top:16px;color:#555;font-size:13px\">{s}</div>"))
            .unwrap_or_default();
        let quoted_html = nonempty(b.get("quotedBodyHtml"))
            .unwrap_or_else(|| if quoted.is_empty() { String::new() } else { send_text_to_html(&quoted) });
        let (html, inline) = embed_inline_data_images(&format!("{html_body}{sig}{quoted_html}"));
        compose.html = Some(html);
        compose.attachments.extend(inline.into_iter().map(|i| Part {
            filename: Some(i.filename),
            content: i.content,
            content_type: i.content_type,
            cid: Some(i.cid),
        }));
    }
    let in_reply_to = nonempty(b.get("inReplyTo")).map(|_| header_value(b.get("inReplyTo")));
    let references = if nonempty(b.get("references")).is_some() {
        Some(header_value(b.get("references")))
    } else {
        in_reply_to.clone()
    };
    compose.in_reply_to = in_reply_to.clone();
    compose.references = references.clone();
    if let Some(list) = b.get("attachments").and_then(Value::as_array) {
        compose.attachments.extend(list.iter().map(|x| Part {
            filename: Some(header_value(x.get("filename"))),
            content: decode_base64(x.get("content").and_then(Value::as_str).unwrap_or("")),
            content_type: x.get("contentType").and_then(Value::as_str).unwrap_or("application/octet-stream").to_owned(),
            cid: None,
        }));
    }
    compose.attachments.extend(forwarded.into_iter().map(|f| Part {
        filename: Some(f.filename),
        content: f.content,
        content_type: f.content_type,
        cid: None,
    }));
    // Gmail and Microsoft 365 file the sent message themselves; every other server (including
    // OIDC-authenticated IMAP such as Dovecot) needs our APPEND, or the copy lands ~30 s later.
    let auto_saves = matches!(account.str("oauth_provider"), Some("google" | "microsoft")) || {
        let host =
            account.str("imap_host").filter(|h| !h.is_empty()).or_else(|| account.str("smtp_host")).unwrap_or("");
        host.to_lowercase().contains("gmail")
    };
    let built = match compose.build() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("Send failed: {e}");
            return Ok(err(500, smtp_error_message(&e)));
        }
    };

    // The database intent is the authoritative gate immediately before SMTP.
    let mut intent: Option<Intent> = None;
    if let Some(key) = &idem_key {
        let token = uuid::Uuid::new_v4().to_string();
        match claim_intent(s, &user, key, &fingerprint, &token).await {
            Err(_) => return Ok(err(503, "Sending is temporarily unavailable. Please try again shortly.")),
            Ok(Claim::Completed(r)) => return Ok(ok(r)),
            Ok(Claim::Mismatch) => return Ok(err(409, "This idempotency key belongs to a different message.")),
            Ok(Claim::Uncertain) => {
                return Ok(err(
                    409,
                    "The result of this send is still being confirmed. It will not be sent again automatically.",
                ));
            }
            Ok(Claim::Inflight) => return Ok(err(409, "This message is already being sent.")),
            Ok(Claim::Claimed) => {}
        }
        intent = Some(Intent { db: s.db.clone(), user: user.clone(), key: key.clone(), token });
    }
    let mut lease: Option<String> = None;
    let mut renewal: Option<tokio::task::JoinHandle<()>> = None;
    if let Some(rk) = &redis_key {
        let token = format!(
            "{INFLIGHT_PREFIX}{}",
            intent.as_ref().map_or_else(|| uuid::Uuid::new_v4().to_string(), |i| i.token.clone())
        );
        let reserved: std::result::Result<Option<String>, _> =
            s.redis.set(rk, token.as_str(), Some(Expiration::EX(LEASE_SECONDS)), Some(SetOptions::NX), false).await;
        match reserved {
            Ok(Some(_)) => {}
            other => {
                if let Some(i) = &intent {
                    i.release().await;
                }
                return Ok(if other.is_err() {
                    err(503, "Sending is temporarily unavailable. Please try again shortly.")
                } else {
                    err(409, "This message is already being sent.")
                });
            }
        }
        let (redis, key, tok, it) = (s.redis.clone(), rk.clone(), token.clone(), intent.clone());
        renewal = Some(tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(60));
            iv.tick().await;
            loop {
                iv.tick().await;
                if !lease_script(&redis, RENEW, &key, vec![tok.clone(), LEASE_SECONDS.to_string()])
                    .await
                    .unwrap_or(false)
                    && let Some(i) = &it
                {
                    i.uncertain().await;
                }
            }
        }));
        lease = Some(token);
    }
    let stop_renewal = |r: &mut Option<tokio::task::JoinHandle<()>>| {
        if let Some(h) = r.take() {
            h.abort();
        }
    };

    // Persist the uncertain state first: a crash or a lost DATA acknowledgement cannot then
    // turn into an automatic re-dispatch.
    if let Some(i) = &intent {
        i.uncertain().await;
    }
    if let Err(e) = transport.send_raw(&built.envelope_from, &built.recipients, &built.raw).await {
        tracing::error!("Send failed: {}", e.message);
        stop_renewal(&mut renewal);
        let pre_delivery =
            e.connection || explicit_rejection(&e.message) || e.message.to_lowercase().contains("invalid login");
        if pre_delivery {
            if let Some(i) = &intent {
                i.release().await;
            }
            if let (Some(rk), Some(t)) = (&redis_key, &lease) {
                let _ = lease_script(&s.redis, RELEASE, rk, vec![t.clone()]).await;
            }
            return Ok(err(500, smtp_error_message(&e.message)));
        }
        if let Some(i) = intent.clone() {
            tokio::spawn(async move { i.uncertain().await });
        }
        return Ok(err(
            502,
            "The mail server response was interrupted after dispatch began. This message will not be sent again automatically.",
        ));
    }

    let all: Vec<String> = to.iter().chain(&cc).chain(&bcc).cloned().collect();
    tokio::spawn(learn_recipients(s.db.clone(), user.clone(), all));

    let mut result = Map::new();
    result.insert("ok".into(), json!(true));
    match resolve_sent_folder(&s.db, account.id(), account.0.get("folder_mappings")).await {
        Err(e) => {
            tracing::error!("Post-send processing failed: {e}");
            result.insert("sentCopySaved".into(), json!(false));
        }
        Ok(sent) => {
            tracing::info!(
                "Post-send: {} sentFolder={} autoSaves={auto_saves}",
                inboxora_mail::util::redact_email(account.str("email_address").unwrap_or("")),
                sent.as_deref().unwrap_or_default()
            );
            if let Some(folder) = &sent {
                let mapped = |list: &[String]| Value::Array(list.iter().map(|a| parse_address(a)).collect());
                let meta = RecordFields {
                    message_id: built.message_id.clone(),
                    subject: Some(subject.clone()),
                    from_name: sender.name.clone(),
                    from_email: Some(sender.email.clone()),
                    to: mapped(&to),
                    cc: mapped(&cc),
                    snippet: snippet(&plain),
                    date: Some(chrono::Utc::now()),
                    in_reply_to: in_reply_to.clone().filter(|v| !v.is_empty()),
                    references: references.clone().filter(|v| !v.is_empty()),
                    ..Default::default()
                };
                if auto_saves {
                    let (e, acc, f, m, raw) =
                        (s.mail.clone(), account.clone(), folder.clone(), mid.clone(), built.raw.clone());
                    tokio::spawn(async move {
                        if ensure_auto_saved_sent(e.clone(), acc.clone(), f.clone(), m, raw, meta).await {
                            e.sync_folder_on_demand(&acc, &f).await;
                        }
                    });
                } else {
                    // APPEND exactly once (not idempotent); a timed-out append keeps running.
                    let (e, acc, f, raw) = (s.mail.clone(), account.clone(), folder.clone(), built.raw.clone());
                    let append = tokio::spawn(async move { e.append_to_folder(&acc, &f, &raw, &["\\Seen"]).await });
                    match tokio::time::timeout(Duration::from_secs(20), append).await {
                        Ok(Ok(Ok((uid, _)))) => {
                            if let Some(u) = uid.filter(|u| *u > 0)
                                && let Err(e) = s.mail.upsert_sent_record(&account, folder, u, &meta).await
                            {
                                tracing::warn!("Sent metadata upsert failed: {e}");
                            }
                            after_delay(&s.mail, &account, folder, 1);
                        }
                        outcome => {
                            let why = match outcome {
                                Ok(Ok(Err(e))) => e.to_string(),
                                Ok(Err(e)) => e.to_string(),
                                _ => "Sent APPEND timed out".into(),
                            };
                            tracing::error!(
                                "IMAP append to Sent failed for {}/{folder}: {why}",
                                inboxora_mail::util::redact_email(account.str("email_address").unwrap_or(""))
                            );
                            result.insert("sentCopySaved".into(), json!(false));
                            after_delay(&s.mail, &account, folder, 8);
                        }
                    }
                }
                result.insert("sentFolder".into(), json!(folder));
            }
        }
    }
    let result = Value::Object(result);
    stop_renewal(&mut renewal);
    if let Some(i) = &intent {
        i.complete(&result).await;
    }
    if let (Some(rk), Some(t)) = (redis_key, lease) {
        let (redis, payload, it) = (
            s.redis.clone(),
            json!({ "version": 1, "fingerprint": fingerprint, "result": result }).to_string(),
            intent,
        );
        tokio::spawn(async move {
            if !lease_script(&redis, COMPLETE, &rk, vec![t, payload, "86400".into()]).await.unwrap_or(false)
                && let Some(i) = it
            {
                i.uncertain().await;
            }
        });
    }
    Ok(ok(result))
}

// ── drafts ──────────────────────────────────────────────────────────────────────────────────

fn safe_positive_int(v: Option<&Value>) -> Option<i64> {
    let f = v?.as_f64()?;
    (f.fract() == 0.0 && f > 0.0 && f <= 9_007_199_254_740_991.0).then_some(f as i64)
}

struct ExistingDraft {
    account_id: String,
    uid: i64,
    folder: String,
    uid_validity: i64,
}

fn existing_draft(v: Option<&Value>) -> Option<ExistingDraft> {
    let o = v?.as_object()?;
    let account_id = o.get("accountId")?.as_str().filter(|a| !a.is_empty())?.to_owned();
    let uid = safe_positive_int(o.get("uid"))?;
    let folder = o
        .get("folder")?
        .as_str()
        .filter(|f| !f.is_empty() && js_len(f) <= 1024 && !f.contains(['\0', '\r', '\n']))?
        .to_owned();
    let uid_validity = safe_positive_int(o.get("uidValidity"))?;
    Some(ExistingDraft { account_id, uid, folder, uid_validity })
}

async fn drafts_folder(s: &AppState, account: &Account) -> Result<Option<String>> {
    if let Some(m) =
        account.0.get("folder_mappings").and_then(|m| m.get("drafts")).filter(|v| inboxora_core::json::truthy(Some(v)))
    {
        return Ok(Some(inboxora_mail::text::js_string(m)));
    }
    Ok(s.db
        .opt(
            "SELECT path FROM folders WHERE account_id = $1 AND special_use = '\\Drafts' LIMIT 1",
            params![account.id()],
        )
        .await?
        .map(|r| r.get("path")))
}

async fn stored_validity(s: &AppState, account: &str, uid: i64, folder: &str) -> Result<Option<f64>> {
    Ok(s.db
        .json_opt(
            "SELECT draft_uid_validity FROM messages WHERE account_id = $1 AND uid = $2 AND folder = $3",
            params![account, uid, folder],
        )
        .await?
        .and_then(|r| match r.get("draft_uid_validity") {
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(v)) => v.trim().parse().ok(),
            _ => None,
        }))
}

/// Recipients of a draft as `{ name, email }` (only arrays; empty entries dropped).
fn draft_recipients(v: Option<&Value>) -> Value {
    Value::Array(
        v.and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|x| inboxora_core::json::truthy(Some(x)))
                    .map(|x| parse_address(&inboxora_mail::text::js_string(x)))
                    .collect()
            })
            .unwrap_or_default(),
    )
}

fn truthy_strings(v: Option<&Value>, wrap_single: bool) -> Vec<String> {
    let items: Vec<Value> = match v {
        Some(Value::Array(a)) => a.clone(),
        Some(x) if wrap_single => vec![x.clone()],
        _ => vec![],
    };
    items.iter().filter(|x| inboxora_core::json::truthy(Some(x))).map(inboxora_mail::text::js_string).collect()
}

pub async fn save_draft(State(s): State<AppState>, a: Auth, b: Body) -> Result<Response> {
    if b.get("editedSignatureIsHtml").is_some_and(|v| !v.is_boolean()) {
        return Ok(err(400, "editedSignatureIsHtml must be a boolean"));
    }
    let has_edited = b.has("editedSignature");
    let existing = existing_draft(b.get("existingDraft"));
    let Some(account_id) =
        b.get("accountId").filter(|v| inboxora_core::json::truthy(Some(v))).map(inboxora_mail::text::js_string)
    else {
        return Ok(err(400, "accountId required"));
    };
    let Some(account) = account_for_user(&s, &account_id, &a.user_id).await? else {
        return Ok(err(404, "Account not found"));
    };
    let sender = match resolve_sender(&s, &account, nonempty(b.get("aliasId")).as_deref()).await {
        Ok(v) => v,
        Err((st, m)) => {
            tracing::error!("Save draft failed: {m}");
            return Ok(err(st, &m));
        }
    };
    let body_is_html = b.truthy("bodyIsHtml");
    let raw_sig = if has_edited {
        nonempty(b.get("editedSignature"))
    } else {
        sender.signature.clone().filter(|s| !s.is_empty())
    };
    let sig_is_html =
        if has_edited { b.get("editedSignatureIsHtml").and_then(Value::as_bool).unwrap_or(true) } else { true };
    let effective_sig = raw_sig.as_deref().map(|r| if sig_is_html { sanitize_signature(r) } else { paragraphs(r) });
    let sig_text = raw_sig.as_ref().map(|r| {
        if sig_is_html { strip_tags(effective_sig.as_deref().unwrap_or("")).trim().to_owned() } else { r.clone() }
    });
    let body = js_text(b.get("body")).to_owned();
    let body = if b.get("body").is_some_and(|v| !inboxora_core::json::truthy(Some(v))) { String::new() } else { body };
    let body_text = if body_is_html { strip_tags(&body) } else { body.clone() };
    let body_html = if body_is_html { sanitize_compose_body(&body) } else { paragraphs(&body) };
    let quoted = nonempty(b.get("quotedBody"));
    let quoted_html = nonempty(b.get("quotedBodyHtml"));
    let raw_html = format!(
        "{body_html}{}{}",
        effective_sig
            .as_deref()
            .map(|s| format!("<div style=\"margin-top:16px;color:#555;font-size:13px\">{s}</div>"))
            .unwrap_or_default(),
        quoted_html.clone().or_else(|| quoted.as_deref().map(paragraphs)).unwrap_or_default()
    );
    let (draft_html, inline) = embed_inline_data_images(&raw_html);
    let mid = message_id(&sender.email);
    let q = quoted.clone().unwrap_or_default();
    let text_body = match sig_text.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => format!("{body_text}\n\n-- \n{t}{q}"),
        None => format!("{body_text}{q}"),
    };
    let in_reply_to = nonempty(b.get("inReplyTo")).map(|_| header_value(b.get("inReplyTo")));
    let references = nonempty(b.get("references")).map(|_| header_value(b.get("references")));
    let compose = Compose {
        from: mailbox(sender.name.as_deref(), &sender.email),
        reply_to: sender.reply_to.as_deref().map(split_addresses).unwrap_or_default(),
        to: truthy_strings(b.get("to"), true),
        cc: truthy_strings(b.get("cc"), false),
        bcc: truthy_strings(b.get("bcc"), false),
        subject: header_value(b.get("subject")),
        in_reply_to: in_reply_to.clone(),
        references: references.clone(),
        text: Some(text_body.clone()),
        html: Some(draft_html),
        message_id: Some(mid.clone()),
        attachments: inline
            .into_iter()
            .map(|i| Part {
                filename: Some(i.filename),
                content: i.content,
                content_type: i.content_type,
                cid: Some(i.cid),
            })
            .collect(),
        ..Default::default()
    };
    let raw = match compose.build() {
        Ok(b) => b.raw,
        Err(e) => {
            tracing::error!("Save draft failed: {e}");
            return Ok(err(500, &e));
        }
    };
    let Some(folder) = drafts_folder(&s, &account).await? else {
        return Ok(err(422, "No Drafts folder found for this account"));
    };
    let (uid, validity) = match s.mail.append_to_folder(&account, &folder, &raw, &["\\Draft", "\\Seen"]).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Save draft failed: {e}");
            let m = e.to_string();
            return Ok(err(500, if m.is_empty() { "Failed to save draft" } else { &m }));
        }
    };
    if let Some(u) = uid {
        let composition = json!({
            "version": 2, "authoredBody": body, "bodyIsHtml": body_is_html, "signatureHtml": effective_sig,
            "signatureText": sig_text, "quotedBody": quoted, "quotedBodyHtml": quoted_html,
        });
        let fields = RecordFields {
            message_id: mid,
            subject: b.str("subject").map(str::to_owned),
            from_name: sender.name.clone(),
            from_email: Some(sender.email.clone()),
            to: draft_recipients(b.get("to")),
            cc: draft_recipients(b.get("cc")),
            bcc: draft_recipients(b.get("bcc")),
            alias_id: sender.alias_id.clone(),
            in_reply_to,
            references,
            composition: Some(composition),
            snippet: snippet(&text_body),
            body_html: Some(raw_html),
            body_text: Some(text_body),
            uid_validity: validity,
            ..Default::default()
        };
        if let Err(e) = s.mail.upsert_draft_record(&account, &folder, u, &fields).await {
            tracing::error!("Draft: failed to persist local row uid={u}: {e}");
        }
        // Delete the prior draft only once the new one has a UID, in its own account.
        if let Some(old) = existing {
            let run = async {
                let Some(prev) = account_for_user(&s, &old.account_id, &a.user_id).await.map_err(|e| e.to_string())?
                else {
                    tracing::warn!("Draft: previous draft account is unavailable; retaining uid={}", old.uid);
                    return Ok(());
                };
                let prev_folder = drafts_folder(&s, &prev).await.map_err(|e| e.to_string())?;
                let stored = stored_validity(&s, prev.id(), old.uid, &old.folder).await.map_err(|e| e.to_string())?;
                if prev_folder.as_deref() != Some(old.folder.as_str()) || stored != Some(old.uid_validity as f64) {
                    tracing::warn!("Draft: previous draft identity cannot be confirmed; retaining uid={}", old.uid);
                    return Ok(());
                }
                s.mail
                    .permanent_delete_message(&prev, old.uid, &old.folder, Some(old.uid_validity))
                    .await
                    .map_err(|e| e.to_string())?;
                s.db.exec(
                    "DELETE FROM messages WHERE account_id = $1 AND uid = $2 AND folder = $3 AND draft_uid_validity = $4",
                    params![prev.id(), old.uid, &old.folder, old.uid_validity],
                )
                .await
                .map_err(|e| e.to_string())?;
                Ok::<_, String>(())
            };
            if let Err(e) = run.await {
                tracing::error!("Draft: failed to delete old uid={}: {e}", old.uid);
            }
        }
    }
    Ok(ok(json!({ "uid": uid, "folder": folder, "uidValidity": validity })))
}

/// JS `parseInt(s, 10)`: leading whitespace, optional sign, then digits.
fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n: i64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// JS `Number(s)` for a query value, as a safe positive integer.
fn query_positive_int(s: Option<&str>) -> Option<i64> {
    let t = s?.trim();
    let f: f64 = if t.is_empty() { 0.0 } else { t.parse().ok()? };
    (f.fract() == 0.0 && f > 0.0 && f <= 9_007_199_254_740_991.0).then_some(f as i64)
}

pub async fn delete_draft(State(s): State<AppState>, a: Auth, Path(uid): Path<String>, q: Qs) -> Result<Response> {
    let Some(uid) = parse_int(&uid).filter(|u| *u != 0) else {
        return Ok(err(400, "Invalid uid"));
    };
    let account_id = q.str("accountId").filter(|v| !v.is_empty());
    let folder = q.str("folder").filter(|v| !v.is_empty());
    let validity = query_positive_int(q.str("uidValidity"));
    let (Some(account_id), Some(folder), Some(validity)) = (account_id, folder, validity) else {
        return Ok(err(400, "accountId, folder and uidValidity required"));
    };
    let Some(account) = account_for_user(&s, account_id, &a.user_id).await? else {
        return Ok(err(404, "Account not found"));
    };
    if drafts_folder(&s, &account).await?.as_deref() != Some(folder) {
        return Ok(err(409, "Draft folder cannot be confirmed"));
    }
    if stored_validity(&s, account.id(), uid, folder).await? != Some(validity as f64) {
        return Ok(err(409, "Draft identity cannot be confirmed"));
    }
    if let Err(e) = s.mail.permanent_delete_message(&account, uid, folder, Some(validity)).await {
        tracing::error!("Delete draft failed: {e}");
        let m = e.to_string();
        return Ok(err(500, if m.is_empty() { "Failed to delete draft" } else { &m }));
    }
    s.db.exec(
        "DELETE FROM messages WHERE account_id = $1 AND uid = $2 AND folder = $3 AND draft_uid_validity = $4",
        params![account.id(), uid, folder, validity],
    )
    .await?;
    Ok(respond(200, json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_and_recipients() {
        assert_eq!(parse_address("\"Bob B\" <Bob@X.test>"), json!({ "name": "Bob B", "email": "bob@x.test" }));
        assert_eq!(parse_address(" <a@b.c> "), json!({ "name": "", "email": "a@b.c" }));
        assert_eq!(
            normalize_recipients(Some(&json!(["a@b.c", " x@y.z "])), "to"),
            Ok(vec!["a@b.c".into(), "x@y.z".into()])
        );
        assert_eq!(normalize_recipients(Some(&json!(["a@"])), "cc"), Err("cc[0] is not a valid email address".into()));
        assert_eq!(normalize_recipients(Some(&json!("a@b.c")), "to"), Err("to must be an array".into()));
        assert_eq!(
            normalize_recipients(Some(&json!(["a@b\n.c"])), "to"),
            Err("to[0] contains invalid characters".into())
        );
    }

    #[test]
    fn smtp_classification() {
        assert!(explicit_rejection("permanent error (550): mailbox unavailable"));
        assert!(!explicit_rejection("connection reset"));
        assert_eq!(
            smtp_error_message("Invalid login: 535 5.7.8"),
            "Authentication failed. Check your email account credentials."
        );
        assert_eq!(smtp_error_message("weird"), "Failed to send message. Please try again.");
    }

    #[test]
    fn lenient_base64_and_ints() {
        assert_eq!(decode_base64("aGVsbG8"), b"hello");
        assert_eq!(decode_base64("aGVs\nbG8="), b"hello");
        assert_eq!(parse_int("12abc"), Some(12));
        assert_eq!(parse_int("abc"), None);
        assert_eq!(query_positive_int(Some(" 7 ")), Some(7));
        assert_eq!(query_positive_int(Some("1.5")), None);
    }
}
