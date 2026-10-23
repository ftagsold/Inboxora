//! Conversation Engine v2 endpoints under `/api/mail`: list/detail/body, copy-scoped actions,
//! manual overrides, diagnostics and rebuild jobs (ports of `routes/conversations.ts`,
//! `routes/conversationOverrides.ts` and `routes/conversationRebuild.ts`).

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Error, Result, params};
use inboxora_mail::sanitize::{block_remote_images, has_remote_images, sanitize_email, should_block_remote_images};
use inboxora_mail::util::normalize_message_id;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::is_uuid;
use crate::http::{Auth, Body, Qs};
use crate::routes::util::{err, json as respond, ok};
use crate::services::conversation::resolve_alias;
use crate::services::conversation_ops::{
    ActionRequest, BulkItem, OpError, OverrideRequest, apply_action, apply_bulk_action, apply_override,
    consume_rebuild_rate_limit, get_rebuild_job, list_overrides, record_rebuild_audit, start_rebuild_job,
};
use crate::state::AppState;

/// `router.param(name, uuidParam(name))`, checked in path order.
fn guard(params: &[(&str, &str)]) -> Option<Response> {
    params.iter().find(|(_, v)| !is_uuid(v)).map(|(n, _)| err(400, &format!("Invalid {n}")))
}

/// `Math.min(Math.max(Number(value) || 50, 1), 100)`.
fn parse_limit(v: Option<&str>) -> i64 {
    let n = match v.map(str::trim) {
        None => 50.0,
        Some("") => 0.0,
        Some(s) => s.parse::<f64>().unwrap_or(f64::NAN),
    };
    let n = if n == 0.0 || n.is_nan() { 50.0 } else { n };
    n.clamp(1.0, 100.0).trunc() as i64
}

fn decode_cursor(v: &str) -> Option<(String, String)> {
    let bytes = URL_SAFE_NO_PAD.decode(v.trim_end_matches('=')).ok()?;
    let parsed: Value = serde_json::from_slice(&bytes).ok()?;
    let s = |k: &str| match parsed.get(k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) if n.as_f64() != Some(0.0) => Some(n.to_string()),
        Value::Bool(true) => Some("true".into()),
        _ => None,
    };
    Some((s("date")?, s("id")?))
}

fn encode_cursor(row: &Value) -> String {
    URL_SAFE_NO_PAD.encode(json!({ "date": row.get("sort_date"), "id": row.get("conversation_id") }).to_string())
}

fn uid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_default()
}

/// The owning account of a conversation, then its canonical (alias-resolved) id.
async fn owned(s: &AppState, user: &str, id: &str) -> Result<Option<(String, Uuid)>> {
    let Some(row) =
        s.db.opt("SELECT account_id FROM conversations WHERE id = $1 AND user_id = $2", params![id, user]).await?
    else {
        return Ok(None);
    };
    let account = row.get::<_, Uuid>("account_id").to_string();
    let canonical = resolve_alias(&s.db, user, Some(&account), uid(id)).await?;
    Ok(Some((account, canonical)))
}

