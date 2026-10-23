//! Conversation Engine v2 persistence: one logical message per RFC message, grouped into
//! conversations by provider thread, RFC parent edges, automated series and user overrides
//! (port of `conversationPersistence.ts`, `conversationOverridePolicy.ts`,
//! `automatedSeries.ts`, `conversationIngestEnvelope.ts` and the provider metadata helpers).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use inboxora_core::pg::{Db, Q, Row, Tx};
use inboxora_core::{Error, Result, params};
use inboxora_mail::conversation::{
    MessageDate, canonical_subject, classify_direction, fingerprint, logical_message_identity,
};
use inboxora_mail::headers::Headers;
use inboxora_mail::util::normalize_message_id_list;
use serde_json::{Map, Value, json};
use uuid::Uuid;

fn sha256_hex(s: &str) -> String {
    fingerprint(s)
}

/// `JSON.stringify` of a value (serde_json matches it for the strings/arrays used here).
fn js_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

// ── provider metadata ───────────────────────────────────────────────────────────────────────

pub fn classify_provider_host(host: &str) -> &'static str {
    let h = host.to_lowercase();
    if h.contains("gmail") || h.contains("googlemail") {
        "gmail"
    } else if ["outlook", "office365", "microsoft", "exchange", "hotmail", "live.com"].iter().any(|p| h.contains(p)) {
        "outlook"
    } else {
        "generic"
    }
}

pub fn provider_namespace(provider: &str, account_id: Option<&str>, host: Option<&str>) -> String {
    format!(
        "{}:{}:{}",
        if provider.is_empty() { "generic" } else { provider },
        account_id.filter(|s| !s.is_empty()).unwrap_or("unknown-account"),
        host.filter(|s| !s.is_empty()).unwrap_or("unknown-host")
    )
}

fn scalar(v: Option<&str>) -> Option<String> {
    v.filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("NIL")).map(str::to_owned)
}

fn outlook_root(thread_index: Option<&str>) -> Option<String> {
    let compact: String = thread_index?.chars().filter(|c| !c.is_whitespace()).collect();
    let raw = inboxora_mail::headers::base64_lenient(&compact);
    if raw.len() < 22 || !(raw.len() - 22).is_multiple_of(5) {
        return None;
    }
    Some(raw[..22].iter().map(|b| format!("{b:02x}")).collect())
}

/// A provider identity for one copy (`ConversationProviderRef`).
#[derive(Debug, Clone, Default)]
pub struct Provider {
    pub provider: Option<String>,
    pub provider_message_id: Option<String>,
    pub provider_thread_id: Option<String>,
    pub namespace: Option<String>,
    pub source: Option<String>,
    pub is_strong: bool,
    pub diagnostics: Value,
}

/// Inputs to `providerMetadataForMessage` from a live fetch merged over the stored row.
pub struct ProviderInput<'a> {
    pub email_id: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub stored_message_id: Option<&'a str>,
    pub stored_thread_id: Option<&'a str>,
    pub headers: Option<&'a Headers>,
    pub account_id: &'a str,
    pub host: &'a str,
}

/// `providerMetadataForMessage`.
pub fn provider_metadata(input: &ProviderInput<'_>) -> Provider {
    let provider = classify_provider_host(input.host);
    let msg_id = scalar(input.email_id).or_else(|| scalar(input.stored_message_id));
    let thread_id = scalar(input.thread_id).or_else(|| scalar(input.stored_thread_id));
    let base_source = if thread_id.is_some() {
        Some("provider-thread-id")
    } else if msg_id.is_some() {
        Some("provider-email-id")
    } else {
        None
    };
    let header = |n: &str| input.headers.and_then(|h| h.get(n)).map(str::to_owned);
    let thread_index = header("thread-index");
    let root = if thread_id.is_none() { outlook_root(thread_index.as_deref()) } else { None };
    let effective_thread = if thread_id.is_none() && provider == "outlook" { root.clone() } else { thread_id.clone() };
    let source = if thread_id.is_some() {
        base_source.map(str::to_owned)
    } else if root.is_some() {
        Some("outlook-conversation-index-root".into())
    } else {
        base_source.map(str::to_owned)
    };
    Provider {
        provider: Some(provider.to_owned()),
        diagnostics: json!({
            "fingerprint": sha256_hex(&[provider, msg_id.as_deref().unwrap_or(""), thread_id.as_deref().unwrap_or("")].join("|")),
            "messageIdSource": if input.email_id.is_some() { json!("imapflow-email-id") } else { Value::Null },
            "threadIdSource": if input.thread_id.is_some() { json!("imapflow-thread-id") } else { Value::Null },
        }),
        provider_message_id: msg_id,
        is_strong: provider == "gmail" && thread_id.is_some(),
        provider_thread_id: effective_thread,
        namespace: Some(provider_namespace(provider, Some(input.account_id), Some(input.host))),
        source,
    }
}

/// `providerIdentityForCopy`: the provider identity reconstructed from a stored copy.
pub fn provider_identity_for_copy(copy: &Map<String, Value>, host: &str) -> Provider {
    let s = |k: &str| copy.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
    let namespace = s("provider_namespace");
    let persisted = namespace.as_deref().and_then(|n| n.split(':').next()).filter(|p| !p.is_empty()).map(str::to_owned);
    let h = host.to_lowercase();
    let provider = persisted.or_else(|| {
        if h.contains("gmail") || h.contains("googlemail") {
            Some("gmail".into())
        } else if ["outlook", "office365", "exchange", "hotmail", "live.com"].iter().any(|p| h.contains(p)) {
            Some("outlook".into())
        } else {
            None
        }
    });
    let thread = s("provider_thread_id");
    let gmail = provider.as_deref() == Some("gmail") && thread.is_some();
    let root = if provider.as_deref() == Some("outlook") {
        outlook_root(s("conversation_thread_index").as_deref()).or_else(|| s("provider_thread_id"))
    } else {
        None
    };
    Provider {
        provider,
        provider_message_id: s("provider_message_id"),
        provider_thread_id: if gmail { thread } else { root.clone() },
        namespace,
        is_strong: gmail,
        source: if gmail {
            Some("persisted-provider-thread".into())
        } else {
            root.map(|_| "outlook-conversation-index-root".to_owned())
        },
        diagnostics: json!({ "reconstructed": true }),
    }
}

// ── own identities ──────────────────────────────────────────────────────────────────────────

fn normalize_address(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(i) = s.find('<')
        && let Some(j) = s[i + 1..].find('>')
        && j > 0
    {
        return Some(s[i + 1..i + 1 + j].to_lowercase().trim().to_owned());
    }
    let parts: Vec<&str> = s.split('@').collect();
    if parts.len() == 2
        && !parts[0].is_empty()
        && !s.chars().any(char::is_whitespace)
        && parts[1].split('.').count() >= 2
        && parts[1].split('.').all(|p| !p.is_empty())
    {
        return Some(s.to_lowercase());
    }
    None
}

/// `resolveOwnIdentityAddresses`: account address, aliases, and the copy's delivery targets.
pub async fn resolve_own_identities(
    db: &impl Q,
    account_id: &str,
    delivery_addresses: Option<&Value>,
    headers: Option<&Headers>,
) -> Result<Vec<String>> {
    let row = db
        .opt(
            "SELECT a.email_address, COALESCE(json_agg(DISTINCT jsonb_build_object('email', aa.email)) \
             FILTER (WHERE aa.email IS NOT NULL), '[]'::json) AS aliases \
             FROM email_accounts a LEFT JOIN account_aliases aa ON aa.account_id = a.id \
             WHERE a.id = $1 GROUP BY a.user_id, a.email_address",
            params![account_id],
        )
        .await?;
    let mut ids: Vec<String> = Vec::new();
    if let Some(r) = row {
        if let Some(e) = r.get::<_, Option<String>>("email_address").filter(|e| !e.is_empty()) {
            ids.push(e);
        }
        let aliases: Value = r.get::<_, Option<Value>>("aliases").unwrap_or(Value::Null);
        for a in aliases.as_array().into_iter().flatten() {
            if let Some(e) = a.get("email").and_then(Value::as_str).filter(|e| !e.is_empty()) {
                ids.push(e.to_owned());
            }
        }
    }
    let delivery: Vec<Value> = match delivery_addresses {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => serde_json::from_str::<Vec<Value>>(s).unwrap_or_default(),
        _ => vec![],
    };
    for item in delivery {
        let email = match &item {
            Value::String(s) => normalize_address(s),
            Value::Object(o) => {
                o.get("email").or_else(|| o.get("address")).and_then(Value::as_str).and_then(normalize_address)
            }
            _ => None,
        };
        ids.extend(email);
    }
    if let Some(h) = headers {
        for key in ["delivered-to", "x-original-to", "envelope-to"] {
            if let Some(v) = h.get(key) {
                ids.extend(v.split(',').filter_map(normalize_address));
            }
        }
    }
    let mut seen = HashSet::new();
    Ok(ids
        .into_iter()
        .map(|v| v.to_lowercase().trim().to_owned())
        .filter(|v| !v.is_empty() && seen.insert(v.clone()))
        .collect())
}

