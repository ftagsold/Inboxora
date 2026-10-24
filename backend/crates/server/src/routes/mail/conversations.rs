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
use inboxora_mail::sanitize::{block_remote_images, has_remote_images, should_block_remote_images};
use inboxora_mail::util::normalize_message_id;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::is_uuid;
use crate::engine::cache::Location;
use crate::engine::search::FolderScope;
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
    if matches!(q.str("starredOnly"), Some("1" | "true")) {
        entry.push("m_entry.is_starred = true".into());
    }
    if let Some(c) = q.str("category").filter(|c| !c.is_empty() && *c != "all") {
        values.push(c.into());
        entry.push(format!("COALESCE(m_entry.category, 'primary') = ${}", values.len()));
    }
    if let Some(term) = q.str("search").map(str::trim).filter(|t| !t.is_empty()) {
        let accounts: Vec<String> = match account {
            Some(acc) => vec![acc.to_owned()],
            None => {
                s.db.rows("SELECT id FROM email_accounts WHERE user_id = $1 AND enabled = true", params![user])
                    .await?
                    .iter()
                    .map(|r| r.get::<_, Uuid>(0).to_string())
                    .collect()
            }
        };
        let scope = match folder {
            Some(f) => FolderScope::Exact(f.to_owned()),
            None => FolderScope::AllButTrash,
        };
        let criteria = format!("TEXT {}", inboxora_imap::quote(term));
        let ids: Vec<String> =
            s.mail.search_ids(&accounts, &scope, &criteria, 1000).await.iter().map(Uuid::to_string).collect();
        values.push(ids.into());
        entry.push(format!("m_entry.id = ANY(${}::uuid[])", values.len()));
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
        "SELECT c.id AS conversation_id, c.account_id, c.kind,
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
           top_latest.folder AS folder, top_latest.date AS date, top_latest.uid AS latest_uid,
           top_latest.is_read AS is_read, top_latest.is_starred AS latest_copy_is_starred,
           top_latest.has_attachments AS latest_copy_has_attachments
      FROM conversations c
      JOIN email_accounts a ON a.id = c.account_id AND a.user_id = $1
      JOIN messages m ON m.conversation_id = c.id AND m.account_id = c.account_id AND m.is_deleted = false
      LEFT JOIN LATERAL (
        SELECT m_copy.id, m_copy.folder, m_copy.uid, m_copy.date, m_copy.is_read, m_copy.is_starred,
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
          'id', logical_preview.id,
          'direction', logical_preview.direction, 'messageDate', logical_preview.message_date,
          'uid', logical_preview.uid, 'unread', logical_preview.unread,
          'accountId', logical_preview.account_id,
          'hasAttachments', logical_preview.has_attachments,
          'latestCopyId', logical_preview.copy_id, 'folder', logical_preview.folder,
          'isLatest', logical_preview.id = top_latest_logical.id
        ) ORDER BY logical_preview.message_date ASC NULLS LAST, logical_preview.id) AS logical_messages
        FROM (
          SELECT lm.id, lm.direction, lm.message_date,
                 preferred_copy.id AS copy_id, preferred_copy.folder, preferred_copy.uid, preferred_copy.account_id,
                 COALESCE(preferred_copy.has_attachments, false) AS has_attachments,
                 COALESCE(NOT preferred_copy.is_read, false) AS unread
            FROM logical_messages lm
            LEFT JOIN LATERAL (
              SELECT m_copy.id, m_copy.folder, m_copy.uid, m_copy.account_id, m_copy.has_attachments, m_copy.is_read
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
     GROUP BY c.id, top_latest.id, top_latest.folder, top_latest.date, top_latest.uid,
              top_latest.is_read, top_latest.is_starred, top_latest.has_attachments,
              top_latest.direction, preview.logical_messages
     ORDER BY COALESCE(MAX(m.date), c.created_at) DESC, c.id DESC
     LIMIT ${limit_param}",
        acct = if account.is_some() { "c.account_id = $2" } else { "true" },
        entry = entry.join(" AND "),
    );
    let mut rows = s.db.json_rows(&sql, &values).await?;
    hydrate_conversation_rows(&s, &mut rows).await;
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
    let mut rows = s
        .db
        .json_rows(
            "SELECT c.*, lm.id AS logical_id, lm.canonical_message_id,
                    lm.direction, lm.message_date, lm.threading_reason, lm.threading_confidence,
                    COALESCE(jsonb_agg(jsonb_build_object(
                      'id', m.id, 'accountId', m.account_id, 'folder', m.folder, 'uid', m.uid,
                      'messageId', m.message_id, 'canonicalMessageId', m.canonical_message_id,
                      'date', m.date, 'unsubscribedAt', m.unsubscribed_at,
                      'isRead', m.is_read, 'isStarred', m.is_starred,
                      'providerMessageId', m.provider_message_id, 'providerThreadId', m.provider_thread_id,
                      'providerNamespace', m.provider_namespace, 'threadKey', m.thread_key
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
    hydrate_detail_copies(&s, &mut rows).await;
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
    let Some(mut row) =
        s.db.json_opt(
            "SELECT lm.id, m.id AS physical_copy_id, m.account_id, m.folder, m.uid, u.preferences
               FROM logical_messages lm
               JOIN messages m ON m.logical_message_id = lm.id AND m.conversation_id = $3 AND m.is_deleted = false
               JOIN email_accounts a ON a.id = m.account_id AND a.user_id = $2
               JOIN users u ON u.id = a.user_id
              WHERE lm.id = $1 AND lm.user_id = $2 AND lm.account_id = $5 AND lm.conversation_id = $3
              AND ($4::uuid IS NULL OR m.id = $4::uuid)
              ORDER BY m.is_read ASC, m.date DESC NULLS LAST, m.id DESC
              LIMIT 1",
            params![&logical, &a.user_id, canonical, copy, &account],
        )
        .await?
    else {
        return Ok(err(404, "Logical message body not found"));
    };
    let loc = Location {
        id: row.get("physical_copy_id").and_then(Value::as_str).and_then(|v| v.parse().ok()).unwrap_or_default(),
        account_id: row.get("account_id").and_then(Value::as_str).unwrap_or_default().to_owned(),
        folder: row.get("folder").and_then(Value::as_str).unwrap_or_default().to_owned(),
        uid: inboxora_core::json::int(row.get("uid")).unwrap_or(0),
    };
    let (meta, body) = tokio::join!(s.mail.message_meta(std::slice::from_ref(&loc)), s.mail.message_body(&loc));
    let body = match body {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("Logical message body fetch error: {e}");
            return Ok(err(502, "Could not load the message from the mail server"));
        }
    };
    let from_email = meta.get(&loc.id).map(|m| m.from_email.clone());
    let requested = q.str("remoteImages") == Some("1");
    let opt_in = headers.get("x-mailflow-image-opt-in").and_then(|v| v.to_str().ok()) == Some("1");
    let prefs = row.get("preferences").cloned().unwrap_or(Value::Null);
    let blocks = should_block_remote_images(&prefs, from_email.as_deref());
    let remote = requested && opt_in && !blocks;
    let html = body
        .html
        .as_deref()
        .map(|h| if !remote && has_remote_images(h) { block_remote_images(h) } else { h.to_owned() });
    let blocked = matches!((&body.html, &html), (Some(r), Some(h)) if r != h);
    row.remove("uid");
    row.insert("from_email".into(), json!(from_email));
    row.insert("body_html".into(), json!(html));
    row.insert("body_text".into(), json!(body.text));
    row.insert("attachments".into(), body.attachments);
    row.insert("remoteImages".into(), json!(remote));
    row.insert("hasBlockedRemoteImages".into(), json!(blocked));
    Ok(ok(Value::Object(row)))
}

/// Display fields for conversation list rows (from the latest copy) and their logical-message
/// previews (from each preferred copy).
async fn hydrate_conversation_rows(s: &AppState, rows: &mut [Value]) {
    let loc = |id: Option<&Value>, account: Option<&Value>, folder: Option<&Value>, uid: Option<&Value>| {
        Some(Location {
            id: id?.as_str()?.parse().ok()?,
            account_id: account?.as_str()?.to_owned(),
            folder: folder?.as_str()?.to_owned(),
            uid: inboxora_core::json::int(uid)?,
        })
    };
    let mut locs = Vec::new();
    for r in rows.iter() {
        locs.extend(loc(r.get("latest_copy_id"), r.get("account_id"), r.get("folder"), r.get("latest_uid")));
        for p in r.get("logical_messages").and_then(Value::as_array).into_iter().flatten() {
            locs.extend(loc(p.get("latestCopyId"), p.get("accountId"), p.get("folder"), p.get("uid")));
        }
    }
    let meta = s.mail.message_meta(&locs).await;
    let meta_of = |id: Option<&Value>| {
        id.and_then(Value::as_str).and_then(|v| v.parse::<Uuid>().ok()).and_then(|id| meta.get(&id))
    };
    for r in rows.iter_mut() {
        let Some(row) = r.as_object_mut() else { continue };
        let latest = meta_of(row.get("latest_copy_id")).cloned().unwrap_or_default();
        row.insert("subject".into(), json!(latest.subject));
        row.insert("snippet".into(), json!(latest.snippet));
        row.insert("from_name".into(), json!(latest.from_name));
        row.insert("from_email".into(), json!(latest.from_email));
        row.remove("latest_uid");
        let mut first_subject: Option<String> = None;
        if let Some(Value::Array(previews)) = row.get_mut("logical_messages") {
            for p in previews.iter_mut() {
                let Some(p) = p.as_object_mut() else { continue };
                let m = meta_of(p.get("latestCopyId")).cloned().unwrap_or_default();
                if first_subject.is_none() {
                    first_subject = Some(m.subject.clone());
                }
                p.insert("canonicalSubject".into(), json!(inboxora_mail::conversation::canonical_subject(&m.subject)));
                p.insert("subject".into(), json!(m.subject));
                p.insert("snippet".into(), json!(m.snippet));
                p.insert("fromName".into(), json!(m.from_name));
                p.insert("fromEmail".into(), json!(m.from_email));
                p.remove("uid");
            }
        }
        let canonical =
            inboxora_mail::conversation::canonical_subject(first_subject.as_deref().unwrap_or(&latest.subject));
        row.insert("canonical_subject".into(), json!(canonical));
    }
}

/// Display fields for the copies of a conversation detail, and each logical message's subject.
async fn hydrate_detail_copies(s: &AppState, rows: &mut [Value]) {
    let mut locs = Vec::new();
    for r in rows.iter() {
        for c in r.get("copies").and_then(Value::as_array).into_iter().flatten() {
            let l = (|| {
                Some(Location {
                    id: c.get("id")?.as_str()?.parse().ok()?,
                    account_id: c.get("accountId")?.as_str()?.to_owned(),
                    folder: c.get("folder")?.as_str()?.to_owned(),
                    uid: inboxora_core::json::int(c.get("uid"))?,
                })
            })();
            locs.extend(l);
        }
    }
    let meta = s.mail.message_meta(&locs).await;
    // Attachments are known once a body has been opened (and cached); no fetch for the list.
    let body_keys: Vec<String> = locs.iter().map(|l| crate::engine::cache::body_key(&l.id)).collect();
    let bodies: Vec<Option<crate::engine::cache::CachedBody>> = s.mail.cache.get_many(&body_keys).await;
    let attachments: std::collections::HashMap<Uuid, Value> =
        locs.iter().zip(bodies).filter_map(|(l, b)| Some((l.id, b?.attachments))).collect();
    for r in rows.iter_mut() {
        let Some(row) = r.as_object_mut() else { continue };
        let mut subject: Option<String> = None;
        if let Some(Value::Array(copies)) = row.get_mut("copies") {
            for c in copies.iter_mut() {
                let Some(c) = c.as_object_mut() else { continue };
                let id = c.get("id").and_then(Value::as_str).and_then(|v| v.parse::<Uuid>().ok());
                let m = id.and_then(|id| meta.get(&id)).cloned().unwrap_or_default();
                subject.get_or_insert_with(|| m.subject.clone());
                c.insert("subject".into(), json!(m.subject));
                c.insert("fromName".into(), json!(m.from_name));
                c.insert("fromEmail".into(), json!(m.from_email));
                c.insert("to".into(), json!(m.to));
                c.insert("cc".into(), json!(m.cc));
                c.insert("snippet".into(), json!(m.snippet));
                c.insert("replyTo".into(), json!(m.reply_to));
                c.insert("inReplyTo".into(), json!(m.in_reply_to));
                c.insert("references".into(), json!(m.references));
                c.insert("listUnsubscribe".into(), json!(m.list_unsubscribe));
                c.insert("listUnsubscribePost".into(), json!(m.list_unsubscribe_post));
                c.insert("deliveryAddresses".into(), json!(m.delivery_addresses));
                c.insert(
                    "attachments".into(),
                    id.and_then(|id| attachments.get(&id)).cloned().unwrap_or_else(|| json!([])),
                );
                c.remove("uid");
            }
        }
        row.insert("subject".into(), json!(subject));
    }
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
    let Some(mut conversation) =
        s.db.json_opt(
            "SELECT id, kind, threading_confidence, manually_locked, logical_message_count, unread_count, copy_count \
             FROM conversations WHERE id = $1 AND user_id = $2 AND account_id = $3",
            params![canonical, &a.user_id, &account],
        )
        .await?
    else {
        return Ok(err(404, "Conversation not found"));
    };
    let mut logical =
        s.db.json_rows(
            "SELECT lm.id, lm.canonical_message_id, lm.direction, lm.message_date,
                    lm.threading_reason, lm.threading_confidence, lm.parent_logical_message_id,
                    COUNT(m.id)::int AS copy_count, (ARRAY_AGG(m.id ORDER BY m.date ASC NULLS LAST, m.id))[1] AS copy_id
               FROM logical_messages lm
               LEFT JOIN messages m ON m.logical_message_id = lm.id AND m.is_deleted = false
              WHERE lm.conversation_id = $1 AND lm.user_id = $2
              GROUP BY lm.id
              ORDER BY lm.message_date ASC NULLS LAST, lm.id",
            params![canonical, &a.user_id],
        )
        .await?;
    let copy_ids: Vec<String> =
        logical.iter().filter_map(|r| r.get("copy_id").and_then(Value::as_str).map(str::to_owned)).collect();
    let mut copies = s
        .db
        .json_rows("SELECT id, account_id, folder, uid FROM messages WHERE id = ANY($1::uuid[])", params![copy_ids])
        .await?;
    s.mail.hydrate_values(&mut copies).await;
    let subject_of = |id: Option<&Value>| {
        copies.iter().find(|c| c.get("id") == id).and_then(|c| c.get("subject")).cloned().unwrap_or(Value::Null)
    };
    for row in &mut logical {
        if let Value::Object(m) = row {
            let subject = subject_of(m.get("copy_id"));
            m.insert("subject".into(), subject);
            m.remove("copy_id");
            m.insert("providerMessageId".into(), Value::Null);
        }
    }
    let first_subject = logical.first().and_then(|l| l.get("subject")).and_then(Value::as_str).unwrap_or("");
    conversation
        .insert("canonical_subject".into(), json!(inboxora_mail::conversation::canonical_subject(first_subject)));
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
    let job = start_rebuild_job(&s.mail, user, account.clone(), limit, dry_run, force);
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