pub async fn list(State(s): State<AppState>, a: Auth, q: Qs) -> Result<Response> {
    let user = a.user_id.as_str();
    let account = q.str("accountId").filter(|v| !v.is_empty());
    let folder = if q.str("searchAllFolders") == Some("1") { None } else { q.str("folder").filter(|v| !v.is_empty()) };
    let raw_cursor = q.str("cursor").filter(|v| !v.is_empty());
    let cursor = raw_cursor.and_then(decode_cursor);
    if raw_cursor.is_some() && cursor.is_none() {
        return Ok(err(400, "Invalid conversation cursor"));
    }
    let mut values: Vec<Val> = vec![user.into()];
    if let Some(acc) = account {
        values.push(acc.into());
    }
    let mut entry = vec![
        "m_entry.conversation_id = c.id".to_owned(),
        "m_entry.account_id = c.account_id".into(),
        "m_entry.is_deleted = false".into(),
    ];
    if account.is_some() {
        entry.push("m_entry.account_id = $2".into());
    }
    if q.str("unifiedInbox") == Some("1") && account.is_none() {
        entry.push("EXISTS (SELECT 1 FROM email_accounts scoped_account WHERE scoped_account.id = m_entry.account_id AND scoped_account.user_id = $1 AND scoped_account.include_in_unified_inbox = true)".into());
    }
    let mut folder_param = None;
    if let Some(f) = folder {
        values.push(f.into());
        folder_param = Some(values.len());
        entry.push(format!("m_entry.folder = ${}", values.len()));
    }
    if matches!(q.str("unreadOnly"), Some("1" | "true")) {
        entry.push("m_entry.is_read = false".into());
    }
    if let Some(c) = q.str("category").filter(|c| !c.is_empty() && *c != "all") {
        values.push(c.into());
        entry.push(format!("COALESCE(m_entry.category, 'primary') = ${}", values.len()));
    }
    if let Some(term) = q.str("search").map(str::trim).filter(|t| !t.is_empty()) {
        values.push(format!("%{term}%").into());
        let n = values.len();
        entry.push(format!(
            "(m_entry.subject ILIKE ${n} OR m_entry.snippet ILIKE ${n} OR m_entry.from_email ILIKE ${n})"
        ));
    }
    let mut cursor_filter = String::new();
    if let Some((date, id)) = cursor {
        values.push(date.into());
        values.push(id.into());
        let n = values.len();
        cursor_filter =
            format!("AND (COALESCE(c.last_message_at, c.created_at), c.id) < (${}::timestamptz, ${n}::uuid)", n - 1);
    }
    let limit = parse_limit(q.str("limit"));
    values.push(limit.into());
    let limit_param = values.len();
    let preferred = match folder_param {
        Some(p) => format!(
            "CASE WHEN m_copy.folder = ${p} THEN 0 WHEN m_copy.folder = 'INBOX' THEN 1 WHEN LOWER(m_copy.folder) = 'sent' THEN 2 ELSE 3 END,"
        ),
        None => "CASE WHEN m_copy.folder = 'INBOX' THEN 0 WHEN LOWER(m_copy.folder) = 'sent' THEN 1 ELSE 2 END,".into(),
    };
    let sql = format!(
        "SELECT c.id AS conversation_id, c.account_id, c.kind, c.canonical_subject,
           COUNT(*) OVER()::int AS total_count,
           MIN(m.date) AS first_message_at, MAX(m.date) AS last_message_at,
           COUNT(DISTINCT m.logical_message_id)::int AS logical_message_count,
           COUNT(m.id)::int AS copy_count,
           COUNT(DISTINCT m.logical_message_id) FILTER (WHERE m.is_read = false)::int AS unread_count,
           c.threading_confidence,
           COALESCE(BOOL_OR(m.is_starred), false) AS is_starred,
           COUNT(DISTINCT m.id)::int AS visible_copy_count,
           COALESCE(MAX(m.date), c.created_at) AS sort_date,
           BOOL_OR(COALESCE(m.has_attachments, false)) AS has_attachments,
           top_latest.direction IN ('outgoing', 'self') AS latest_message_is_mine,
           COALESCE(preview.logical_messages, '[]'::jsonb) AS logical_messages,
           top_latest.id AS latest_copy_id,
           top_latest.folder AS folder, top_latest.date AS date,
           top_latest.subject AS subject, top_latest.snippet AS snippet,
           top_latest.from_name AS from_name, top_latest.from_email AS from_email,
           top_latest.is_read AS is_read, top_latest.is_starred AS latest_copy_is_starred,
           top_latest.has_attachments AS latest_copy_has_attachments
      FROM conversations c
      JOIN email_accounts a ON a.id = c.account_id AND a.user_id = $1
      JOIN messages m ON m.conversation_id = c.id AND m.account_id = c.account_id AND m.is_deleted = false
      LEFT JOIN LATERAL (
        SELECT m_copy.id, m_copy.folder, m_copy.date, m_copy.subject, m_copy.snippet,
               m_copy.from_name, m_copy.from_email, m_copy.is_read, m_copy.is_starred,
               m_copy.has_attachments, lm.direction
          FROM logical_messages lm
          JOIN messages m_copy ON m_copy.logical_message_id = lm.id
            AND m_copy.conversation_id = c.id AND m_copy.account_id = c.account_id
            AND m_copy.is_deleted = false
         WHERE lm.conversation_id = c.id AND lm.account_id = c.account_id
         ORDER BY lm.message_date DESC NULLS LAST, m_copy.date DESC NULLS LAST, m_copy.id DESC
         LIMIT 1
      ) top_latest ON true
      LEFT JOIN LATERAL (
        SELECT jsonb_agg(jsonb_build_object(
          'id', logical_preview.id, 'subject', logical_preview.subject,
          'canonicalSubject', logical_preview.canonical_subject,
          'direction', logical_preview.direction, 'messageDate', logical_preview.message_date,
          'snippet', logical_preview.snippet, 'fromName', logical_preview.from_name,
          'fromEmail', logical_preview.from_email, 'unread', logical_preview.unread,
          'accountId', logical_preview.account_id,
          'hasAttachments', logical_preview.has_attachments,
          'latestCopyId', logical_preview.copy_id, 'folder', logical_preview.folder,
          'isLatest', logical_preview.id = top_latest_logical.id
        ) ORDER BY logical_preview.message_date ASC NULLS LAST, logical_preview.id) AS logical_messages
        FROM (
          SELECT lm.id, lm.subject, lm.canonical_subject, lm.direction, lm.message_date,
                 preferred_copy.id AS copy_id, preferred_copy.folder, preferred_copy.snippet,
                 preferred_copy.from_name, preferred_copy.from_email, preferred_copy.account_id,
                 COALESCE(preferred_copy.has_attachments, false) AS has_attachments,
                 COALESCE(NOT preferred_copy.is_read, false) AS unread
            FROM logical_messages lm
            LEFT JOIN LATERAL (
              SELECT m_copy.id, m_copy.folder, m_copy.snippet, m_copy.from_name,
                     m_copy.from_email, m_copy.account_id, m_copy.has_attachments, m_copy.is_read
                FROM messages m_copy
               WHERE m_copy.logical_message_id = lm.id
                 AND m_copy.conversation_id = c.id
                 AND m_copy.account_id = c.account_id
                 AND m_copy.is_deleted = false
               ORDER BY {preferred} m_copy.date DESC NULLS LAST, m_copy.id DESC
               LIMIT 1
            ) preferred_copy ON true
           WHERE lm.conversation_id = c.id AND lm.account_id = c.account_id
        ) logical_preview
        LEFT JOIN LATERAL (
          SELECT lm_latest.id
            FROM logical_messages lm_latest
           WHERE lm_latest.conversation_id = c.id AND lm_latest.account_id = c.account_id
           ORDER BY lm_latest.message_date DESC NULLS LAST, lm_latest.id DESC
           LIMIT 1
        ) top_latest_logical ON true
      ) preview ON true
     WHERE c.user_id = $1
       AND ({acct})
       AND EXISTS (SELECT 1 FROM messages m_entry WHERE {entry})
       AND NOT EXISTS (SELECT 1 FROM conversation_aliases ca WHERE ca.user_id = $1 AND ca.account_id = c.account_id AND ca.alias_conversation_id = c.id)
       {cursor_filter}
     GROUP BY c.id, top_latest.id, top_latest.folder, top_latest.date, top_latest.subject,
              top_latest.snippet, top_latest.from_name, top_latest.from_email,
              top_latest.is_read, top_latest.is_starred, top_latest.has_attachments,
              top_latest.direction, preview.logical_messages
     ORDER BY COALESCE(MAX(m.date), c.created_at) DESC, c.id DESC
     LIMIT ${limit_param}",
        acct = if account.is_some() { "c.account_id = $2" } else { "true" },
        entry = entry.join(" AND "),
    );
    let mut rows = s.db.json_rows(&sql, &values).await?;
    for row in &mut rows {
        if let Value::Object(m) = row {
            let latest = m.get("latest_copy_id").cloned().unwrap_or(Value::Null);
            m.insert("latestCopyId".into(), latest);
        }
    }
    let next = (rows.len() as i64 == limit).then(|| rows.last().map(encode_cursor)).flatten();
    let total =
        rows.first().and_then(|r| r.get("total_count")).cloned().filter(|v| v.as_i64() != Some(0)).unwrap_or(json!(0));
    Ok(ok(json!({ "conversations": rows, "nextCursor": next, "total": total })))
}