// ── overrides ───────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct OverrideRow {
    pub id: Uuid,
    pub override_type: String,
    pub target_id: Option<Uuid>,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default)]
pub struct EffectiveOverride {
    pub locked: Option<bool>,
    pub force_include: Option<OverrideRow>,
    pub force_exclude: Option<OverrideRow>,
    pub split: Option<OverrideRow>,
    pub mv: Option<OverrideRow>,
    pub merge: Option<OverrideRow>,
}

fn override_row(r: &Row) -> OverrideRow {
    OverrideRow {
        id: r.get("id"),
        override_type: r.get("override_type"),
        target_id: r.get("target_id"),
        created_at: r.get("created_at"),
    }
}

/// `effectiveConversationOverride`.
pub async fn effective_override(
    tx: &impl Q,
    user_id: &str,
    account_id: &str,
    conversation_id: Option<Uuid>,
    logical_message_id: Option<Uuid>,
) -> Result<EffectiveOverride> {
    let conv_rows = tx
        .rows(
            "SELECT id, override_type, target_id, reason, logical_message_id, created_at FROM conversation_overrides \
             WHERE user_id = $1 AND account_id = $3 AND conversation_id = $2 AND logical_message_id IS NULL \
             ORDER BY created_at DESC, id DESC",
            params![user_id, conversation_id, account_id],
        )
        .await?;
    let conv: Vec<OverrideRow> = conv_rows.iter().map(override_row).collect();
    let mut conv_latest: HashMap<String, OverrideRow> = HashMap::new();
    for r in &conv {
        conv_latest.entry(r.override_type.clone()).or_insert_with(|| r.clone());
    }
    let mut msg_latest: Vec<OverrideRow> = Vec::new();
    if let Some(lm) = logical_message_id {
        let rows = tx
            .rows(
                "SELECT id, override_type, target_id, reason, logical_message_id, created_at FROM conversation_overrides \
                 WHERE user_id = $1 AND account_id = $3 AND logical_message_id = $2 ORDER BY created_at DESC, id DESC",
                params![user_id, lm, account_id],
            )
            .await?;
        let mut seen = HashSet::new();
        for r in rows.iter().map(override_row) {
            if seen.insert(r.override_type.clone()) {
                msg_latest.push(r);
            }
        }
    }
    let lock = conv_latest.contains_key("lock-conversation");
    let unlock = conv_latest.contains_key("unlock-conversation");
    let locked = match (lock, unlock) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        (true, true) => conv
            .iter()
            .find(|r| r.override_type == "lock-conversation" || r.override_type == "unlock-conversation")
            .map(|r| r.override_type == "lock-conversation"),
        _ => None,
    };
    // Latest membership event; created_at compares as Node's String(Date) (second precision)
    // with the id as tie-break.
    let mut membership: Vec<&OverrideRow> = msg_latest
        .iter()
        .filter(|r| r.override_type == "force-include" || r.override_type == "force-exclude")
        .collect();
    let key =
        |r: &OverrideRow| r.created_at.map(|t| inboxora_mail::conversation::js_date_to_string(&t)).unwrap_or_default();
    membership.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| b.id.to_string().cmp(&a.id.to_string())));
    let latest = membership.first().map(|r| (*r).clone());
    let msg = |t: &str| msg_latest.iter().find(|r| r.override_type == t).cloned();
    Ok(EffectiveOverride {
        locked,
        force_include: latest.clone().filter(|r| r.override_type == "force-include"),
        force_exclude: latest.filter(|r| r.override_type == "force-exclude"),
        split: msg("manual-split"),
        mv: msg("manual-move"),
        merge: conv_latest.get("manual-merge").cloned(),
    })
}

/// `resolveConversationAlias`: follow alias links to the canonical conversation.
pub async fn resolve_alias(
    tx: &impl Q,
    user_id: &str,
    account_id: Option<&str>,
    conversation_id: Uuid,
) -> Result<Uuid> {
    let mut current = conversation_id;
    let mut seen = HashSet::new();
    for _ in 0..20 {
        if !seen.insert(current) {
            return Err(Error::internal("Conversation alias cycle detected"));
        }
        let row = tx
            .opt(
                "SELECT canonical_conversation_id FROM conversation_aliases WHERE user_id = $1 AND alias_conversation_id = $2 AND ($3::uuid IS NULL OR account_id = $3)",
                params![user_id, current, account_id],
            )
            .await?;
        match row.map(|r| r.get::<_, Uuid>("canonical_conversation_id")) {
            Some(next) if next != current => current = next,
            _ => return Ok(current),
        }
    }
    Err(Error::internal("Conversation alias chain too deep"))
}

/// `assertNoAliasCycle`.
pub async fn assert_no_alias_cycle(
    tx: &impl Q,
    user_id: &str,
    account_id: &str,
    source: Uuid,
    target: Uuid,
) -> Result<()> {
    let mut current = target;
    let mut seen = HashSet::new();
    for _ in 0..20 {
        if current == source {
            return Err(Error::internal("manual-merge would create an alias cycle"));
        }
        if !seen.insert(current) {
            return Err(Error::internal("Conversation alias cycle detected"));
        }
        let row = tx
            .opt(
                "SELECT canonical_conversation_id FROM conversation_aliases WHERE user_id = $1 AND account_id = $3 AND alias_conversation_id = $2 FOR UPDATE",
                params![user_id, current, account_id],
            )
            .await?;
        match row.map(|r| r.get::<_, Uuid>("canonical_conversation_id")) {
            Some(next) if next != current => current = next,
            _ => return Ok(()),
        }
    }
    Err(Error::internal("Conversation alias chain too deep"))
}

/// `refreshConversationAggregates`.
pub async fn refresh_aggregates(tx: &impl Q, user_id: &str, conversation_id: Uuid) -> Result<()> {
    tx.exec(
        "UPDATE conversations c SET \
           first_message_at = (SELECT MIN(message_date) FROM logical_messages WHERE conversation_id = c.id), \
           last_message_at = (SELECT MAX(message_date) FROM logical_messages WHERE conversation_id = c.id), \
           logical_message_count = (SELECT COUNT(*) FROM logical_messages WHERE conversation_id = c.id), \
           copy_count = (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND is_deleted = false), \
           unread_count = (SELECT COUNT(*) FROM logical_messages lm WHERE lm.conversation_id = c.id AND EXISTS (SELECT 1 FROM messages m WHERE m.logical_message_id = lm.id AND m.conversation_id = c.id AND m.is_deleted = false AND m.is_read = false)), \
           updated_at = NOW() \
         WHERE c.id = $1 AND c.user_id = $2",
        params![conversation_id, user_id],
    )
    .await?;
    Ok(())
}

/// `lockConversationsDeterministically`.
pub async fn lock_conversations(tx: &impl Q, user_id: &str, ids: &[Uuid]) -> Result<Vec<Uuid>> {
    let mut ordered: Vec<Uuid> = ids.to_vec();
    ordered.sort_by_key(|a| a.to_string());
    ordered.dedup();
    if !ordered.is_empty() {
        tx.rows(
            "SELECT id FROM conversations WHERE user_id = $1 AND id = ANY($2::uuid[]) ORDER BY id FOR UPDATE",
            params![user_id, ordered.iter().map(Uuid::to_string).collect::<Vec<_>>()],
        )
        .await?;
    }
    Ok(ordered)
}

const DELETE_ORPHANS: &str = "DELETE FROM conversations c WHERE c.user_id = $1 AND c.id = ANY($2::uuid[]) \
     AND NOT EXISTS (SELECT 1 FROM logical_messages lm WHERE lm.conversation_id = c.id) \
     AND NOT EXISTS (SELECT 1 FROM conversation_aliases ca WHERE ca.alias_conversation_id = c.id OR ca.canonical_conversation_id = c.id) \
     AND NOT EXISTS (SELECT 1 FROM conversation_overrides co WHERE co.conversation_id = c.id)";

fn uuids(v: &[Uuid]) -> Vec<String> {
    v.iter().map(Uuid::to_string).collect()
}

// ── automated series ────────────────────────────────────────────────────────────────────────

const SERIES_WINDOW_MS: i64 = 72 * 60 * 60 * 1000;
const MAX_SEGMENT: f64 = 100.0;
const GENERIC_SUBJECTS: [&str; 12] = [
    "test",
    "hello",
    "hi",
    "question",
    "invoice",
    "faktura",
    "oferta",
    "informacja",
    "notification",
    "powiadomienie",
    "no subject",
    "brak tematu",
];

/// The incoming message as the series decisions read it.
#[derive(Debug, Clone, Default)]
pub struct SeriesInput {
    pub headers: Map<String, Value>,
    pub from_email: Option<String>,
    pub to_addresses: Value,
    pub date: Option<DateTime<Utc>>,
    pub body_text: Option<String>,
    pub canonical_subject: String,
    pub references_anchor: Option<String>,
}

