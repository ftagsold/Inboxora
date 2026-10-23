//! Reading mail: message lists, single messages, threads, unread counts, bodies, headers and
//! attachments, plus the read/star toggles.

use std::collections::HashMap;
use std::io::Write as _;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Result, params};
use inboxora_mail::sanitize::{block_remote_images, has_remote_images, should_block_remote_images};
use serde_json::{Map, Value, json};

use super::{account_of, attachments_of, is_uuid};
use crate::engine::cache::Location;
use crate::http::{Auth, Body, Qs};
use crate::routes::contacts::{query_int, query_string};
use crate::routes::search::resolve_account_scope;
use crate::routes::util::{err, ok};
use crate::services::diagnostics_ring::record_sync_signal;
use crate::state::AppState;

const MESSAGE_COLS: &str = "m.id, m.uid, m.folder, m.message_id, m.date, m.is_read, m.is_starred, m.has_attachments, m.account_id, \
    m.category, m.unsubscribed_at, a.name AS account_name, a.email_address AS account_email, a.color AS account_color";

fn valid_folder_name(name: &str) -> bool {
    let n = inboxora_mail::text::js_len(name);
    n > 0 && n <= 255 && name.chars().all(|c| (c as u32) > 31 && c as u32 != 127)
}

/// `listMessages` (`services/messageService.ts`).
pub async fn list_messages(
    s: &AppState,
    user_id: &str,
    account_id: Option<&str>,
    folder: &str,
    limit: i64,
    offset: i64,
    unread_only: bool,
    threaded: bool,
    category: Option<&str>,
) -> Result<(Vec<Value>, i64, bool, Option<String>)> {
    let accounts: Vec<(String, Option<bool>)> =
        s.db.rows(
            "SELECT id, include_in_unified_inbox FROM email_accounts WHERE user_id = $1 AND enabled = true",
            params![user_id],
        )
        .await?
        .iter()
        .map(|r| (r.get::<_, uuid::Uuid>(0).to_string(), r.get::<_, Option<bool>>(1)))
        .collect();
    let (scoped, resolved) = resolve_account_scope(&accounts, account_id);
    if scoped.is_empty() {
        return Ok((vec![], 0, false, None));
    }
    let mut conds = vec!["m.is_deleted = false".to_owned()];
    let mut values: Vec<Val> = Vec::new();
    let specific = resolved.is_some();
    if let Some(r) = &resolved {
        values.push(Val::from(r.as_str()));
        conds.push(format!("m.account_id = ${}", values.len()));
        values.push(Val::from(folder));
        conds.push(format!("m.folder = ${}", values.len()));
    } else {
        values.push(Val::from(scoped.clone()));
        conds.push(format!("m.account_id = ANY(${})", values.len()));
        conds.push("m.folder = 'INBOX'".into());
    }
    if unread_only {
        conds.push("m.is_read = false".into());
    }
    match category.filter(|c| !c.is_empty()) {
        Some("primary") => conds.push("(m.category IS NULL OR m.category = 'primary')".into()),
        Some(c) => {
            values.push(Val::from(c));
            conds.push(format!("m.category = ${}", values.len()));
        }
        None => {}
    }
    let where_ = conds.join(" AND ");
    let safe_limit = if limit == 0 { 50 } else { limit.clamp(1, 500) };
    let safe_offset = offset.max(0);
    let mut total: i64 = 0;
    let totals = async {
        if specific {
            let r =
                s.db.opt(
                    "SELECT total_count, unread_count FROM folders WHERE account_id = $1 AND path = $2",
                    params![account_id, folder],
                )
                .await?;
            Ok::<_, inboxora_core::Error>(r.map_or(0, |r| {
                i64::from(
                    if unread_only {
                        r.get::<_, Option<i32>>("unread_count")
                    } else {
                        r.get::<_, Option<i32>>("total_count")
                    }
                    .unwrap_or(0),
                )
            }))
        } else {
            let col = if unread_only { "unread_count" } else { "total_count" };
            let r = s
                .db
                .opt(
                    &format!("SELECT COALESCE(SUM({col}), 0)::int AS n FROM folders WHERE account_id = ANY($1) AND path = 'INBOX'"),
                    params![scoped.clone()],
                )
                .await?;
            Ok(r.map_or(0, |r| i64::from(r.get::<_, i32>("n"))))
        }
    };
    if let Ok(t) = totals.await {
        total = t;
    }
    if threaded {
        let p = values.len() + 1;
        let identity = if specific { "m.thread_key" } else { "(m.account_id::text || ':' || m.thread_key)" };
        let sql = format!(
            "WITH paged_threads AS ( \
               SELECT m.account_id, m.thread_key FROM messages m WHERE {where_} GROUP BY m.account_id, m.thread_key \
               ORDER BY MAX(m.date) DESC, m.account_id, m.thread_key LIMIT ${l} OFFSET ${o}), \
             deduped AS MATERIALIZED ( \
               SELECT DISTINCT ON (m.account_id, m.thread_key, COALESCE(NULLIF(btrim(m.message_id), ''), '__physical__:' || m.id::text)) \
                 m.id, m.uid, m.folder, m.message_id, {identity} AS thread_id, m.thread_key, \
                 m.draft_bcc_addresses, m.draft_uid_validity::text AS draft_uid_validity, m.draft_alias_id, \
                 m.draft_in_reply_to, m.draft_references, m.draft_composition, m.date, m.is_read, \
                 m.is_starred, m.has_attachments, m.account_id, m.category, \
                 a.name AS account_name, a.email_address AS account_email, a.color AS account_color \
               FROM messages m JOIN paged_threads pt ON pt.account_id = m.account_id AND pt.thread_key = m.thread_key \
               JOIN email_accounts a ON m.account_id = a.id \
               WHERE {where_} \
               ORDER BY m.account_id, m.thread_key, COALESCE(NULLIF(btrim(m.message_id), ''), '__physical__:' || m.id::text), \
                 CASE WHEN m.folder = 'INBOX' THEN 0 ELSE 1 END, m.date ASC), \
             thread_totals AS ( \
               SELECT {identity} AS thread_id, COUNT(DISTINCT COALESCE(NULLIF(btrim(m.message_id), ''), '__physical__:' || m.id::text))::int AS message_count \
               FROM messages m JOIN paged_threads pt ON pt.account_id = m.account_id AND pt.thread_key = m.thread_key \
               WHERE m.account_id = ANY(${p}) AND m.is_deleted = false GROUP BY {identity}), \
             ranked AS ( \
               SELECT d.*, COALESCE(tt.message_count, 1) AS message_count, \
                 COUNT(*) FILTER (WHERE NOT d.is_read) OVER (PARTITION BY d.thread_id)::int AS unread_count, \
                 FIRST_VALUE(d.id) OVER (PARTITION BY d.thread_id ORDER BY d.date ASC) AS first_id, \
                 ROW_NUMBER() OVER (PARTITION BY d.thread_id ORDER BY d.date DESC) AS rn \
               FROM deduped d LEFT JOIN thread_totals tt ON tt.thread_id = d.thread_id) \
             SELECT id, uid, folder, message_id, thread_id, thread_key, draft_bcc_addresses, draft_uid_validity, draft_alias_id, \
               draft_in_reply_to, draft_references, draft_composition, date, is_starred, is_read, \
               has_attachments, account_id, account_name, account_email, account_color, category, \
               message_count, unread_count, first_id \
             FROM ranked WHERE rn = 1 ORDER BY date DESC",
            l = p + 1,
            o = p + 2,
        );
        let mut tv = values.clone();
        tv.push(Val::from(if specific { vec![resolved.clone().unwrap_or_default()] } else { scoped.clone() }));
        tv.push(Val::Int(safe_limit));
        tv.push(Val::Int(safe_offset));
        let mut rows = s.db.json_rows(&sql, &tv).await?;
        hydrate_threads(s, user_id, &mut rows).await?;
        let count = s
            .db
            .opt(
                &format!(
                    "SELECT COUNT(*)::int AS total FROM (SELECT m.account_id, m.thread_key FROM messages m WHERE {where_} GROUP BY m.account_id, m.thread_key) counted_threads"
                ),
                &values,
            )
            .await?
            .map_or(0, |r| i64::from(r.get::<_, i32>("total")));
        return Ok((rows, count, true, resolved));
    }
    let lp = values.len() + 1;
    values.push(Val::Int(safe_limit));
    values.push(Val::Int(safe_offset));
    let mut rows =
        s.db.json_rows(
            &format!(
                "SELECT m.id, m.uid, m.folder, m.message_id, m.thread_id, m.thread_key, \
                   m.draft_bcc_addresses, m.draft_uid_validity::text AS draft_uid_validity, m.draft_alias_id, \
                   m.draft_in_reply_to, m.draft_references, m.draft_composition, m.date, m.is_read, \
                   m.is_starred, m.has_attachments, m.account_id, m.category, \
                   a.name as account_name, a.email_address as account_email, a.color as account_color \
                 FROM messages m JOIN email_accounts a ON m.account_id = a.id \
                 WHERE {where_} ORDER BY m.date DESC LIMIT ${lp} OFFSET ${}",
                lp + 1
            ),
            &values,
        )
        .await?;
    s.mail.hydrate_values(&mut rows).await;
    mark_contact_photos(s, user_id, &mut rows, "from_email").await?;
    Ok((rows, total, false, resolved))
}