const LOGICAL_KEYS: [&str; 7] = [
    "logical_id",
    "canonical_message_id",
    "subject",
    "direction",
    "message_date",
    "threading_reason",
    "threading_confidence",
];

pub async fn detail(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = guard(&[("id", &id)]) {
        return Ok(r);
    }
    let Some((account, canonical)) = owned(&s, &a.user_id, &id).await? else {
        return Ok(err(404, "Conversation not found"));
    };
    let rows = s
        .db
        .json_rows(
            "SELECT c.*, lm.id AS logical_id, lm.canonical_message_id, lm.subject,
                    lm.direction, lm.message_date, lm.threading_reason, lm.threading_confidence,
                    COALESCE(jsonb_agg(jsonb_build_object(
                      'id', m.id, 'accountId', m.account_id, 'folder', m.folder,
                      'messageId', m.message_id, 'canonicalMessageId', m.canonical_message_id,
                      'subject', m.subject, 'fromName', m.from_name, 'fromEmail', m.from_email,
                      'to', m.to_addresses, 'cc', m.cc_addresses, 'date', m.date,
                      'snippet', m.snippet, 'replyTo', m.reply_to, 'inReplyTo', m.in_reply_to, 'references', m.thread_references, 'attachments', m.attachments,
                      'listUnsubscribe', m.list_unsubscribe, 'listUnsubscribePost', m.list_unsubscribe_post, 'unsubscribedAt', m.unsubscribed_at,
                      'isRead', m.is_read, 'isStarred', m.is_starred,
                      'providerMessageId', m.provider_message_id, 'providerThreadId', m.provider_thread_id,
                      'providerNamespace', m.provider_namespace, 'threadKey', m.thread_key,
                      'deliveryAddresses', m.delivery_addresses
                    ) ORDER BY m.date ASC NULLS LAST, m.id) FILTER (WHERE m.id IS NOT NULL), '[]'::jsonb) AS copies
               FROM conversations c
               LEFT JOIN logical_messages lm ON lm.conversation_id = c.id AND lm.account_id = c.account_id
               LEFT JOIN messages m ON m.logical_message_id = lm.id AND m.conversation_id = c.id AND m.is_deleted = false
              WHERE c.id = $1 AND c.user_id = $2 AND c.account_id = $3
              GROUP BY c.id, lm.id
              ORDER BY lm.message_date ASC NULLS LAST, lm.id",
            params![canonical, &a.user_id, &account],
        )
        .await?;
    let Some(Value::Object(first)) = rows.first() else {
        return Ok(err(404, "Conversation not found"));
    };
    let logical: Vec<Value> = rows
        .iter()
        .filter(|r| r.get("logical_id").is_some_and(|v| !v.is_null()))
        .map(|r| {
            let g = |k: &str| r.get(k).cloned().unwrap_or(Value::Null);
            let copies = r.get("copies").filter(|v| !v.is_null()).cloned().unwrap_or(json!([]));
            json!({ "id": g("logical_id"), "canonicalMessageId": g("canonical_message_id"), "subject": g("subject"),
                    "direction": g("direction"), "messageDate": g("message_date"), "threadingReason": g("threading_reason"),
                    "threadingConfidence": g("threading_confidence"), "copies": copies })
        })
        .collect();
    let mut summary: Map<String, Value> = first
        .iter()
        .filter(|(k, _)| !LOGICAL_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    summary.insert("conversation_id".into(), json!(canonical.to_string()));
    summary.insert("requested_conversation_id".into(), json!(id));
    Ok(ok(json!({ "summary": summary, "logicalMessages": logical })))
}

pub async fn logical_body(
    State(s): State<AppState>,
    a: Auth,
    Path((conversation, logical)): Path<(String, String)>,
    q: Qs,
    headers: HeaderMap,
) -> Result<Response> {
    if let Some(r) = guard(&[("conversationId", &conversation), ("logicalMessageId", &logical)]) {
        return Ok(r);
    }
    let Some((account, canonical)) = owned(&s, &a.user_id, &conversation).await? else {
        return Ok(err(404, "Logical message body not found"));
    };
    let copy: Option<String> = q.str("copyId").filter(|v| !v.is_empty()).map(str::to_owned);
    let Some(mut row) = s
        .db
        .json_opt(
            "SELECT lm.id, m.body_text, m.body_html, m.attachments, m.id AS physical_copy_id, m.account_id, m.folder, u.preferences, m.from_email
               FROM logical_messages lm
               JOIN messages m ON m.logical_message_id = lm.id AND m.conversation_id = $3 AND m.is_deleted = false
               JOIN email_accounts a ON a.id = m.account_id AND a.user_id = $2
               JOIN users u ON u.id = a.user_id
              WHERE lm.id = $1 AND lm.user_id = $2 AND lm.account_id = $5 AND lm.conversation_id = $3
              AND ($4::uuid IS NULL OR m.id = $4::uuid)
              ORDER BY (m.body_html IS NOT NULL OR m.body_text IS NOT NULL) DESC, m.is_read ASC, m.date DESC NULLS LAST, m.id DESC
              LIMIT 1",
            params![&logical, &a.user_id, canonical, copy, &account],
        )
        .await?
    else {
        return Ok(err(404, "Logical message body not found"));
    };
    let requested = q.str("remoteImages") == Some("1");
    let opt_in = headers.get("x-mailflow-image-opt-in").and_then(|v| v.to_str().ok()) == Some("1");
    let prefs = row.get("preferences").cloned().unwrap_or(Value::Null);
    let blocks = should_block_remote_images(&prefs, row.get("from_email").and_then(Value::as_str));
    let remote = requested && opt_in && !blocks;
    let raw = row.get("body_html").and_then(Value::as_str).filter(|h| !h.is_empty()).map(str::to_owned);
    let html = raw.as_deref().map(|h| {
        if !remote && has_remote_images(h) { block_remote_images(&sanitize_email(h)) } else { sanitize_email(h) }
    });
    let blocked = matches!((&raw, &html), (Some(r), Some(h)) if r != h);
    if let Some(h) = html {
        row.insert("body_html".into(), json!(h));
    }
    row.insert("remoteImages".into(), json!(remote));
    row.insert("hasBlockedRemoteImages".into(), json!(blocked));
    Ok(ok(Value::Object(row)))
}

pub async fn message_conversation(
    State(s): State<AppState>,
    a: Auth,
    Path(r): Path<String>,
    q: Qs,
) -> Result<Response> {
    let by_copy = is_uuid(&r);
    let canonical = if by_copy { None } else { normalize_message_id(&r) };
    let raw_account = q.str("accountId").map(str::trim).unwrap_or("");
    if !by_copy && canonical.is_none() {
        return Ok(err(400, "Invalid message reference"));
    }
    if !raw_account.is_empty() && !is_uuid(raw_account) {
        return Ok(err(400, "Invalid accountId"));
    }
    let account = (!raw_account.is_empty()).then(|| raw_account.to_owned());
    let rows = if by_copy {
        s.db.json_rows(
            "SELECT m.id, m.account_id, m.folder, m.date, m.conversation_id, m.logical_message_id FROM messages m \
             JOIN email_accounts a ON a.id = m.account_id WHERE m.id = $1 AND a.user_id = $2 AND m.is_deleted = false",
            params![&r, &a.user_id],
        )
        .await?
    } else {
        s.db.json_rows(
            "SELECT m.id, m.account_id, m.folder, m.date, m.conversation_id, m.logical_message_id FROM messages m \
             JOIN email_accounts a ON a.id = m.account_id WHERE m.canonical_message_id = $1 AND a.user_id = $2 \
             AND ($3::uuid IS NULL OR m.account_id = $3) AND m.is_deleted = false \
             ORDER BY (m.folder = 'INBOX') DESC, m.date DESC NULLS LAST, m.id DESC",
            params![canonical, &a.user_id, account],
        )
        .await?
    };
    let Some(first) = rows.first().cloned() else {
        return Ok(err(404, "Conversation not found"));
    };
    let present = |v: &Value, k: &str| v.get(k).is_some_and(|x| !x.is_null());
    if by_copy {
        if !present(&first, "conversation_id") {
            return Ok(err(404, "Conversation not found"));
        }
        return Ok(ok(first));
    }
    let key = |v: &Value| {
        ["account_id", "logical_message_id", "conversation_id"]
            .map(|k| v.get(k).and_then(Value::as_str).unwrap_or("").to_owned())
            .join(":")
    };
    let first_key = key(&first);
    let ambiguous = rows.iter().any(|r| key(r) != first_key)
        || !present(&first, "logical_message_id")
        || !present(&first, "conversation_id");
    if ambiguous {
        return Ok(respond(
            409,
            json!({ "error": "Conversation reference is ambiguous", "code": "CONVERSATION_REFERENCE_AMBIGUOUS" }),
        ));
    }
    Ok(ok(first))
}

// ── copy-aware actions ──────────────────────────────────────────────────────────────────────

/// `req.body?.x || null` for string selectors.
fn opt_str(b: &Body, k: &str) -> Option<String> {
    match b.get(k) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) if n.as_f64() != Some(0.0) => Some(n.to_string()),
        _ => None,
    }
}