/// A stored logical message as the series decisions compare it: digests only, so no subject,
/// address or body text is kept in the database.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeriesDigest {
    pub subject_hash: Option<String>,
    pub sender_hash: String,
    pub recipients_hash: String,
    pub automated: bool,
    /// Header authentication results are not part of the digest (the stored-row inputs never
    /// carried headers), so a previous message never counts as authenticated.
    pub authenticated: bool,
    pub references_anchor: Option<String>,
    pub body_template: Option<String>,
    pub date: Option<DateTime<Utc>>,
    pub logical_message_count: f64,
}

/// A 64-bit digest: enough to compare series signals, small enough to index.
fn digest64(s: &str) -> String {
    sha256_hex(s)[..16].to_owned()
}

pub fn subject_hash(canonical: &str) -> Option<String> {
    let s = canonical.to_lowercase();
    (!s.is_empty()).then(|| digest64(&s))
}

struct Signals {
    automated: bool,
    authenticated: bool,
    sender: String,
    recipients: String,
}

fn signals(m: &SeriesInput) -> Signals {
    let h = |k: &str| m.headers.get(k).map(inboxora_mail::text::js_string).unwrap_or_default();
    let auto = h("auto-submitted").to_lowercase();
    let prec = h("precedence").to_lowercase();
    let sender = m.from_email.clone().unwrap_or_default().to_lowercase();
    let auth = {
        let a = h("authentication-results");
        if a.is_empty() { h("arc-authentication-results") } else { a }
    }
    .to_lowercase();
    static NOREPLY: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?:^|[+.-])no-?reply@").expect("regex"));
    let mut recips: Vec<String> = m
        .to_addresses
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| match a {
            Value::String(s) => s.clone(),
            other => match other.get("email") {
                Some(Value::String(e)) if !e.is_empty() => e.clone(),
                _ => "[object Object]".into(),
            },
        })
        .collect();
    recips.sort();
    Signals {
        automated: auto == "auto-generated"
            || auto == "auto-replied"
            || prec == "bulk"
            || prec == "list"
            || NOREPLY.is_match(&sender),
        authenticated: auth.contains("dkim=pass") || auth.contains("spf=pass") || auth.contains("dmarc=pass"),
        sender: [sender, h("return-path"), h("list-id")].join("|").to_lowercase(),
        recipients: js_json(&json!(recips)),
    }
}

impl SeriesInput {
    /// The digest stored for this message so later messages can be compared against it.
    pub fn digest(&self) -> SeriesDigest {
        let s = signals(self);
        SeriesDigest {
            subject_hash: subject_hash(&self.canonical_subject),
            sender_hash: digest64(&s.sender),
            recipients_hash: digest64(&s.recipients),
            automated: s.automated,
            authenticated: false,
            references_anchor: self.references_anchor.clone(),
            body_template: self.body_text.as_deref().map(|b| body_template_fingerprint(b)[..16].to_owned()),
            date: self.date,
            logical_message_count: 0.0,
        }
    }
}

/// `bodyTemplateFingerprint`: OTP-like 4–8 digit tokens masked unless they follow an
/// identifier keyword.
pub fn body_template_fingerprint(body: &str) -> String {
    static SPLIT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(\s+|[,;:(){}\[\]<>])").expect("regex"));
    static IDENT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)(?:order|zamów|ticket|case|ref(?:erence)?|tracking|shipment|parcel|invoice|faktura|numer|id)\b",
        )
        .expect("regex")
    });
    static SEP_ONLY: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[\s,;:(){}\[\]<>]+$").expect("regex"));
    let mut tokens: Vec<&str> = Vec::new();
    let mut last = 0;
    for m in SPLIT.find_iter(body) {
        tokens.push(&body[last..m.start()]);
        tokens.push(m.as_str());
        last = m.end();
    }
    tokens.push(&body[last..]);
    let mut prev = String::new();
    let mut masked = String::with_capacity(body.len());
    for t in tokens {
        if (4..=8).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_digit()) && !IDENT.is_match(&prev) {
            masked.push_str("{otp}");
            continue;
        }
        if !t.trim().is_empty() && !SEP_ONLY.is_match(t) {
            prev = t.to_owned();
        }
        masked.push_str(t);
    }
    sha256_hex(inboxora_mail::text::collapse_ws(&masked).trim())
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeriesDecision {
    pub kind: Option<&'static str>,
    pub confidence: Option<f64>,
}

fn ts(d: Option<DateTime<Utc>>) -> i64 {
    d.map(|t| t.timestamp_millis()).unwrap_or(0)
}

pub fn strict_series(message: &SeriesInput, previous: &SeriesDigest) -> Option<SeriesDecision> {
    let (now, prior) = (ts(message.date), ts(previous.date));
    let s = signals(message);
    let current = message.digest();
    let subject = message.canonical_subject.to_lowercase();
    if subject.is_empty() || GENERIC_SUBJECTS.contains(&subject.as_str()) {
        return None;
    }
    if !s.authenticated
        || !previous.authenticated
        || current.sender_hash != previous.sender_hash
        || current.recipients_hash != previous.recipients_hash
    {
        return None;
    }
    if current.subject_hash != previous.subject_hash {
        return None;
    }
    if now - prior > 7 * 24 * 60 * 60 * 1000 || now < prior {
        return None;
    }
    match (&message.references_anchor, &previous.references_anchor) {
        (Some(a), Some(b)) if a == b => {}
        _ => return None,
    }
    if previous.logical_message_count >= MAX_SEGMENT {
        return Some(SeriesDecision { kind: None, confidence: None });
    }
    Some(SeriesDecision { kind: Some("automated_reference_series"), confidence: Some(0.98) })
}

pub fn smart_series(message: &SeriesInput, previous: &SeriesDigest) -> Option<SeriesDecision> {
    static OTP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)(?:otp|one[- ]time|verification|security|code|kod|weryfik|passcode|hasło)")
            .expect("regex")
    });
    let (now, prior) = (ts(message.date), ts(previous.date));
    let current = message.digest();
    if !current.automated
        || !previous.automated
        || current.sender_hash != previous.sender_hash
        || current.recipients_hash != previous.recipients_hash
    {
        return None;
    }
    let body = message.body_text.clone().unwrap_or_default();
    if !OTP.is_match(&format!("{} {body}", message.canonical_subject)) {
        return None;
    }
    let previous_template =
        previous.body_template.clone().unwrap_or_else(|| body_template_fingerprint("")[..16].to_owned());
    if now - prior > SERIES_WINDOW_MS || now < prior || body_template_fingerprint(&body)[..16] != previous_template {
        return None;
    }
    if previous.logical_message_count >= MAX_SEGMENT {
        return Some(SeriesDecision { kind: None, confidence: None });
    }
    Some(SeriesDecision { kind: Some("automated_smart_series"), confidence: Some(0.9) })
}

/// `referencesAnchor`: the References root, else the direct reply target.
pub fn references_anchor(references: Option<&str>, in_reply_to: Option<&str>) -> Option<String> {
    let refs = normalize_message_id_list(references.unwrap_or(""));
    if let Some(first) = refs.into_iter().next() {
        return Some(first);
    }
    normalize_message_id_list(in_reply_to.unwrap_or("")).pop()
}

// ── upsert ──────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UpsertResult {
    pub logical_message_id: Uuid,
    pub conversation_id: Option<Uuid>,
}

struct Hydrated {
    user_id: String,
    account_id: String,
    raw_in_reply_to: Option<String>,
    raw_references: Option<String>,
    canonical_message_id: Option<String>,
    canonical_subject: String,
    collision_key: String,
    direction: &'static str,
    message_date: Option<DateTime<Utc>>,
    body_fingerprint: Option<String>,
    header_fingerprint: Option<String>,
}

fn str_of(m: &Map<String, Value>, k: &str) -> Option<String> {
    match m.get(k) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(inboxora_mail::text::js_string(other)),
    }
}

fn truthy_str(m: &Map<String, Value>, k: &str) -> Option<String> {
    str_of(m, k).filter(|s| !s.is_empty())
}