/// Threaded rows carry their latest message; the thread's subject and sender come from its
/// first message (`first_id`), whose headers are fetched alongside.
async fn hydrate_threads(s: &AppState, user_id: &str, rows: &mut [Value]) -> Result<()> {
    s.mail.hydrate_values(rows).await;
    let firsts: Vec<String> = rows
        .iter()
        .filter(|r| r.get("first_id") != r.get("id"))
        .filter_map(|r| r.get("first_id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let mut first_meta: HashMap<String, Map<String, Value>> = HashMap::new();
    if !firsts.is_empty() {
        let mut locs = s
            .db
            .json_rows("SELECT id, account_id, folder, uid FROM messages WHERE id = ANY($1::uuid[])", params![firsts])
            .await?;
        s.mail.hydrate_values(&mut locs).await;
        for l in locs {
            if let Value::Object(m) = l {
                let id = m.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                first_meta.insert(id, m);
            }
        }
    }
    for r in rows.iter_mut() {
        let Some(row) = r.as_object_mut() else { continue };
        row.insert("latest_from_email".into(), row.get("from_email").cloned().unwrap_or(Value::Null));
        row.insert("latest_from_name".into(), row.get("from_name").cloned().unwrap_or(Value::Null));
        let first = row.get("first_id").and_then(Value::as_str).and_then(|f| first_meta.get(f));
        if let Some(f) = first {
            for k in ["subject", "from_name", "from_email"] {
                row.insert(k.into(), f.get(k).cloned().unwrap_or(Value::Null));
            }
        }
        row.remove("first_id");
    }
    mark_contact_photos(s, user_id, rows, "from_email").await
}

/// `has_contact_photo` for each row's sender (`key`), from the user's contacts.
pub async fn mark_contact_photos(s: &AppState, user_id: &str, rows: &mut [Value], key: &str) -> Result<()> {
    let emails: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get(key).and_then(Value::as_str))
        .filter(|e| !e.is_empty())
        .map(str::to_lowercase)
        .collect();
    let with_photo: std::collections::HashSet<String> = if emails.is_empty() {
        Default::default()
    } else {
        let (relation, live) = crate::services::remote_dav::contacts_relation(&s.db, user_id, 3).await;
        s.db.rows(
            &format!(
                "SELECT DISTINCT primary_email FROM {relation} c WHERE user_id = $1 AND primary_email = ANY($2) AND photo_data IS NOT NULL"
            ),
            &[Val::from(user_id), Val::from(emails), live],
        )
        .await?
        .iter()
        .filter_map(|r| r.get::<_, Option<String>>(0))
        .collect()
    };
    for r in rows.iter_mut() {
        if let Some(row) = r.as_object_mut() {
            let has = row.get(key).and_then(Value::as_str).is_some_and(|e| with_photo.contains(&e.to_lowercase()));
            row.insert("has_contact_photo".into(), json!(has));
        }
    }
    Ok(())
}

pub async fn messages(State(s): State<AppState>, a: Auth, q: Qs) -> Result<Response> {
    let account_id = query_string(&q, "accountId");
    let folder = query_string(&q, "folder").unwrap_or_else(|| "INBOX".into());
    let (limit, offset) = (query_int(&q, "limit", 50), query_int(&q, "offset", 0));
    let started = std::time::Instant::now();
    if !valid_folder_name(&folder) {
        return Ok(err(400, "Invalid folder name"));
    }
    let category = query_string(&q, "category")
        .filter(|c| ["primary", "newsletter", "promotion", "automated", "social"].contains(&c.as_str()));
    let (messages, total, threaded, resolved) = list_messages(
        &s,
        &a.user_id,
        account_id.as_deref(),
        &folder,
        limit,
        offset,
        query_string(&q, "unreadOnly").as_deref() == Some("true"),
        query_string(&q, "threaded").as_deref() == Some("true"),
        category.as_deref(),
    )
    .await?;
    tracing::info!(
        "[perf] GET /api/mail/messages scope={} folder={folder} {}ms total={total}",
        account_id.as_deref().filter(|a| !a.is_empty()).unwrap_or("unified"),
        started.elapsed().as_millis()
    );
    if let Some(acc) = &resolved
        && !messages.is_empty()
    {
        let ghosts = messages
            .iter()
            .filter(|m| {
                m.get("message_id").is_none_or(Value::is_null)
                    && m.get("subject").and_then(Value::as_str).is_none_or(|s| s.is_empty() || s == "(no subject)")
                    && m.get("snippet").and_then(Value::as_str).is_none_or(str::is_empty)
            })
            .count();
        if ghosts > 0 {
            record_sync_signal("ghost_rows_served", Some(acc), Some(ghosts as f64));
        }
    }
    let mut body = json!({ "messages": messages, "total": total });
    if threaded {
        body["threaded"] = json!(true);
    }
    Ok(ok(body))
}

pub async fn message(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message ID"));
    }
    let r = s
        .db
        .json_opt(
            &format!(
                "SELECT {MESSAGE_COLS} FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.id = $1 AND a.user_id = $2 AND m.is_deleted = false"
            ),
            params![&id, &a.user_id],
        )
        .await;
    Ok(match r {
        Ok(Some(mut row)) => {
            s.mail.hydrate_rows(std::slice::from_mut(&mut row)).await;
            ok(Value::Object(row))
        }
        Ok(None) => err(404, "Message not found"),
        Err(e) => {
            tracing::error!("GET /messages/:id error: {e}");
            err(500, "Failed to load message")
        }
    })
}