fn base_request(b: &Body, action: &str) -> ActionRequest {
    ActionRequest {
        scope: opt_str(b, "scope").unwrap_or_else(|| "THIS_COPY".into()),
        copy_id: opt_str(b, "copyId"),
        logical_message_id: opt_str(b, "logicalMessageId"),
        action: action.into(),
        is_read: b.truthy("isRead"),
        is_starred: b.truthy("isStarred"),
        target_folder: opt_str(b, "targetFolder"),
        ..Default::default()
    }
}

fn action_error(e: OpError) -> Response {
    err(e.status.unwrap_or(400), &e.message)
}

async fn run_action(s: AppState, a: Auth, id: String, b: Body, action: &str) -> Result<Response> {
    if let Some(r) = guard(&[("id", &id)]) {
        return Ok(r);
    }
    let req = ActionRequest { conversation_id: id, ..base_request(&b, action) };
    Ok(match apply_action(&s.mail, &a.user_id, req).await {
        Ok(v) => ok(v),
        Err(e) => action_error(e),
    })
}

pub async fn archive(State(s): State<AppState>, a: Auth, Path(id): Path<String>, b: Body) -> Result<Response> {
    run_action(s, a, id, b, "archive").await
}
pub async fn delete(State(s): State<AppState>, a: Auth, Path(id): Path<String>, b: Body) -> Result<Response> {
    run_action(s, a, id, b, "delete").await
}
pub async fn move_to(State(s): State<AppState>, a: Auth, Path(id): Path<String>, b: Body) -> Result<Response> {
    run_action(s, a, id, b, "move").await
}
pub async fn read(State(s): State<AppState>, a: Auth, Path(id): Path<String>, b: Body) -> Result<Response> {
    run_action(s, a, id, b, "read").await
}
pub async fn star(State(s): State<AppState>, a: Auth, Path(id): Path<String>, b: Body) -> Result<Response> {
    run_action(s, a, id, b, "star").await
}