fn hydrate(source: &Map<String, Value>, date: Option<DateTime<Utc>>, user_id: &str, identities: &[Value]) -> Hydrated {
    let raw_id = truthy_str(source, "message_id");
    let from = source
        .get("from_email")
        .filter(|v| inboxora_mail::text_truthy(v))
        .or_else(|| source.get("from").filter(|v| inboxora_mail::text_truthy(v)))
        .or_else(|| source.get("sender"));
    let identity = logical_message_identity(
        raw_id.as_deref(),
        &date.map_or(MessageDate::None, MessageDate::JsDate),
        str_of(source, "subject").as_deref(),
        from,
        Some(user_id),
    );
    let body = source.get("body_text").filter(|v| !v.is_null()).map(inboxora_mail::text::js_string);
    let header_fp = js_json(&json!([
        source.get("message_id").cloned().unwrap_or(Value::Null),
        source.get("in_reply_to").cloned().unwrap_or(Value::Null),
        source.get("thread_references").cloned().unwrap_or(Value::Null),
        source.get("conversation_raw_headers").cloned().unwrap_or(Value::Null),
    ]));
    // Fingerprints only identify copies that have no Message-ID; 64 bits tell collisions apart.
    let no_id = identity.canonical_message_id.is_none();
    Hydrated {
        user_id: user_id.to_owned(),
        account_id: str_of(source, "account_id").unwrap_or_default(),
        raw_in_reply_to: truthy_str(source, "in_reply_to"),
        raw_references: truthy_str(source, "thread_references"),
        canonical_message_id: identity.canonical_message_id,
        canonical_subject: canonical_subject(&str_of(source, "subject").unwrap_or_default()),
        collision_key: identity.collision_key.chars().take(16).collect(),
        direction: classify_direction(&Value::Object(source.clone()), identities),
        message_date: date,
        body_fingerprint: if no_id { body.map(|b| sha256_hex(&b)) } else { None },
        header_fingerprint: no_id.then(|| sha256_hex(&header_fp)),
    }
}

#[derive(Debug, Clone)]
struct Logical {
    id: Uuid,
    conversation_id: Option<Uuid>,
    parent_logical_message_id: Option<Uuid>,
}

fn logical_of(r: &Row) -> Logical {
    Logical {
        id: r.get("id"),
        conversation_id: r.get("conversation_id"),
        parent_logical_message_id: r.try_get("parent_logical_message_id").ok().flatten(),
    }
}

/// Older rows whose collision key was computed by a previous algorithm used to be matched by
/// re-deriving the key from the stored subject/sender; with no text stored, only rows sharing
/// the current key are consolidated.
async fn matching_legacy_rows(tx: &Tx<'_>, h: &Hydrated) -> Result<Vec<Logical>> {
    let Some(canonical) = &h.canonical_message_id else { return Ok(vec![]) };
    let rows = tx
        .rows(
            "SELECT id, conversation_id, parent_logical_message_id FROM logical_messages \
              WHERE user_id = $1 AND account_id = $3 AND canonical_message_id = $2 AND message_id_collision_key = $4 \
              ORDER BY created_at ASC, id FOR UPDATE",
            params![&h.user_id, canonical, &h.account_id, &h.collision_key],
        )
        .await?;
    Ok(rows.iter().map(logical_of).collect())
}

async fn consolidate_legacy(
    tx: &Tx<'_>,
    h: &Hydrated,
    rows: Vec<Logical>,
    preferred: Option<Uuid>,
) -> Result<Option<Logical>> {
    let mut unique: Vec<Logical> = Vec::new();
    for r in rows {
        if let Some(slot) = unique.iter_mut().find(|u| u.id == r.id) {
            *slot = r;
        } else {
            unique.push(r);
        }
    }
    if unique.is_empty() {
        return Ok(None);
    }
    let ids: Vec<Uuid> = unique.iter().map(|r| r.id).collect();
    let mut conv_ids: Vec<Uuid> = Vec::new();
    for c in unique.iter().filter_map(|r| r.conversation_id) {
        if !conv_ids.contains(&c) {
            conv_ids.push(c);
        }
    }
    let winner = unique.iter().find(|r| Some(r.id) == preferred).unwrap_or(&unique[0]).clone();
    let protected = tx
        .opt(
            "SELECT (EXISTS (SELECT 1 FROM conversation_overrides WHERE user_id = $1 AND account_id = $4 \
                AND (logical_message_id = ANY($2::uuid[]) OR conversation_id = ANY($3::uuid[]))) \
              OR EXISTS (SELECT 1 FROM conversations WHERE user_id = $1 AND account_id = $4 AND id = ANY($3::uuid[]) AND manually_locked = true)) AS protected",
            params![&h.user_id, uuids(&ids), uuids(&conv_ids), &h.account_id],
        )
        .await?
        .is_some_and(|r| r.get::<_, Option<bool>>("protected").unwrap_or(false));
    if protected {
        return Ok(Some(winner));
    }
    let losers: Vec<Uuid> = unique.iter().filter(|r| r.id != winner.id).map(|r| r.id).collect();
    if !losers.is_empty() {
        let l = uuids(&losers);
        tx.exec(
            "INSERT INTO unresolved_message_references (user_id, account_id, child_logical_message_id, referenced_message_id, relation_type, \
               reference_position, resolved_logical_message_id, resolved_at, created_at) \
             SELECT user_id, account_id, $1, referenced_message_id, relation_type, reference_position, \
                    CASE WHEN resolved_logical_message_id = ANY($2::uuid[]) THEN $1 ELSE resolved_logical_message_id END, resolved_at, created_at \
               FROM unresolved_message_references WHERE user_id = $3 AND account_id = $4 AND child_logical_message_id = ANY($2::uuid[]) \
             ON CONFLICT DO NOTHING",
            params![winner.id, l.clone(), &h.user_id, &h.account_id],
        )
        .await?;
        tx.exec(
            "DELETE FROM unresolved_message_references WHERE user_id = $1 AND account_id = $3 AND child_logical_message_id = ANY($2::uuid[])",
            params![&h.user_id, l.clone(), &h.account_id],
        )
        .await?;
        tx.exec(
            "UPDATE unresolved_message_references SET resolved_logical_message_id = $1 WHERE user_id = $2 AND account_id = $4 AND resolved_logical_message_id = ANY($3::uuid[])",
            params![winner.id, &h.user_id, l.clone(), &h.account_id],
        )
        .await?;
        tx.exec(
            "INSERT INTO conversation_evidence (user_id, account_id, conversation_id, logical_message_id, evidence_type, \
               evidence_value_hash, weight, algorithm_version, details, created_at) \
             SELECT user_id, account_id, COALESCE($1, conversation_id), $2, evidence_type, evidence_value_hash, weight, algorithm_version, details, created_at \
               FROM conversation_evidence WHERE user_id = $3 AND account_id = $5 AND logical_message_id = ANY($4::uuid[]) \
             ON CONFLICT DO NOTHING",
            params![winner.conversation_id, winner.id, &h.user_id, l.clone(), &h.account_id],
        )
        .await?;
        tx.exec(
            "DELETE FROM conversation_evidence WHERE user_id = $1 AND account_id = $3 AND logical_message_id = ANY($2::uuid[])",
            params![&h.user_id, l.clone(), &h.account_id],
        )
        .await?;
        tx.exec(
            "UPDATE logical_messages SET parent_logical_message_id = $1, updated_at = NOW() \
             WHERE user_id = $2 AND account_id = $4 AND parent_logical_message_id = ANY($3::uuid[]) AND id <> $1",
            params![winner.id, &h.user_id, l.clone(), &h.account_id],
        )
        .await?;
        if winner.parent_logical_message_id.is_some_and(|p| losers.contains(&p)) {
            tx.exec(
                "UPDATE logical_messages SET parent_logical_message_id = NULL, updated_at = NOW() WHERE id = $1 AND user_id = $2 AND account_id = $3",
                params![winner.id, &h.user_id, &h.account_id],
            )
            .await?;
        }
        tx.exec(
            "UPDATE messages m SET logical_message_id = $1, conversation_id = $2, conversation_user_id = $3 \
             WHERE m.logical_message_id = ANY($4::uuid[]) AND m.account_id = $5 \
               AND EXISTS (SELECT 1 FROM email_accounts a WHERE a.id = m.account_id AND a.user_id = $3)",
            params![winner.id, winner.conversation_id, &h.user_id, l.clone(), &h.account_id],
        )
        .await?;
        tx.exec(
            "DELETE FROM logical_messages WHERE id = ANY($1::uuid[]) AND user_id = $2 AND account_id = $3",
            params![l, &h.user_id, &h.account_id],
        )
        .await?;
    }
    tx.exec(
        "UPDATE logical_messages SET message_id_collision_key = $1, updated_at = NOW() WHERE id = $2 AND user_id = $3 AND account_id = $4",
        params![&h.collision_key, winner.id, &h.user_id, &h.account_id],
    )
    .await?;
    for c in &conv_ids {
        refresh_aggregates(tx, &h.user_id, *c).await?;
    }
    if !conv_ids.is_empty() {
        tx.exec(DELETE_ORPHANS, params![&h.user_id, uuids(&conv_ids)]).await?;
    }
    Ok(Some(winner))
}