pub async fn resolve_message(State(s): State<AppState>, a: Auth, q: Qs) -> Result<Response> {
    let reference = q.str("ref").unwrap_or_default().to_owned();
    if reference.is_empty() {
        return Ok(err(400, "Missing ref"));
    }
    let raw_account = q.str("accountId").unwrap_or_default().to_owned();
    if !raw_account.is_empty() && !is_uuid(&raw_account) {
        return Ok(err(400, "Invalid accountId"));
    }
    let account = (!raw_account.is_empty()).then_some(raw_account);
    let run = async {
        let mut row = s
            .db
            .json_opt(
                &format!(
                    "SELECT {MESSAGE_COLS} FROM messages m JOIN email_accounts a ON m.account_id = a.id \
                     WHERE m.message_id = $1 AND a.user_id = $2 AND m.is_deleted = false AND ($3::uuid IS NULL OR m.account_id = $3) \
                     ORDER BY (m.folder = 'INBOX') DESC, m.date DESC NULLS LAST LIMIT 1"
                ),
                params![&reference, &a.user_id, &account],
            )
            .await?;
        if row.is_none() && is_uuid(&reference) {
            row = s
                .db
                .json_opt(
                    &format!(
                        "SELECT {MESSAGE_COLS} FROM messages m JOIN email_accounts a ON m.account_id = a.id \
                         WHERE m.id = $1 AND a.user_id = $2 AND m.is_deleted = false AND ($3::uuid IS NULL OR m.account_id = $3)"
                    ),
                    params![&reference, &a.user_id, &account],
                )
                .await?;
        }
        Ok::<_, inboxora_core::Error>(row)
    };
    Ok(match run.await {
        Ok(Some(mut r)) => {
            s.mail.hydrate_rows(std::slice::from_mut(&mut r)).await;
            ok(Value::Object(r))
        }
        Ok(None) => err(404, "Message not found"),
        Err(e) => {
            tracing::error!("GET /resolve-message error: {e}");
            err(500, "Failed to resolve message")
        }
    })
}