/// `normalizedItems` of `applyBulkConversationAction`.
fn bulk_items(b: &Body) -> Vec<BulkItem> {
    let s = |v: Option<&Value>| match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) if n.as_f64() != Some(0.0) => Some(n.to_string()),
        Some(Value::Bool(true)) => Some("true".into()),
        _ => None,
    };
    if let Some(Value::Array(items)) = b.get("items") {
        return items
            .iter()
            .filter_map(|i| {
                Some(BulkItem {
                    conversation_id: s(i.get("conversationId"))?,
                    copy_id: s(i.get("copyId")),
                    logical_message_id: s(i.get("logicalMessageId")),
                })
            })
            .collect();
    }
    let ids: Vec<Value> = match b.get("conversationIds") {
        Some(Value::Array(v)) => v.clone(),
        Some(v) => vec![v.clone()],
        None => vec![],
    };
    let mut out: Vec<BulkItem> = Vec::new();
    for id in ids.iter().filter_map(|v| s(Some(v))) {
        if !out.iter().any(|i| i.conversation_id == id) {
            out.push(BulkItem { conversation_id: id, ..Default::default() });
        }
    }
    out
}

async fn run_bulk(s: AppState, a: Auth, b: Body, action: &str) -> Result<Response> {
    let mut base = base_request(&b, action);
    // The bulk routes pass no top-level selectors and read only their own action's field.
    base.copy_id = None;
    base.logical_message_id = None;
    if action != "read" {
        base.is_read = false;
    }
    if action != "move" {
        base.target_folder = None;
    }
    base.is_starred = false;
    Ok(match apply_bulk_action(&s.mail, &a.user_id, bulk_items(&b), base).await {
        Ok(v) => ok(v),
        Err(e) => action_error(e),
    })
}