async fn find_existing(tx: &Tx<'_>, h: &Hydrated, repair: bool) -> Result<(Option<Logical>, bool)> {
    if let Some(canonical) = &h.canonical_message_id {
        let row = tx
            .opt(
                "SELECT id, conversation_id, message_id_collision_key FROM logical_messages \
                 WHERE user_id = $1 AND account_id = $4 AND canonical_message_id = $2 AND message_id_collision_key = $3 \
                 ORDER BY created_at ASC LIMIT 1 FOR UPDATE",
                params![&h.user_id, canonical, &h.collision_key, &h.account_id],
            )
            .await?;
        if let Some(r) = row {
            let first = logical_of(&r);
            if !repair {
                return Ok((Some(first), false));
            }
            let legacy = matching_legacy_rows(tx, h).await?;
            let mut matching = vec![first.clone()];
            matching.extend(legacy.into_iter().filter(|l| l.id != first.id));
            return Ok((consolidate_legacy(tx, h, matching, Some(first.id)).await?, false));
        }
        let legacy = if repair { matching_legacy_rows(tx, h).await? } else { vec![] };
        if !legacy.is_empty() {
            return Ok((consolidate_legacy(tx, h, legacy, None).await?, false));
        }
        let exists = tx
            .opt(
                "SELECT 1 FROM logical_messages WHERE user_id = $1 AND account_id = $3 AND canonical_message_id = $2 LIMIT 1",
                params![&h.user_id, canonical, &h.account_id],
            )
            .await?
            .is_some();
        return Ok((None, exists));
    }
    let row = tx
        .opt(
            "SELECT id, conversation_id, message_id_collision_key FROM logical_messages \
             WHERE user_id = $1 AND account_id = $4 AND canonical_message_id IS NULL AND body_fingerprint = $2 AND header_fingerprint = $3 \
             ORDER BY created_at ASC LIMIT 1 FOR UPDATE",
            params![&h.user_id, &h.body_fingerprint, &h.header_fingerprint, &h.account_id],
        )
        .await?;
    Ok((row.as_ref().map(logical_of), false))
}

#[derive(Debug, Clone)]
struct Parent {
    id: Option<Uuid>,
    conversation_id: Option<Uuid>,
    canonical_message_id: Option<String>,
    ambiguous: bool,
    relation_type: &'static str,
}

async fn find_parent(tx: &Tx<'_>, h: &Hydrated) -> Result<Option<Parent>> {
    let pick = |r: &Row, rel: &'static str| Parent {
        id: Some(r.get("id")),
        conversation_id: r.get("conversation_id"),
        canonical_message_id: r.get("canonical_message_id"),
        ambiguous: false,
        relation_type: rel,
    };
    if let Some(reply) = normalize_message_id_list(h.raw_in_reply_to.as_deref().unwrap_or("")).pop() {
        let rows = tx
            .rows(
                "SELECT id, conversation_id, canonical_message_id FROM logical_messages \
                 WHERE user_id = $1 AND account_id = $3 AND canonical_message_id = $2 ORDER BY created_at ASC FOR UPDATE",
                params![&h.user_id, &reply, &h.account_id],
            )
            .await?;
        if rows.len() == 1 {
            return Ok(Some(pick(&rows[0], "in-reply-to")));
        }
        if rows.len() > 1 {
            return Ok(Some(Parent {
                id: None,
                conversation_id: None,
                canonical_message_id: Some(reply),
                ambiguous: true,
                relation_type: "ambiguous-in-reply-to",
            }));
        }
    }
    let refs = normalize_message_id_list(h.raw_references.as_deref().unwrap_or(""));
    if !refs.is_empty() {
        let reversed: Vec<String> = refs.into_iter().rev().collect();
        let rows = tx
            .rows(
                "SELECT id, conversation_id, canonical_message_id FROM logical_messages \
                 WHERE user_id = $1 AND account_id = $3 AND canonical_message_id = ANY($2::text[]) ORDER BY created_at ASC FOR UPDATE",
                params![&h.user_id, reversed.clone(), &h.account_id],
            )
            .await?;
        let mut by_id: HashMap<String, Vec<&Row>> = HashMap::new();
        for r in &rows {
            let k: Option<String> = r.get("canonical_message_id");
            by_id.entry(k.unwrap_or_default()).or_default().push(r);
        }
        for id in reversed {
            match by_id.get(&id).map(Vec::as_slice) {
                None | Some([]) => continue,
                Some([one]) => return Ok(Some(pick(one, "references"))),
                Some(_) => {
                    return Ok(Some(Parent {
                        id: None,
                        conversation_id: None,
                        canonical_message_id: Some(id),
                        ambiguous: true,
                        relation_type: "ambiguous-references",
                    }));
                }
            }
        }
    }
    Ok(None)
}

/// Upsert one physical copy (by row id) into its logical message and conversation. Runs in a
/// serializable transaction; `user_id` is the authenticated tenant (never taken from the row).
pub async fn upsert_copy(
    db: &Db,
    row_id: Uuid,
    facts: &CopyFacts,
    identities: &[String],
    provider: Option<&Provider>,
    user_id: &str,
) -> Result<UpsertResult> {
    let ids: Vec<Value> = identities.iter().map(|s| Value::String(s.clone())).collect();
    let provider = provider.cloned();
    let user = user_id.to_owned();
    let facts = facts.clone();
    db.transaction(true, 2, move |tx| {
        let (ids, provider, user, facts) = (ids.clone(), provider.clone(), user.clone(), facts.clone());
        Box::pin(async move { upsert_copy_tx(tx, row_id, &facts, &ids, provider.as_ref(), &user, false).await })
    })
    .await
}

