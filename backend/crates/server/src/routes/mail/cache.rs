//! Cache-facing mail endpoints: display fields for rows a list served as `meta_pending`, the
//! preload status the client blocks on, and the change feed that refreshes a list view
//! without reloading it.

use axum::extract::State;
use axum::response::Response;
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Result, params};
use serde_json::{Value, json};

use super::bulk::bulk_ids;
use super::read::{View, mark_contact_photos, view_of};
use crate::http::{Auth, Body, Qs};
use crate::routes::contacts::query_string;
use crate::routes::util::{err, ok};
use crate::state::AppState;

/// Rows changed after `since - CHANGE_OVERLAP` are returned: sequence numbers of transactions
/// still in flight when the client read its sequence commit later, and merging is idempotent.
const CHANGE_OVERLAP: i64 = 100;
/// More changes than this and the client reloads the view instead.
const CHANGE_LIMIT: i64 = 300;

/// The newest change sequence (0 before the first change).
pub async fn current_change_seq(s: &AppState) -> i64 {
    s.db.opt("SELECT CASE WHEN is_called THEN last_value ELSE 0 END AS seq FROM messages_change_seq", params![])
        .await
        .ok()
        .flatten()
        .map_or(0, |r| r.get::<_, i64>("seq"))
}

/// `POST /mail/meta {ids}`: display fields of the user's messages, fetched from IMAP when the
/// cache misses them (the client asks for rows a list served with `meta_pending`).
pub async fn meta(State(s): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let ids = match bulk_ids(&body, "Invalid message ids") {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let mut rows =
        s.db.json_rows(
            "SELECT m.id, m.account_id, m.folder, m.uid FROM messages m JOIN email_accounts a ON m.account_id = a.id \
             WHERE m.id = ANY($1::uuid[]) AND a.user_id = $2",
            params![ids, &a.user_id],
        )
        .await?;
    s.mail.hydrate_list_values(&mut rows, std::time::Duration::from_secs(20)).await;
    mark_contact_photos(&s, &a.user_id, &mut rows, "from_email").await?;
    for r in &mut rows {
        if let Some(m) = r.as_object_mut() {
            m.remove("uid");
            m.remove("folder");
            m.remove("account_id");
        }
    }
    Ok(ok(json!({ "messages": rows })))
}

/// `GET /mail/preload/status`: whether every account of the user has its cache loaded.
pub async fn preload_status(State(s): State<AppState>, a: Auth) -> Result<Response> {
    match s.mail.preload_overview(&a.user_id).await {
        Ok(v) => Ok(ok(v)),
        Err(e) => {
            tracing::error!("GET /mail/preload/status error: {e}");
            Ok(err(500, "Failed to load preload status"))
        }
    }
}

/// `GET /mail/messages/changes?since=<seq>&accountId&folder&unreadOnly&category`: rows of the
/// view's accounts changed since `since` (with `in_view` telling whether each belongs in the
/// view now) and ids deleted since. `reset: true` asks the client to reload the view.
pub async fn changes(State(s): State<AppState>, a: Auth, q: Qs) -> Result<Response> {
    let seq = current_change_seq(&s).await;
    let since = query_string(&q, "since").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    if since <= 0 || query_string(&q, "threaded").as_deref() == Some("true") {
        return Ok(ok(json!({ "seq": seq, "reset": true })));
    }
    let folder = query_string(&q, "folder").unwrap_or_else(|| "INBOX".into());
    let category = query_string(&q, "category")
        .filter(|c| ["primary", "newsletter", "promotion", "automated", "social"].contains(&c.as_str()));
    let Some(View { conds, mut values, scoped, resolved }) = view_of(
        &s,
        &a.user_id,
        query_string(&q, "accountId").as_deref(),
        &folder,
        super::read::ListFilter::from_query(&q),
        category.as_deref(),
    )
    .await?
    else {
        return Ok(ok(json!({ "seq": seq, "changed": [], "removed": [] })));
    };
    let from = (since - CHANGE_OVERLAP).max(0);
    let filter = super::read::ListFilter::from_query(&q);
    let starred_total = if filter.starred { super::read::count_view(&s, &conds, &values).await } else { None };
    let in_view = conds.join(" AND ");
    values.push(Val::from(scoped.clone()));
    let acc = values.len();
    values.push(Val::Int(from));
    let fp = values.len();
    values.push(Val::Int(CHANGE_LIMIT + 1));
    let lp = values.len();
    let mut rows =
        s.db.json_rows(
            &format!(
                "SELECT m.id, m.uid, m.folder, m.message_id, m.thread_id, m.thread_key, \
                   m.draft_bcc_addresses, m.draft_uid_validity::text AS draft_uid_validity, m.draft_alias_id, \
                   m.draft_in_reply_to, m.draft_references, m.draft_composition, m.date, m.is_read, \
                   m.is_starred, m.has_attachments, m.account_id, m.category, m.change_seq, \
                   a.name as account_name, a.email_address as account_email, a.color as account_color, \
                   ({in_view}) AS in_view \
                 FROM messages m JOIN email_accounts a ON m.account_id = a.id \
                 WHERE m.account_id = ANY(${acc}::uuid[]) AND m.change_seq > ${fp} ORDER BY m.change_seq LIMIT ${lp}"
            ),
            &values,
        )
        .await?;
    if rows.len() as i64 > CHANGE_LIMIT {
        return Ok(ok(json!({ "seq": seq, "reset": true })));
    }
    let removed: Vec<String> =
        s.db.rows(
            "SELECT id FROM message_tombstones WHERE account_id = ANY($1::uuid[]) AND seq > $2",
            params![scoped.clone(), from],
        )
        .await?
        .iter()
        .map(|r| r.get::<_, uuid::Uuid>("id").to_string())
        .collect();
    let (mut visible, gone): (Vec<Value>, Vec<Value>) =
        rows.drain(..).partition(|r| r.get("in_view") == Some(&Value::Bool(true)));
    s.mail.hydrate_list_values(&mut visible, std::time::Duration::from_millis(1500)).await;
    mark_contact_photos(&s, &a.user_id, &mut visible, "from_email").await?;
    let removed: Vec<Value> =
        removed.into_iter().map(Value::String).chain(gone.iter().filter_map(|r| r.get("id").cloned())).collect();
    let total = match starred_total {
        Some(n) => n,
        None => view_total(&s, resolved.as_deref(), &folder, &scoped, filter.unread).await,
    };
    Ok(ok(json!({ "seq": seq, "changed": visible, "removed": removed, "total": total })))
}

/// The view's message count from the folder counters (as `GET /messages` reports it).
async fn view_total(s: &AppState, account: Option<&str>, folder: &str, scoped: &[String], unread_only: bool) -> i64 {
    let col = if unread_only { "unread_count" } else { "total_count" };
    let r = match account {
        Some(acc) => {
            s.db.opt(
                &format!("SELECT COALESCE({col}, 0)::int AS n FROM folders WHERE account_id = $1 AND path = $2"),
                params![acc, folder],
            )
            .await
        }
        None => {
            s.db.opt(
                &format!("SELECT COALESCE(SUM({col}), 0)::int AS n FROM folders WHERE account_id = ANY($1) AND path = 'INBOX'"),
                params![scoped.to_vec()],
            )
            .await
        }
    };
    r.ok().flatten().map_or(0, |r| i64::from(r.get::<_, i32>("n")))
}