pub async fn bulk_archive(State(s): State<AppState>, a: Auth, b: Body) -> Result<Response> {
    run_bulk(s, a, b, "archive").await
}
pub async fn bulk_delete(State(s): State<AppState>, a: Auth, b: Body) -> Result<Response> {
    run_bulk(s, a, b, "delete").await
}
pub async fn bulk_read(State(s): State<AppState>, a: Auth, b: Body) -> Result<Response> {
    run_bulk(s, a, b, "read").await
}
pub async fn bulk_move(State(s): State<AppState>, a: Auth, b: Body) -> Result<Response> {
    run_bulk(s, a, b, "move").await
}

// ── manual operations ───────────────────────────────────────────────────────────────────────

fn manual_error(label: &str, e: OpError) -> Response {
    respond(e.status.unwrap_or(400), json!({ "error": label, "detail": e.message }))
}

fn with(head: Value, result: Value) -> Value {
    let mut m = head.as_object().cloned().unwrap_or_default();
    if let Value::Object(r) = result {
        m.extend(r);
    }
    Value::Object(m)
}

pub async fn merge(State(s): State<AppState>, a: Auth, Path(id): Path<String>, b: Body) -> Result<Response> {
    if let Some(r) = guard(&[("id", &id)]) {
        return Ok(r);
    }
    let Some(target) = opt_str(&b, "targetConversationId") else {
        return Ok(err(400, "targetConversationId required"));
    };
    let req = OverrideRequest {
        conversation_id: id,
        override_type: "manual-merge".into(),
        target_id: Some(target),
        ..Default::default()
    };
    Ok(match apply_override(&s.db, &a.user_id, req).await {
        Ok(v) => ok(with(json!({ "merged": true }), v)),
        Err(e) => manual_error("Merge failed", e),
    })
}