pub async fn upsert_copy_tx(
    tx: &Tx<'_>,
    row_id: Uuid,
    facts: &CopyFacts,
    identities: &[Value],
    provider: Option<&Provider>,
    user_id: &str,
    repair: bool,
) -> Result<UpsertResult> {
    let verified = tx
        .opt(
            "SELECT m.*, a.user_id, a.automated_series_mode FROM messages m JOIN email_accounts a ON a.id = m.account_id \
             WHERE m.id = $1 AND a.user_id = $2 FOR UPDATE",
            params![row_id, user_id],
        )
        .await?
        .ok_or_else(|| Error::internal("Conversation copy not found or owner mismatch"))?;
    let date: Option<DateTime<Utc>> = verified.get("date");
    let mut source = inboxora_core::pg::row_to_json(&verified).map_err(Error::from)?;
    facts.overlay(&mut source);
    let h = hydrate(&source, date, user_id, identities);
    let parent = find_parent(tx, &h).await?;
    let usable_parent = parent.as_ref().filter(|p| !p.ambiguous);

    // threadingDecision
    let strong = provider.is_some_and(|p| p.is_strong && p.provider_thread_id.is_some());
    let (mut kind, mut reason, mut confidence): (String, Option<String>, f64) = if strong {
        ("provider_thread".into(), provider.and_then(|p| p.source.clone()), 1.0)
    } else if usable_parent.is_some() {
        ("human_reply_chain".into(), Some("rfc-in-reply-to".into()), 0.99)
    } else {
        ("human_reply_chain".into(), Some("new-root".into()), 0.5)
    };

    let series_mode =
        str_of(&source, "automated_series_mode").filter(|s| !s.is_empty()).unwrap_or_else(|| "off".into());
    let message = SeriesInput {
        headers: Map::new(),
        from_email: str_of(&source, "from_email"),
        to_addresses: source.get("to_addresses").cloned().unwrap_or(Value::Null),
        date,
        body_text: str_of(&source, "body_text"),
        canonical_subject: h.canonical_subject.clone(),
        references_anchor: references_anchor(
            truthy_str(&source, "thread_references").as_deref(),
            truthy_str(&source, "in_reply_to").as_deref(),
        ),
    };
    let digest = message.digest();
    let mut series: Option<SeriesDecision> = None;
    let mut matched_previous: Option<(Uuid, Option<Uuid>)> = None;
    if series_mode == "strict" || series_mode == "smart" {
        let candidates = tx
            .rows(
                "SELECT lm.id, lm.conversation_id, lm.subject_hash, lm.references_anchor, lm.series_sender_hash, \
                   lm.series_recipients_hash, lm.series_automated, lm.body_template_fingerprint, lm.message_date, c.logical_message_count \
                   FROM logical_messages lm \
                   JOIN conversations c ON c.id = lm.conversation_id AND c.user_id = lm.user_id AND c.account_id = lm.account_id \
                  WHERE lm.user_id = $1 AND lm.account_id = $3 AND lm.subject_hash = $2 \
                    AND lm.message_date > NOW() - INTERVAL '7 days' \
                    AND EXISTS (SELECT 1 FROM messages m WHERE m.logical_message_id = lm.id AND m.account_id = lm.account_id AND m.is_deleted = false) \
                  ORDER BY lm.message_date DESC NULLS LAST LIMIT 10",
                params![user_id, subject_hash(&h.canonical_subject), &h.account_id],
            )
            .await?;
        for c in &candidates {
            let previous = SeriesDigest {
                subject_hash: c.get("subject_hash"),
                sender_hash: c.get::<_, Option<String>>("series_sender_hash").unwrap_or_default(),
                recipients_hash: c.get::<_, Option<String>>("series_recipients_hash").unwrap_or_default(),
                automated: c.get::<_, Option<bool>>("series_automated").unwrap_or(false),
                authenticated: false,
                references_anchor: c.get("references_anchor"),
                body_template: c.get("body_template_fingerprint"),
                date: c.get("message_date"),
                logical_message_count: f64::from(c.get::<_, Option<i32>>("logical_message_count").unwrap_or(0)),
            };
            let decision = if series_mode == "strict" {
                strict_series(&message, &previous)
            } else {
                smart_series(&message, &previous)
            };
            if let Some(d) = decision {
                series = Some(d);
                matched_previous = Some((c.get("id"), c.get("conversation_id")));
                break;
            }
        }
    }
    if let Some(s) = &series
        && !parent.as_ref().is_some_and(|p| p.ambiguous)
    {
        if let Some(k) = s.kind {
            kind = k.into();
            reason = Some(k.into());
        }
        if let Some(c) = s.confidence {
            confidence = c;
        }
    }
    if let Some(p) = parent.as_ref().filter(|p| p.ambiguous) {
        reason = Some(p.relation_type.into());
    }

    let (existing, collision) = find_existing(tx, &h, repair).await?;
    let in_reply_list = normalize_message_id_list(h.raw_in_reply_to.as_deref().unwrap_or(""));
    let refs_list = normalize_message_id_list(h.raw_references.as_deref().unwrap_or(""));
    let logical = match existing {
        Some(l) => {
            tx.exec(
                "UPDATE logical_messages SET updated_at = NOW(), threading_reason = $2, threading_confidence = $3, \
                   body_template_fingerprint = COALESCE(body_template_fingerprint, $5) WHERE id = $1 AND account_id = $4",
                params![l.id, &reason, confidence, &h.account_id, &digest.body_template],
            )
            .await?;
            l
        }
        None => {
            let inserted = tx
                .opt(
                    "INSERT INTO logical_messages (user_id, account_id, canonical_message_id, message_id_collision_key, direction, message_date, \
                       body_fingerprint, header_fingerprint, threading_reason, threading_confidence, subject_hash, references_anchor, \
                       series_sender_hash, series_recipients_hash, series_automated, body_template_fingerprint) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) ON CONFLICT DO NOTHING RETURNING id, conversation_id",
                    params![
                        user_id,
                        &h.account_id,
                        &h.canonical_message_id,
                        &h.collision_key,
                        h.direction,
                        h.message_date,
                        &h.body_fingerprint,
                        &h.header_fingerprint,
                        &reason,
                        confidence,
                        &digest.subject_hash,
                        &digest.references_anchor,
                        &digest.sender_hash,
                        &digest.recipients_hash,
                        digest.automated,
                        &digest.body_template
                    ],
                )
                .await?;
            let winner = match inserted {
                Some(r) => Some(logical_of(&r)),
                None => match &h.canonical_message_id {
                    Some(c) => tx
                        .opt(
                            "SELECT id, conversation_id FROM logical_messages WHERE user_id = $1 AND account_id = $4 AND canonical_message_id = $2 AND message_id_collision_key = $3 FOR UPDATE",
                            params![user_id, c, &h.collision_key, &h.account_id],
                        )
                        .await?
                        .as_ref()
                        .map(logical_of),
                    None => tx
                        .opt(
                            "SELECT id, conversation_id FROM logical_messages WHERE user_id = $1 AND account_id = $4 AND canonical_message_id IS NULL AND body_fingerprint = $2 AND header_fingerprint = $3 ORDER BY created_at ASC LIMIT 1 FOR UPDATE",
                            params![user_id, &h.body_fingerprint, &h.header_fingerprint, &h.account_id],
                        )
                        .await?
                        .as_ref()
                        .map(logical_of),
                },
            };
            winner.ok_or_else(|| Error::internal("Logical message insert raced without a recoverable winner"))?
        }
    };

    // findProviderConversation
    let provider_usable = provider.is_some_and(|p| {
        (p.is_strong || p.source.as_deref() == Some("outlook-conversation-index-root"))
            && p.provider_thread_id.is_some()
    });
    let provider_conversation: Option<Uuid> = if provider_usable && !h.account_id.is_empty() {
        let p = provider.expect("usable provider");
        tx.opt(
            "SELECT conversation_id FROM provider_thread_mappings WHERE user_id = $1 AND account_id = $2 AND provider = $3 AND provider_thread_id = $4 FOR UPDATE",
            params![user_id, &h.account_id, &p.provider, &p.provider_thread_id],
        )
        .await?
        .and_then(|r| r.get("conversation_id"))
    } else {
        None
    };
    let parent_conv = usable_parent.and_then(|p| p.conversation_id);
    let mut conversation_id = if repair {
        parent_conv.or(provider_conversation).or(logical.conversation_id)
    } else {
        logical.conversation_id.or(provider_conversation).or(parent_conv)
    };
    if matches!(series.as_ref().and_then(|s| s.kind), Some("automated_reference_series" | "automated_smart_series")) {
        conversation_id = matched_previous.and_then(|(_, c)| c).or(conversation_id);
    }
    let override_conv =
        if repair && logical.conversation_id.is_some() { logical.conversation_id } else { conversation_id };
    let ov = effective_override(tx, user_id, &h.account_id, override_conv, Some(logical.id)).await?;
    let acct = Some(h.account_id.as_str());
    let mut requested: Option<Uuid> = None;
    if let Some(t) = ov.merge.as_ref().and_then(|m| m.target_id) {
        requested = Some(resolve_alias(tx, user_id, acct, t).await?);
    }
    if let Some(split) = &ov.split {
        requested = match split.target_id {
            Some(t) => Some(resolve_alias(tx, user_id, acct, t).await?),
            None => logical.conversation_id,
        };
    }
    if ov.locked == Some(true) && logical.conversation_id.is_some() {
        conversation_id = logical.conversation_id;
    }
    if let Some(t) = ov.mv.as_ref().and_then(|m| m.target_id) {
        requested = Some(resolve_alias(tx, user_id, acct, t).await?);
    }
    let previous_conversation = logical.conversation_id;
    if ov.force_exclude.is_some() {
        tx.exec(
            "UPDATE messages SET conversation_id = NULL, threading_reason = $1, threading_confidence = 0 WHERE logical_message_id = $2 AND conversation_user_id = $3 AND account_id = $4",
            params!["manual-force-exclude", logical.id, user_id, &h.account_id],
        )
        .await?;
        tx.exec(
            "UPDATE logical_messages SET conversation_id = NULL, parent_logical_message_id = NULL, updated_at = NOW() WHERE id = $1 AND user_id = $2 AND account_id = $3",
            params![logical.id, user_id, &h.account_id],
        )
        .await?;
        if let Some(prev) = previous_conversation {
            tx.exec(
                "DELETE FROM conversation_evidence WHERE logical_message_id = $1 AND conversation_id = $2 AND user_id = $3 AND account_id = $4",
                params![logical.id, prev, user_id, &h.account_id],
            )
            .await?;
        }
        tx.exec(
            "UPDATE unresolved_message_references SET resolved_logical_message_id = NULL, resolved_at = NULL WHERE resolved_logical_message_id = $1 AND user_id = $2 AND account_id = $3",
            params![logical.id, user_id, &h.account_id],
        )
        .await?;
        if let Some(prev) = previous_conversation {
            refresh_aggregates(tx, user_id, prev).await?;
        }
        return Ok(UpsertResult { logical_message_id: logical.id, conversation_id: None });
    }
    if let Some(t) = ov.force_include.as_ref().and_then(|m| m.target_id) {
        requested = Some(resolve_alias(tx, user_id, acct, t).await?);
    }
    if ov.locked == Some(true)
        && let Some(c) = conversation_id
    {
        requested = Some(resolve_alias(tx, user_id, acct, c).await?);
    }
    if requested.is_some() {
        conversation_id = requested;
    }
    let conversation_id = match conversation_id {
        Some(c) => c,
        None => tx
            .opt(
                "INSERT INTO conversations (user_id, account_id, kind, first_message_at, last_message_at, \
                   logical_message_count, copy_count, unread_count, threading_confidence) \
                 VALUES ($1,$2,$3,$4,$4,0,0,0,$5) RETURNING id",
                params![user_id, &h.account_id, &kind, h.message_date, confidence],
            )
            .await?
            .map(|r| r.get::<_, Uuid>("id"))
            .ok_or_else(|| Error::internal("conversation insert returned nothing"))?,
    };
    let changed = previous_conversation != Some(conversation_id);
    tx.exec(
        "UPDATE logical_messages SET conversation_id = $1, parent_logical_message_id = CASE WHEN $4::boolean THEN $2 ELSE COALESCE(parent_logical_message_id, $2) END, updated_at = NOW() WHERE id = $3 AND account_id = $5",
        params![conversation_id, usable_parent.and_then(|p| p.id), logical.id, repair, &h.account_id],
    )
    .await?;
    let ns = provider.and_then(|p| p.namespace.clone().or_else(|| p.provider.clone()));
    let attached = tx
        .exec(
            "UPDATE messages SET logical_message_id = $1, conversation_id = $2, conversation_user_id = $3, canonical_message_id = $4, \
               provider_message_id = COALESCE($5, provider_message_id), provider_thread_id = COALESCE($6, provider_thread_id), \
               provider_namespace = COALESCE($7, provider_namespace), \
               thread_id = CASE WHEN $7 LIKE 'gmail:%' AND $6 IS NOT NULL THEN 'gmail:' || $6 ELSE thread_id END, \
               threading_reason = $8, threading_confidence = $9, threading_algorithm_version = 'conversation-v2', row_version = row_version + 1 \
             WHERE id = $10 RETURNING id",
            params![
                logical.id,
                conversation_id,
                user_id,
                &h.canonical_message_id,
                provider.and_then(|p| p.provider_message_id.clone()),
                provider.and_then(|p| p.provider_thread_id.clone()),
                ns,
                &reason,
                confidence,
                row_id
            ],
        )
        .await?;
    if attached != 1 {
        return Err(Error::internal("Conversation copy attachment failed"));
    }
    if changed {
        tx.exec(
            "UPDATE messages SET conversation_id = $1, conversation_user_id = $2 WHERE logical_message_id = $3 AND conversation_user_id = $2 AND account_id = $4",
            params![conversation_id, user_id, logical.id, &h.account_id],
        )
        .await?;
        if let Some(prev) = previous_conversation {
            refresh_aggregates(tx, user_id, prev).await?;
            tx.exec(DELETE_ORPHANS, params![user_id, vec![prev.to_string()]]).await?;
        }
    }
    if provider_usable {
        let p = provider.expect("usable provider");
        tx.exec(
            "INSERT INTO provider_thread_mappings (user_id, account_id, provider, provider_thread_id, conversation_id, last_seen_at, diagnostics) \
             VALUES ($1,$2,$3,$4,$5,NOW(),$6::jsonb) ON CONFLICT (user_id, account_id, provider, provider_thread_id) \
             DO UPDATE SET conversation_id = EXCLUDED.conversation_id, last_seen_at = NOW(), diagnostics = EXCLUDED.diagnostics",
            params![user_id, &h.account_id, &p.provider, &p.provider_thread_id, conversation_id, js_json(&p.diagnostics)],
        )
        .await?;
    }
    let mut unresolved: Vec<String> = Vec::new();
    for id in in_reply_list.iter().chain(refs_list.iter()) {
        if !unresolved.contains(id) && Some(id) != h.canonical_message_id.as_ref() {
            unresolved.push(id.clone());
        }
    }
    if !unresolved.is_empty() {
        let known: HashSet<String> = tx
            .rows(
                "SELECT id, canonical_message_id FROM logical_messages WHERE user_id = $1 AND account_id = $3 AND canonical_message_id = ANY($2::text[])",
                params![user_id, unresolved.clone(), &h.account_id],
            )
            .await?
            .iter()
            .filter_map(|r| r.get::<_, Option<String>>("canonical_message_id"))
            .collect();
        let last_reply = in_reply_list.last();
        for (position, referenced) in unresolved.iter().enumerate() {
            if known.contains(referenced) {
                continue;
            }
            let relation = if Some(referenced) == last_reply { "in-reply-to" } else { "references" };
            tx.exec(
                "INSERT INTO unresolved_message_references (user_id, account_id, child_logical_message_id, referenced_message_id, relation_type, reference_position) \
                 VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING",
                params![user_id, &h.account_id, logical.id, referenced, relation, position as i64],
            )
            .await?;
        }
    }
    let waiting = tx
        .rows(
            "SELECT id, child_logical_message_id FROM unresolved_message_references \
             WHERE user_id = $1 AND account_id = $3 AND referenced_message_id = $2 AND resolved_at IS NULL FOR UPDATE",
            params![user_id, &h.canonical_message_id, &h.account_id],
        )
        .await?;
    for reference in &waiting {
        let ref_id: Uuid = reference.get("id");
        let child: Uuid = reference.get("child_logical_message_id");
        tx.exec(
            "UPDATE unresolved_message_references SET resolved_logical_message_id = $1, resolved_at = NOW() WHERE id = $2",
            params![logical.id, ref_id],
        )
        .await?;
        let component = tx
            .rows(
                "WITH RECURSIVE component(id, path) AS ( \
                   SELECT id, ARRAY[id] FROM logical_messages WHERE id = $1 AND user_id = $2 AND account_id = $3 \
                   UNION ALL \
                   SELECT lm.id, component.path || lm.id FROM logical_messages lm JOIN component ON lm.parent_logical_message_id = component.id \
                    WHERE lm.user_id = $2 AND lm.account_id = $3 AND NOT lm.id = ANY(component.path) \
                 ) SELECT lm.id, lm.conversation_id, c.manually_locked FROM logical_messages lm JOIN component ON component.id = lm.id \
                   LEFT JOIN conversations c ON c.id = lm.conversation_id",
                params![child, user_id, &h.account_id],
            )
            .await?;
        let nodes: Vec<(Uuid, Option<Uuid>, bool)> = component
            .iter()
            .map(|r| {
                (r.get("id"), r.get("conversation_id"), r.get::<_, Option<bool>>("manually_locked").unwrap_or(false))
            })
            .collect();
        let mut old_convs: Vec<Uuid> = Vec::new();
        for c in nodes.iter().filter_map(|n| n.1) {
            if !old_convs.contains(&c) {
                old_convs.push(c);
            }
        }
        let logical_ids: Vec<Uuid> = nodes.iter().map(|n| n.0).collect();
        let conv_ov: Vec<(String, Option<Uuid>)> = if old_convs.is_empty() {
            vec![]
        } else {
            tx.rows(
                "SELECT override_type, conversation_id FROM conversation_overrides WHERE user_id = $1 AND account_id = $3 AND conversation_id = ANY($2::uuid[]) AND logical_message_id IS NULL ORDER BY created_at DESC, id DESC",
                params![user_id, uuids(&old_convs), &h.account_id],
            )
            .await?
            .iter()
            .map(|r| (r.get("override_type"), r.get("conversation_id")))
            .collect()
        };
        let msg_ov: Vec<(String, Option<Uuid>)> = if logical_ids.is_empty() {
            vec![]
        } else {
            tx.rows(
                "SELECT override_type, logical_message_id FROM conversation_overrides WHERE user_id = $1 AND account_id = $3 AND logical_message_id = ANY($2::uuid[]) ORDER BY created_at DESC, id DESC",
                params![user_id, uuids(&logical_ids), &h.account_id],
            )
            .await?
            .iter()
            .map(|r| (r.get("override_type"), r.get("logical_message_id")))
            .collect()
        };
        let blocked = nodes.iter().any(|(id, conv, locked)| {
            let conv_has = |t: &str| conv_ov.iter().any(|(ty, c)| ty == t && c == conv && conv.is_some());
            let msg_has = |t: &str| msg_ov.iter().any(|(ty, l)| ty == t && *l == Some(*id));
            *locked
                || conv_has("force-exclude")
                || msg_has("force-exclude")
                || conv_has("force-include")
                || msg_has("force-include")
                || conv_has("manual-split")
                || msg_has("manual-split")
                || conv_has("lock-conversation")
                || conv_has("unlock-conversation")
        });
        if !blocked && !nodes.is_empty() && old_convs.iter().any(|c| *c != conversation_id) {
            tx.exec(
                "UPDATE logical_messages SET conversation_id = $1, parent_logical_message_id = CASE WHEN id = $2 THEN $3 ELSE parent_logical_message_id END, updated_at = NOW() \
                 WHERE id = ANY($4::uuid[]) AND user_id = $5 AND account_id = $6",
                params![conversation_id, child, logical.id, uuids(&logical_ids), user_id, &h.account_id],
            )
            .await?;
            tx.exec(
                "UPDATE messages SET conversation_id = $1, conversation_user_id = $2 WHERE logical_message_id = ANY($3::uuid[]) AND conversation_user_id = $2 AND account_id = $4",
                params![conversation_id, user_id, uuids(&logical_ids), &h.account_id],
            )
            .await?;
            let mut touched = old_convs.clone();
            if !touched.contains(&conversation_id) {
                touched.push(conversation_id);
            }
            for t in touched {
                refresh_aggregates(tx, user_id, t).await?;
            }
            tx.exec(DELETE_ORPHANS, params![user_id, uuids(&old_convs)]).await?;
            let cross = tx
                .opt(
                    "SELECT 1 FROM logical_messages child JOIN logical_messages parent ON parent.id = child.parent_logical_message_id \
                     WHERE child.user_id = $1 AND child.account_id = $2 AND child.conversation_id IS DISTINCT FROM parent.conversation_id LIMIT 1",
                    params![user_id, &h.account_id],
                )
                .await?;
            if cross.is_some() {
                return Err(Error::internal("delayed parent reconcile left a cross-conversation parent edge"));
            }
        }
    }
    if let Some(p) = parent.as_ref().filter(|p| p.ambiguous) {
        let cmid = p.canonical_message_id.clone().unwrap_or_default();
        tx.exec(
            "INSERT INTO conversation_evidence (user_id, account_id, conversation_id, logical_message_id, evidence_type, evidence_value_hash, weight, details) \
             VALUES ($1,$2,$3,$4,'ambiguous-parent',$5,0,$6::jsonb) ON CONFLICT DO NOTHING",
            params![
                user_id,
                &h.account_id,
                conversation_id,
                logical.id,
                sha256_hex(&cmid),
                js_json(&json!({ "canonical_message_id": cmid, "relation_type": p.relation_type }))
            ],
        )
        .await?;
    }
    // Routine evidence (reason, parent, provider) is already on the copy as threading_reason;
    // only the cases that need explaining later are recorded.
    let mut evidence: Vec<(Option<String>, Option<f64>, Value)> = Vec::new();
    if collision {
        evidence.push((
            Some("message-id-collision".into()),
            Some(0.0),
            json!({ "canonicalMessageId": h.canonical_message_id, "collisionKey": h.collision_key }),
        ));
    }
    for (ty, weight, details) in evidence {
        let d = js_json(&details);
        tx.exec(
            "INSERT INTO conversation_evidence (user_id, account_id, conversation_id, logical_message_id, evidence_type, evidence_value_hash, weight, details) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8::jsonb) ON CONFLICT DO NOTHING",
            params![user_id, &h.account_id, conversation_id, logical.id, ty, sha256_hex(&d), weight, d],
        )
        .await?;
    }
    tx.exec(
        "UPDATE conversations c SET first_message_at = (SELECT MIN(message_date) FROM logical_messages WHERE conversation_id = c.id), \
           last_message_at = (SELECT MAX(message_date) FROM logical_messages WHERE conversation_id = c.id), \
           logical_message_count = (SELECT COUNT(*) FROM logical_messages WHERE conversation_id = c.id), \
           copy_count = (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND is_deleted = false), \
           unread_count = (SELECT COUNT(*) FROM logical_messages lm WHERE lm.conversation_id = c.id AND EXISTS (SELECT 1 FROM messages m WHERE m.logical_message_id = lm.id AND m.conversation_id = c.id AND m.is_deleted = false AND m.is_read = false)), \
           updated_at = NOW() WHERE c.id = $1",
        params![conversation_id],
    )
    .await?;
    Ok(UpsertResult { logical_message_id: logical.id, conversation_id: Some(conversation_id) })
}