pub async fn thread(State(s): State<AppState>, a: Auth, Path(thread_id): Path<String>, q: Qs) -> Result<Response> {
    if thread_id.is_empty() {
        return Ok(err(400, "threadId required"));
    }
    let run = async {
        let requested = q.first("accountId").filter(|v| !v.is_empty());
        let accounts: Vec<(String, Option<bool>)> =
            s.db.rows(
                "SELECT id, include_in_unified_inbox FROM email_accounts WHERE user_id = $1 AND enabled = true",
                params![&a.user_id],
            )
            .await?
            .iter()
            .map(|r| (r.get::<_, uuid::Uuid>(0).to_string(), r.get::<_, Option<bool>>(1)))
            .collect();
        let ids: Vec<String> = if let Some(r) = requested {
            if !accounts.iter().any(|(id, _)| *id == r) {
                return Ok(Some(err(404, "Account not found")));
            }
            vec![r]
        } else if q.str("unified") == Some("true") {
            resolve_account_scope(&accounts, None).0
        } else {
            accounts.into_iter().map(|(id, _)| id).collect()
        };
        if ids.is_empty() {
            return Ok(Some(ok(json!({ "messages": [] }))));
        }
        let mut rows = s
            .db
            .json_rows(
                "WITH deduped AS ( \
                   SELECT DISTINCT ON (m.account_id, COALESCE(NULLIF(btrim(m.message_id), ''), '__physical__:' || m.id::text)) \
                     m.id, m.uid, m.folder, m.message_id, m.thread_id, m.thread_key, m.date, m.is_read, m.is_starred, m.has_attachments, \
                     m.account_id, m.category, m.unsubscribed_at, \
                     a.name AS account_name, a.email_address AS account_email, a.color AS account_color \
                   FROM messages m JOIN email_accounts a ON m.account_id = a.id \
                   WHERE m.is_deleted = false AND m.account_id = ANY($1) AND m.thread_key = $2 \
                   ORDER BY m.account_id, COALESCE(NULLIF(btrim(m.message_id), ''), '__physical__:' || m.id::text), \
                     CASE WHEN m.folder = 'INBOX' THEN 0 ELSE 1 END, m.date ASC) \
                 SELECT * FROM deduped ORDER BY date ASC",
                params![ids, &thread_id],
            )
            .await?;
        s.mail.hydrate_values(&mut rows).await;
        Ok::<_, inboxora_core::Error>(Some(ok(json!({ "messages": rows }))))
    };
    match run.await {
        Ok(Some(r)) => Ok(r),
        Ok(None) => Ok(err(500, "Failed to load thread")),
        Err(e) => {
            tracing::error!("Thread fetch error: {e}");
            Ok(err(500, "Failed to load thread"))
        }
    }
}