pub async fn split(
    State(s): State<AppState>,
    a: Auth,
    Path((id, logical)): Path<(String, String)>,
    b: Body,
) -> Result<Response> {
    if let Some(r) = guard(&[("id", &id), ("logicalMessageId", &logical)]) {
        return Ok(r);
    }
    let scope = if b.truthy("includeReplies") { "message-with-descendants" } else { "message-only" };
    let req = OverrideRequest {
        conversation_id: id,
        logical_message_id: Some(logical),
        scope: scope.into(),
        override_type: "manual-split".into(),
        ..Default::default()
    };
    Ok(match apply_override(&s.db, &a.user_id, req).await {
        Ok(v) => {
            let target = v.get("targetId").cloned().unwrap_or(Value::Null);
            respond(201, with(json!({ "split": true, "newConversationId": target }), v))
        }
        Err(e) => manual_error("Split failed", e),
    })
}

pub async fn move_logical(
    State(s): State<AppState>,
    a: Auth,
    Path((id, logical)): Path<(String, String)>,
    b: Body,
) -> Result<Response> {
    if let Some(r) = guard(&[("id", &id), ("logicalMessageId", &logical)]) {
        return Ok(r);
    }
    let Some(target) = opt_str(&b, "targetConversationId") else {
        return Ok(err(400, "targetConversationId required"));
    };
    let req = OverrideRequest {
        conversation_id: id,
        logical_message_id: Some(logical),
        override_type: "manual-move".into(),
        target_id: Some(target),
        ..Default::default()
    };
    Ok(match apply_override(&s.db, &a.user_id, req).await {
        Ok(v) => ok(with(json!({ "moved": true }), v)),
        Err(e) => manual_error("Move failed", e),
    })
}

async fn set_lock(s: AppState, a: Auth, id: String, lock: bool) -> Result<Response> {
    if let Some(r) = guard(&[("id", &id)]) {
        return Ok(r);
    }
    let ty = if lock { "lock-conversation" } else { "unlock-conversation" };
    let req = OverrideRequest { conversation_id: id, override_type: ty.into(), ..Default::default() };
    Ok(match apply_override(&s.db, &a.user_id, req).await {
        Ok(v) => ok(with(json!({ "locked": lock }), v)),
        Err(e) => manual_error(if lock { "Lock failed" } else { "Unlock failed" }, e),
    })
}

pub async fn lock(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    set_lock(s, a, id, true).await
}
pub async fn unlock(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    set_lock(s, a, id, false).await
}

/// Force include/exclude: Node has no handler here, so failures surface as 500.
async fn force(s: AppState, a: Auth, id: String, logical: String, b: Body, include: bool) -> Result<Response> {
    if let Some(r) = guard(&[("id", &id), ("logicalMessageId", &logical)]) {
        return Ok(r);
    }
    let target = if include {
        opt_str(&b, "targetConversationId").or_else(|| opt_str(&b, "targetId")).or_else(|| Some(id.clone()))
    } else {
        None
    };
    let req = OverrideRequest {
        conversation_id: id,
        logical_message_id: Some(logical),
        scope: "message-only".into(),
        override_type: if include { "force-include" } else { "force-exclude" }.into(),
        target_conversation_id: target,
        ..Default::default()
    };
    let v = apply_override(&s.db, &a.user_id, req).await.map_err(Error::from)?;
    Ok(respond(201, v))
}

pub async fn force_include(
    State(s): State<AppState>,
    a: Auth,
    Path((id, logical)): Path<(String, String)>,
    b: Body,
) -> Result<Response> {
    force(s, a, id, logical, b, true).await
}
pub async fn force_exclude(
    State(s): State<AppState>,
    a: Auth,
    Path((id, logical)): Path<(String, String)>,
    b: Body,
) -> Result<Response> {
    force(s, a, id, logical, b, false).await
}

pub async fn diagnostics(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = guard(&[("id", &id)]) {
        return Ok(r);
    }
    let Some((account, canonical)) = owned(&s, &a.user_id, &id).await? else {
        return Ok(err(404, "Conversation not found"));
    };
    let Some(conversation) = s
        .db
        .json_opt(
            "SELECT id, kind, canonical_subject, threading_confidence, manually_locked, logical_message_count, unread_count, copy_count \
             FROM conversations WHERE id = $1 AND user_id = $2 AND account_id = $3",
            params![canonical, &a.user_id, &account],
        )
        .await?
    else {
        return Ok(err(404, "Conversation not found"));
    };
    let mut logical =
        s.db.json_rows(
            "SELECT lm.id, lm.canonical_message_id, lm.subject, lm.direction, lm.message_date,
                    lm.threading_reason, lm.threading_confidence, lm.parent_logical_message_id,
                    COUNT(m.id)::int AS copy_count
               FROM logical_messages lm
               LEFT JOIN messages m ON m.logical_message_id = lm.id AND m.is_deleted = false
              WHERE lm.conversation_id = $1 AND lm.user_id = $2
              GROUP BY lm.id
              ORDER BY lm.message_date ASC NULLS LAST, lm.id",
            params![canonical, &a.user_id],
        )
        .await?;
    for row in &mut logical {
        if let Value::Object(m) = row {
            m.insert("providerMessageId".into(), Value::Null);
        }
    }
    let overrides =
        s.db.json_rows(
            "SELECT * FROM conversation_overrides WHERE conversation_id = $1 AND user_id = $2 ORDER BY created_at DESC",
            params![canonical, &a.user_id],
        )
        .await?;
    Ok(ok(json!({ "conversation": conversation, "logicalMessages": logical, "overrides": overrides })))
}