// ── ingest glue ─────────────────────────────────────────────────────────────────────────────

/// `recordConversationIngestFailure`.
pub async fn record_ingest_failure(
    db: &Db,
    user_id: &str,
    account_id: Option<&str>,
    row_id: Option<Uuid>,
    operation: &str,
    error: &Error,
    diagnostics: Value,
) -> Result<()> {
    db.exec(
        "INSERT INTO conversation_ingest_failures (user_id, account_id, message_row_id, operation, error_code, error_message, diagnostics) \
         VALUES ($1,$2,$3,$4,$5,$6,$7::jsonb)",
        params![user_id, account_id, row_id, operation, error.pg_code().map(str::to_owned), error.to_string(), js_json(&diagnostics)],
    )
    .await?;
    Ok(())
}

/// What threading reads from a message. It comes from the fetched headers (sync, append) or the
/// header cache (rebuild, retry) — the database keeps no subject, address or body text.
#[derive(Debug, Clone, Default)]
pub struct CopyFacts {
    pub message_id: Option<String>,
    pub subject: String,
    pub from_email: String,
    pub to: Value,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub delivery_addresses: Value,
    /// The raw header block; only its fingerprint is kept (to tell apart Message-ID-less copies).
    pub raw_headers: Option<String>,
    /// Plain text, when it was fetched anyway (needed only by "smart" series detection).
    pub body_text: Option<String>,
}