pub async fn unread_counts(State(s): State<AppState>, a: Auth) -> Result<Response> {
    let rows = s
        .db
        .rows(
            "SELECT m.account_id, a.include_in_unified_inbox, COUNT(*) AS count FROM messages m JOIN email_accounts a ON a.id = m.account_id \
             WHERE a.user_id = $1 AND a.enabled = true AND m.folder = 'INBOX' AND m.is_read = false AND m.is_deleted = false \
             GROUP BY m.account_id, a.include_in_unified_inbox",
            params![&a.user_id],
        )
        .await?;
    let mut by = serde_json::Map::new();
    let mut total = 0i64;
    for r in rows {
        let n: i64 = r.get("count");
        by.insert(r.get::<_, uuid::Uuid>("account_id").to_string(), json!(n));
        if r.get::<_, Option<bool>>("include_in_unified_inbox") != Some(false) {
            total += n;
        }
    }
    let mut res = ok(json!({ "total": total, "byAccount": by }));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

const BODY_FETCH_TIMEOUT: Duration = Duration::from_secs(40);

pub async fn body(State(s): State<AppState>, a: Auth, Path(id): Path<String>, q: Qs) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let Some(mut m) =
        s.db.json_opt(
            "SELECT m.id, m.account_id, m.folder, m.uid, u.preferences, ci.message_id AS calendar_invitation_id FROM messages m \
             JOIN email_accounts a ON m.account_id = a.id JOIN users u ON u.id = a.user_id \
             LEFT JOIN inbound_calendar_invitations ci ON ci.message_id = m.id WHERE m.id = $1 AND a.user_id = $2",
            params![&id, &a.user_id],
        )
        .await?
    else {
        return Ok(err(404, "Message not found"));
    };
    let Some(loc) = Location::from_row(&m) else { return Ok(err(404, "Message not found")) };
    let fetched = tokio::time::timeout(BODY_FETCH_TIMEOUT, async {
        let (meta, body) = tokio::join!(s.mail.message_meta(std::slice::from_ref(&loc)), s.mail.message_body(&loc));
        (meta, body)
    })
    .await;
    let (meta, body) = match fetched {
        Ok((meta, Ok(b))) => (meta, b),
        Ok((_, Err(e))) => return Ok(body_fetch_error(&e.0)),
        Err(_) => return Ok(body_fetch_error("BODY_FETCH_TIMEOUT")),
    };
    if let Some(meta) = meta.get(&loc.id) {
        meta.apply(&mut m);
    }
    let st = |k: &str| m.get(k).and_then(Value::as_str);
    let prefs = m.get("preferences").cloned().unwrap_or(Value::Null);
    let skip_blocking = q.str("remoteImages") == Some("1");
    let (html, blocked) = match body.html.as_deref() {
        Some(h) if !skip_blocking && should_block_remote_images(&prefs, st("from_email")) && has_remote_images(h) => {
            (Some(block_remote_images(h)), true)
        }
        Some(h) => (Some(h.to_owned()), false),
        None => (None, false),
    };
    let mut out = json!({
        "html": html,
        "text": body.text.clone().map_or(Value::Null, Value::String),
        "attachments": body.attachments,
        "hasBlockedRemoteImages": blocked,
        "senderEmail": m.get("sender_email").cloned().unwrap_or(Value::Null),
        "senderName": m.get("sender_name").cloned().unwrap_or(Value::Null),
    });
    if m.get("calendar_invitation_id").is_some_and(|v| !v.is_null()) {
        out["calendarInvitation"] = json!(true);
    }
    Ok(ok(out))
}