// ── generic overrides ───────────────────────────────────────────────────────────────────────

/// `readConversationOverrideBody`.
fn override_body(v: &Value) -> std::result::Result<OverrideRequest, String> {
    let empty = Map::new();
    let m = match v {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => return Err("Invalid request body".into()),
    };
    let opt = |k: &str| match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("Invalid {k}")),
    };
    let logical_message_id = opt("logicalMessageId")?;
    let scope = opt("scope")?;
    let Some(Value::String(override_type)) = m.get("overrideType") else {
        return Err("Unsupported conversation override type".into());
    };
    Ok(OverrideRequest {
        conversation_id: String::new(),
        logical_message_id,
        scope: scope.unwrap_or_else(|| "message-only".into()),
        override_type: override_type.clone(),
        target_id: opt("targetId")?,
        target_conversation_id: opt("targetConversationId")?,
        reason: opt("reason")?,
    })
}

pub async fn create_override(State(s): State<AppState>, a: Auth, Path(id): Path<String>, b: Body) -> Result<Response> {
    if let Some(r) = guard(&[("id", &id)]) {
        return Ok(r);
    }
    let req = match override_body(&b.0) {
        Ok(r) => OverrideRequest { conversation_id: id, ..r },
        Err(m) => return Ok(err(400, &m)),
    };
    Ok(match apply_override(&s.db, &a.user_id, req).await {
        Ok(v) => respond(201, v),
        Err(e) if e.status == Some(404) => err(404, &e.message),
        Err(e) => err(400, &e.message),
    })
}

pub async fn get_overrides(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = guard(&[("id", &id)]) {
        return Ok(r);
    }
    Ok(ok(json!({ "overrides": list_overrides(&s.db, &a.user_id, &id).await? })))
}

// ── rebuild ─────────────────────────────────────────────────────────────────────────────────

pub async fn rebuild(State(s): State<AppState>, a: Auth, b: Body) -> Result<Response> {
    let user = a.user_id.as_str();
    if !consume_rebuild_rate_limit(user) {
        return Err(Error::status(429, "Conversation rebuild rate limit exceeded"));
    }
    let account = opt_str(&b, "accountId");
    if let Some(acc) = &account
        && s.db.opt("SELECT 1 FROM email_accounts WHERE id = $1 AND user_id = $2", params![acc, user]).await?.is_none()
    {
        return Ok(err(404, "Account not found"));
    }
    let limit = match b.get("limit") {
        Some(Value::Number(n)) => n.as_f64().map_or(100, |f| f.trunc() as i64),
        Some(Value::String(s)) => s.trim().parse::<f64>().map_or(100, |f| f.trunc() as i64),
        _ => 100,
    };
    let dry_run = b.get("dryRun") != Some(&Value::Bool(false));
    let force = b.get("force") == Some(&Value::Bool(true));
    let job = start_rebuild_job(&s.db, user, account.clone(), limit, dry_run, force);
    record_rebuild_audit(
        &s.db,
        user,
        &job,
        "requested",
        json!({ "accountId": account, "dryRun": dry_run, "force": force }),
    )
    .await?;
    Ok(respond(202, json!({ "jobId": job, "status": "queued" })))
}

/// Static `/conversations/<name>` paths reached with GET fall through to `:id` in Node.
pub async fn invalid_id() -> Response {
    err(400, "Invalid id")
}

pub async fn rebuild_job(a: Auth, Path(job): Path<String>) -> Result<Response> {
    if let Some(r) = guard(&[("jobId", &job)]) {
        return Ok(r);
    }
    Ok(match get_rebuild_job(&a.user_id, &job) {
        Some(v) => ok(v),
        None => err(404, "Rebuild job not found"),
    })
}