impl CopyFacts {
    pub fn from_meta(m: &crate::engine::cache::Meta) -> Self {
        CopyFacts {
            message_id: m.message_id.clone(),
            subject: m.subject.clone(),
            from_email: m.from_email.clone(),
            to: json!(m.to),
            in_reply_to: m.in_reply_to.clone(),
            references: m.references.clone(),
            delivery_addresses: json!(m.delivery_addresses),
            raw_headers: None,
            body_text: None,
        }
    }

    pub fn from_parsed(p: &crate::engine::parse::Parsed) -> Self {
        let mut f = Self::from_meta(&crate::engine::cache::Meta::from_parsed(p));
        f.raw_headers = Some(p.headers.to_raw_string()).filter(|h| !h.is_empty());
        f
    }

    /// Lay the facts over a `messages` row under the column names the threading code reads.
    fn overlay(&self, row: &mut Map<String, Value>) {
        if self.message_id.is_some() && row.get("message_id").is_none_or(Value::is_null) {
            row.insert("message_id".into(), json!(self.message_id));
        }
        row.insert("subject".into(), json!(self.subject));
        row.insert("from_email".into(), json!(self.from_email));
        row.insert("to_addresses".into(), self.to.clone());
        row.insert("in_reply_to".into(), json!(self.in_reply_to));
        row.insert("thread_references".into(), json!(self.references));
        row.insert("delivery_addresses".into(), self.delivery_addresses.clone());
        row.insert("conversation_raw_headers".into(), json!(self.raw_headers));
        row.insert("body_text".into(), json!(self.body_text));
    }
}

/// Fetch-side inputs for `persistConversationCopyForRow`.
#[derive(Default)]
pub struct RawCopy<'a> {
    pub headers: Option<&'a Headers>,
    pub email_id: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub message_id: Option<&'a str>,
}

/// `persistConversationCopyForRow`: derive the provider envelope and identities for a stored
/// row and attach the copy to its conversation. Failures are recorded for the retry worker
/// instead of failing the ingest.
pub async fn persist_copy_for_row(
    db: &Db,
    row_id: Uuid,
    account_id: &str,
    user_id: &str,
    host: &str,
    raw: &RawCopy<'_>,
    facts: &CopyFacts,
) {
    let run = async {
        let Some(row) = db
            .json_opt(
                "SELECT m.provider_message_id, m.provider_thread_id FROM messages m WHERE m.id = $1 AND m.account_id = $2",
                params![row_id, account_id],
            )
            .await?
        else {
            return Ok(());
        };
        let s = |k: &str| row.get(k).and_then(Value::as_str);
        let provider = provider_metadata(&ProviderInput {
            email_id: raw.email_id,
            thread_id: raw.thread_id,
            stored_message_id: s("provider_message_id"),
            stored_thread_id: s("provider_thread_id"),
            headers: raw.headers,
            account_id,
            host,
        });
        let identities = resolve_own_identities(db, account_id, Some(&facts.delivery_addresses), raw.headers).await?;
        upsert_copy(db, row_id, facts, &identities, Some(&provider), user_id).await.map(|_| ())
    };
    if let Err(e) = run.await {
        tracing::error!("Conversation persistence error: {e}");
        let diag = json!({ "rawMessageId": raw.message_id });
        if let Err(re) =
            record_ingest_failure(db, user_id, Some(account_id), Some(row_id), "imap-ingest", &e, diag).await
        {
            tracing::error!("Conversation failure recording error: {re}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otp_fingerprints() {
        assert_eq!(body_template_fingerprint("Your code is 123456"), body_template_fingerprint("Your code is 654321"));
        assert_ne!(
            body_template_fingerprint("Order 123456 shipped"),
            body_template_fingerprint("Order 654321 shipped")
        );
    }

    #[test]
    fn provider_identity() {
        let p = provider_metadata(&ProviderInput {
            email_id: Some("123"),
            thread_id: Some("456"),
            stored_message_id: None,
            stored_thread_id: None,
            headers: None,
            account_id: "acc",
            host: "imap.gmail.com",
        });
        assert!(p.is_strong);
        assert_eq!(p.namespace.as_deref(), Some("gmail:acc:imap.gmail.com"));
        assert_eq!(p.source.as_deref(), Some("provider-thread-id"));
        let generic = provider_metadata(&ProviderInput {
            email_id: None,
            thread_id: Some("T1"),
            stored_message_id: None,
            stored_thread_id: None,
            headers: None,
            account_id: "acc",
            host: "mail.example.com",
        });
        assert!(!generic.is_strong);
        assert_eq!(normalize_address("Bob <BOB@x.test>").as_deref(), Some("bob@x.test"));
        assert_eq!(normalize_address("not an address"), None);
        assert_eq!(references_anchor(Some("<a@x> <b@x>"), Some("<c@x>")).as_deref(), Some("<a@x>"));
        assert_eq!(references_anchor(None, Some("<c@x>")).as_deref(), Some("<c@x>"));
    }
}