fn body_fetch_error(msg: &str) -> Response {
    let msg = if msg.is_empty() { "Unknown error" } else { msg };
    tracing::error!("Body fetch error: {msg}");
    if msg.to_uppercase().contains("THROTTL") {
        return crate::routes::util::json(
            503,
            json!({ "error": "The mail server is temporarily throttling access. Please wait a few minutes and try again.", "throttled": true }),
        );
    }
    if msg == "BODY_FETCH_TIMEOUT" {
        return crate::routes::util::json(
            504,
            json!({ "error": "This message is taking too long to load — the mail server may be temporarily unreachable. Please try again.", "timeout": true }),
        );
    }
    err(500, msg)
}

/// A message's attachment list (from its cached body) when it belongs to `user`.
async fn owned_attachments(s: &AppState, user: &str, id: &str) -> Result<Option<(Map<String, Value>, Vec<Value>)>> {
    let Some(mut m) =
        s.db.json_opt(
            "SELECT m.id, m.account_id, m.folder, m.uid FROM messages m JOIN email_accounts a ON m.account_id = a.id \
             WHERE m.id = $1 AND a.user_id = $2",
            params![id, user],
        )
        .await?
    else {
        return Ok(None);
    };
    let Some(loc) = Location::from_row(&m) else { return Ok(None) };
    let atts = match s.mail.message_body(&loc).await {
        Ok(b) => attachments_of(Some(&b.attachments)).as_array().cloned().unwrap_or_default(),
        Err(e) => {
            tracing::warn!("Attachment list fetch failed: {e}");
            vec![]
        }
    };
    s.mail.hydrate_rows(std::slice::from_mut(&mut m)).await;
    Ok(Some((m, atts)))
}

pub async fn headers(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let Some(m) = s
        .db
        .json_opt("SELECT m.*, a.user_id FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.id = $1 AND a.user_id = $2", params![
            &id,
            &a.user_id
        ])
        .await?
    else {
        return Ok(err(404, "Message not found"));
    };
    let run = async {
        let account_id = m.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
        let mut headers = String::new();
        if let Some(account) = account_of(&s, &account_id).await? {
            let uid = inboxora_core::json::int(m.get("uid")).unwrap_or(0);
            let folder = m.get("folder").and_then(Value::as_str).unwrap_or("INBOX");
            match s.mail.fetch_headers(&account, uid, folder).await {
                Ok(h) => headers = h,
                Err(e) => tracing::warn!("Headers IMAP fetch failed: {e}"),
            }
        }
        if headers.trim().is_empty() {
            let mut row = m.clone();
            s.mail.hydrate_rows(std::slice::from_mut(&mut row)).await;
            headers = inboxora_mail::headers::build_headers_from_message(&Value::Object(row));
        }
        let mut subject = Value::Null;
        if !headers.trim().is_empty() {
            let parsed = inboxora_mail::headers::Headers::parse(&headers);
            let imap_subject =
                inboxora_mail::headers::decode_mime_words(parsed.get("subject").unwrap_or("")).trim().to_owned();
            if !imap_subject.is_empty() {
                subject = json!(imap_subject);
            }
        }
        Ok::<_, crate::engine::EngineError>(ok(json!({ "headers": headers, "subject": subject })))
    };
    Ok(run.await.unwrap_or_else(|e| {
        tracing::error!("Headers fetch error: {e}");
        err(500, "Failed to fetch message headers")
    }))
}

const ZIP_MAX_FILES: usize = 100;
const ZIP_MAX_TOTAL: f64 = 150.0 * 1024.0 * 1024.0;
const ZIP_MAX_FILE: f64 = 50.0 * 1024.0 * 1024.0;

fn att_size(a: &Value) -> f64 {
    a.get("size").and_then(Value::as_f64).unwrap_or(0.0)
}

pub async fn attachments_zip(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let Some((m, atts)) = owned_attachments(&s, &a.user_id, &id).await? else {
        return Ok(err(404, "Message not found"));
    };
    if atts.is_empty() {
        return Ok(err(404, "No attachments"));
    }
    if atts.len() > ZIP_MAX_FILES {
        return Ok(err(400, &format!("Too many attachments (max {ZIP_MAX_FILES})")));
    }
    if atts.iter().map(att_size).sum::<f64>() > ZIP_MAX_TOTAL {
        return Ok(err(413, "Total attachment size exceeds the 150 MB ZIP limit."));
    }
    let eligible: Vec<&Value> = atts.iter().filter(|a| att_size(a) == 0.0 || att_size(a) <= ZIP_MAX_FILE).collect();
    if eligible.is_empty() {
        return Ok(err(413, "All attachments exceed the 50 MB per-file limit."));
    }
    let run = async {
        let account_id = m.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
        let Some(account) = account_of(&s, &account_id).await? else { return Ok(Some(err(404, "Account not found"))) };
        let parts: Vec<(String, Option<String>)> = eligible
            .iter()
            .map(|a| {
                (
                    inboxora_mail::text::js_string(a.get("part").unwrap_or(&Value::Null)),
                    a.get("encoding").and_then(Value::as_str).map(str::to_owned),
                )
            })
            .collect();
        let uid = inboxora_core::json::int(m.get("uid")).unwrap_or(0);
        let folder = m.get("folder").and_then(Value::as_str).unwrap_or("INBOX").to_owned();
        let buffers = s.mail.fetch_multiple_attachments(&account, uid, &folder, &parts).await?;
        if buffers.is_empty() {
            return Ok(Some(err(404, "Could not fetch attachments")));
        }
        let mut used: HashMap<String, u32> = HashMap::new();
        let mut entries: Vec<(String, &Vec<u8>)> = Vec::new();
        for (att, (part, _)) in eligible.iter().zip(&parts) {
            let Some(buf) = buffers.get(part) else { continue };
            let mut name = inboxora_mail::util::safe_filename(att.get("filename").and_then(Value::as_str));
            match used.get_mut(&name) {
                Some(n) => {
                    *n += 1;
                    let k = *n;
                    name = match name.rfind('.') {
                        Some(dot) if dot > 0 => format!("{} ({k}){}", &name[..dot], &name[dot..]),
                        _ => format!("{name} ({k})"),
                    };
                }
                None => {
                    used.insert(name.clone(), 1);
                }
            }
            entries.push((name, buf));
        }
        if entries.is_empty() {
            return Ok(Some(err(404, "Could not fetch attachments")));
        }
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut cursor);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .compression_level(Some(6));
            for (name, buf) in entries {
                zw.start_file(name, opts).map_err(|e| crate::engine::EngineError(e.to_string()))?;
                zw.write_all(buf).map_err(|e| crate::engine::EngineError(e.to_string()))?;
            }
            zw.finish().map_err(|e| crate::engine::EngineError(e.to_string()))?;
        }
        let subject = m.get("subject").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("attachments");
        let zip_name = format!("{}-attachments.zip", inboxora_mail::text::js_substring(subject, 100));
        let mut res = cursor.into_inner().into_response();
        let h = res.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
        if let Ok(v) = HeaderValue::from_str(&inboxora_mail::util::attachment_disposition(&zip_name)) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
        res.extensions_mut().insert(crate::http::middleware::NoEtag);
        Ok::<_, crate::engine::EngineError>(Some(res))
    };
    match run.await {
        Ok(Some(r)) => Ok(r),
        Ok(None) => Ok(err(500, "Failed to create ZIP")),
        Err(e) => {
            tracing::error!("ZIP fetch error: {e}");
            Ok(err(500, "Failed to create ZIP"))
        }
    }
}

pub async fn attachment(
    State(s): State<AppState>,
    a: Auth,
    Path((id, part)): Path<(String, String)>,
) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let Some(part_num) = crate::routes::dav::decode_uri_component(&part) else {
        return Ok(err(400, "Invalid attachment part identifier"));
    };
    let Some((m, atts)) = owned_attachments(&s, &a.user_id, &id).await? else {
        return Ok(err(404, "Message not found"));
    };
    let Some(att) = atts
        .iter()
        .find(|a| inboxora_mail::text::js_string(a.get("part").unwrap_or(&Value::Null)) == part_num)
        .cloned()
    else {
        return Ok(err(404, "Attachment not found"));
    };
    if att.get("size").and_then(Value::as_f64).is_some_and(|s| s > 50.0 * 1024.0 * 1024.0) {
        return Ok(err(413, "Attachment exceeds the 50 MB download limit."));
    }
    let run = async {
        let account_id = m.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
        let Some(account) = account_of(&s, &account_id).await? else { return Ok(err(404, "Account not found")) };
        let uid = inboxora_core::json::int(m.get("uid")).unwrap_or(0);
        let folder = m.get("folder").and_then(Value::as_str).unwrap_or("INBOX").to_owned();
        let Some(buf) = s.mail.fetch_attachment(&account, uid, &folder, &part_num).await? else {
            return Ok(err(404, "Could not fetch attachment"));
        };
        let ct =
            att.get("type").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or("application/octet-stream");
        let name = att.get("filename").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or("attachment");
        let len = buf.len();
        let mut res = buf.into_response();
        let h = res.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(ct).unwrap_or(HeaderValue::from_static("application/octet-stream")),
        );
        if let Ok(v) = HeaderValue::from_str(&inboxora_mail::util::attachment_disposition(name)) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        Ok::<_, crate::engine::EngineError>(res)
    };
    Ok(run.await.unwrap_or_else(|e| {
        tracing::error!("Attachment fetch error: {e}");
        err(500, "Failed to fetch attachment")
    }))
}

/// PATCH read/star: DB first (with the local-wins marker), then the IMAP flag, queued for the
/// reconciler when the push fails.
async fn toggle(s: &AppState, user: &str, id: &str, value: &Value, flag: &str) -> Result<Option<(Value, bool)>> {
    let Some(m) = s
        .db
        .json_opt("SELECT m.*, a.user_id FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.id = $1 AND a.user_id = $2", params![
            id, user
        ])
        .await?
    else {
        return Ok(None);
    };
    let (col, marker, prev_key) = if flag == "\\Seen" {
        ("is_read", "read_changed_at", "is_read")
    } else {
        ("is_starred", "star_changed_at", "is_starred")
    };
    s.db.exec(
        &format!("UPDATE messages SET {col} = $1, {marker} = NOW() WHERE id = $2"),
        &[Val::from(value), Val::from(id)],
    )
    .await?;
    let new = inboxora_core::json::truthy(Some(value));
    let prev = m.get(prev_key).and_then(Value::as_bool).unwrap_or(false);
    let account_id = m.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
    if prev != new && flag == "\\Seen" {
        let folder = m.get("folder").and_then(Value::as_str).unwrap_or("");
        crate::services::mail_utils::adjust_folder_counts(&s.db, &account_id, folder, 0, if new { -1 } else { 1 });
    }
    if let Some(account) = account_of(s, &account_id).await? {
        let uid = inboxora_core::json::int(m.get("uid")).unwrap_or(0);
        let folder = m.get("folder").and_then(Value::as_str).unwrap_or("INBOX");
        match s.mail.set_flag(&account, uid, folder, flag, new).await {
            Ok(()) => s.mail.resolve_flag_push(&account_id, id, flag),
            Err(e) => {
                tracing::error!(
                    "{} failed: {e}",
                    if flag == "\\Seen" { "IMAP flag update" } else { "IMAP star update" }
                );
                s.mail.enqueue_flag_push(&account_id, id, flag, new);
            }
        }
    }
    Ok(Some((json!(account_id), prev != new)))
}

pub async fn mark_read(State(s): State<AppState>, a: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let read = body.get("read").cloned().unwrap_or(Value::Null);
    let Some((account_id, changed)) = toggle(&s, &a.user_id, &id, &read, "\\Seen").await? else {
        return Ok(err(404, "Message not found"));
    };
    if changed {
        s.mail.broadcast(
            json!({ "type": "message_flags", "accountId": account_id, "changes": [{ "id": id, "is_read": read }] }),
            Some(&a.user_id),
        );
    }
    let mut out = json!({ "ok": true });
    if !read.is_null() || body.has("read") {
        out["is_read"] = read;
    }
    Ok(ok(out))
}

pub async fn mark_star(State(s): State<AppState>, a: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let starred = body.get("starred").cloned().unwrap_or(Value::Null);
    let Some((account_id, changed)) = toggle(&s, &a.user_id, &id, &starred, "\\Flagged").await? else {
        return Ok(err(404, "Message not found"));
    };
    if changed {
        s.mail.broadcast(
            json!({ "type": "message_flags", "accountId": account_id, "changes": [{ "id": id, "is_starred": starred }] }),
            Some(&a.user_id),
        );
    }
    let mut out = json!({ "ok": true });
    if !starred.is_null() || body.has("starred") {
        out["is_starred"] = starred;
    }
    Ok(ok(out))
}
