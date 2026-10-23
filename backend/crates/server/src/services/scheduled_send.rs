//! Scheduled send: `POST /mail/send` with `sendAt` stores the request in `scheduled_sends`,
//! and a worker sends it through the regular send path once it is due. Each send uses the
//! idempotency key `scheduled:<id>`, so a crash mid-send never dispatches the mail twice. The
//! list/detail/update/cancel routes back the scheduled rows the web app shows in the Sent
//! folder. The worker sleeps until the next row is due and is woken whenever one changes.

use std::sync::LazyLock;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::Response;
use chrono::{DateTime, Utc};
use inboxora_core::pg::Q;
use inboxora_core::{Result, params};
use serde_json::{Map, Value, json};

use crate::http::{Auth, Body, Qs};
use crate::routes::contacts::query_string;
use crate::routes::mail::{account_for_user, compose, is_uuid};
use crate::routes::util::{err, ok};
use crate::state::AppState;

/// The longest the worker sleeps between checks (it is woken early on every change).
const MAX_SLEEP: Duration = Duration::from_secs(60);
/// Wakes the worker after a scheduled mail was added or changed.
static WAKE: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);
/// Mail due in less than this is sent right away instead of scheduled.
const MIN_LEAD_SECS: i64 = 30;
/// The furthest ahead a send can be scheduled.
const MAX_AHEAD_DAYS: i64 = 366;
/// Upper bound for a stored request (attachments are capped at 25 MB, base64 adds a third).
const MAX_PAYLOAD_BYTES: usize = 36 * 1024 * 1024;

/// Columns for list and detail rows: never the attachment contents, only their names.
const SUMMARY: &str = "id, account_id, send_at, status, last_error, created_at, \
     payload->'to' AS to, payload->'cc' AS cc, payload->'bcc' AS bcc, payload->>'subject' AS subject, \
     COALESCE(jsonb_path_query_array(payload, '$.attachments[*].filename'), '[]'::jsonb) AS attachments";

/// Tell the user's clients; `sent` means the mail just went out (its Sent copy follows).
fn notify(s: &AppState, user: &str, account_id: &str, sent: bool) {
    s.mail.broadcast(json!({ "type": "scheduled_changed", "accountId": account_id, "sent": sent }), Some(user));
}

fn recipients(v: Option<&Value>) -> usize {
    match v {
        Some(Value::Array(a)) => a.iter().filter(|x| x.as_str().is_some_and(|s| !s.trim().is_empty())).count(),
        Some(Value::String(s)) => usize::from(!s.trim().is_empty()),
        _ => 0,
    }
}

/// A request ready to store: its send time, account and payload (without `sendAt`).
struct Valid {
    send_at: DateTime<Utc>,
    account_id: String,
    payload: Value,
}

/// Validate a send request carrying `sendAt`; `Err` is the response to return.
async fn validate(s: &AppState, user: &str, mut payload: Value) -> Result<std::result::Result<Valid, Response>> {
    let Some(obj) = payload.as_object_mut() else { return Ok(Err(err(400, "Invalid request body"))) };
    let Some(send_at) = obj
        .remove("sendAt")
        .and_then(|v| v.as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()))
        .map(|t| t.with_timezone(&Utc))
    else {
        return Ok(Err(err(400, "sendAt must be an ISO 8601 date-time")));
    };
    let now = Utc::now();
    if send_at < now + chrono::Duration::seconds(MIN_LEAD_SECS) {
        return Ok(Err(err(400, "The scheduled time must be in the future")));
    }
    if send_at > now + chrono::Duration::days(MAX_AHEAD_DAYS) {
        return Ok(Err(err(400, "Mail can be scheduled at most one year ahead")));
    }
    let Some(account_id) = obj.get("accountId").and_then(Value::as_str).filter(|v| is_uuid(v)).map(str::to_owned)
    else {
        return Ok(Err(err(400, "accountId required")));
    };
    if recipients(obj.get("to")) + recipients(obj.get("cc")) + recipients(obj.get("bcc")) == 0 {
        return Ok(Err(err(400, "At least one recipient is required")));
    }
    if account_for_user(s, &account_id, user).await?.is_none() {
        return Ok(Err(err(404, "Account not found")));
    }
    if payload.to_string().len() > MAX_PAYLOAD_BYTES {
        return Ok(Err(err(400, "Total attachment size exceeds 25 MB")));
    }
    Ok(Ok(Valid { send_at, account_id, payload }))
}

/// `POST /mail/send` with `sendAt`: validate and store the request.
pub async fn schedule(s: &AppState, user: &str, payload: Value) -> Result<Response> {
    let v = match validate(s, user, payload).await? {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let row =
        s.db.opt(
            "INSERT INTO scheduled_sends (user_id, account_id, send_at, payload) VALUES ($1, $2, $3, $4) RETURNING id",
            params![user, &v.account_id, v.send_at, v.payload],
        )
        .await?
        .ok_or_else(|| inboxora_core::Error::Internal("scheduled send insert returned no row".into()))?;
    let id: uuid::Uuid = row.get("id");
    notify(s, user, &v.account_id, false);
    WAKE.notify_one();
    Ok(ok(json!({
        "ok": true,
        "scheduled": { "id": id.to_string(), "sendAt": inboxora_core::json::js_iso(&v.send_at) },
    })))
}

/// `PATCH /mail/scheduled/:id` `{ sendAt?, payload? }`: move a scheduled (or failed) mail to a
/// new time and/or replace its content. A failed mail is scheduled again.
pub async fn update(State(s): State<AppState>, a: Auth, Path(id): Path<String>, b: Body) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(404, "Scheduled mail not found"));
    }
    let Some(row) =
        s.db.opt(
            "SELECT status, send_at, payload, account_id FROM scheduled_sends WHERE id = $1 AND user_id = $2",
            params![&id, &a.user_id],
        )
        .await?
    else {
        return Ok(err(404, "Scheduled mail not found"));
    };
    if row.get::<_, String>("status") == "sending" {
        return Ok(err(409, "This message is being sent right now"));
    }
    let old_account = row.get::<_, uuid::Uuid>("account_id").to_string();
    let mut payload = match b.get("payload") {
        Some(p @ Value::Object(_)) => p.clone(),
        Some(_) => return Ok(err(400, "payload must be an object")),
        None => row.get::<_, Value>("payload"),
    };
    let send_at = match b.get("sendAt") {
        Some(v) => v.clone(),
        None => json!(inboxora_core::json::js_iso(&row.get::<_, DateTime<Utc>>("send_at"))),
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("sendAt".into(), send_at);
    }
    let v = match validate(&s, &a.user_id, payload).await? {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let updated =
        s.db.exec(
            "UPDATE scheduled_sends SET send_at = $3, payload = $4, account_id = $5, status = 'scheduled', \
               last_error = NULL, revision = revision + 1 \
             WHERE id = $1 AND user_id = $2 AND status <> 'sending'",
            params![&id, &a.user_id, v.send_at, v.payload, &v.account_id],
        )
        .await?;
    if updated == 0 {
        return Ok(err(409, "This message is being sent right now"));
    }
    notify(&s, &a.user_id, &v.account_id, false);
    if old_account != v.account_id {
        notify(&s, &a.user_id, &old_account, false);
    }
    WAKE.notify_one();
    Ok(ok(json!({ "ok": true, "sendAt": inboxora_core::json::js_iso(&v.send_at) })))
}

/// `GET /mail/scheduled?accountId=`: the user's pending and failed scheduled mail.
pub async fn list(State(s): State<AppState>, a: Auth, q: Qs) -> Result<Response> {
    let account = query_string(&q, "accountId").filter(|v| is_uuid(v));
    let rows = s
        .db
        .json_rows(
            &format!(
                "SELECT {SUMMARY} FROM scheduled_sends WHERE user_id = $1 AND ($2::uuid IS NULL OR account_id = $2::uuid) \
                 ORDER BY send_at ASC"
            ),
            params![&a.user_id, account],
        )
        .await?;
    Ok(ok(json!({ "scheduled": rows })))
}

/// `GET /mail/scheduled/:id`: one scheduled mail with its body, for the preview.
pub async fn detail(State(s): State<AppState>, a: Auth, Path(id): Path<String>, q: Qs) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(404, "Scheduled mail not found"));
    }
    // `?full=1` adds the whole stored request (attachments included) for editing in the composer.
    let full = if query_string(&q, "full").as_deref() == Some("1") { ", payload" } else { "" };
    let row = s
        .db
        .json_opt(
            &format!(
                "SELECT {SUMMARY}, payload->>'body' AS body, COALESCE((payload->>'bodyIsHtml')::boolean, false) AS body_is_html{full} \
                 FROM scheduled_sends WHERE id = $1 AND user_id = $2"
            ),
            params![&id, &a.user_id],
        )
        .await?;
    Ok(match row {
        Some(r) => ok(Value::Object(r)),
        None => err(404, "Scheduled mail not found"),
    })
}

/// `DELETE /mail/scheduled/:id`: cancel a scheduled (or discard a failed) send.
pub async fn cancel(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(404, "Scheduled mail not found"));
    }
    let row =
        s.db.opt(
            "DELETE FROM scheduled_sends WHERE id = $1 AND user_id = $2 AND status <> 'sending' RETURNING account_id",
            params![&id, &a.user_id],
        )
        .await?;
    let Some(row) = row else {
        let sending =
            s.db.opt("SELECT 1 FROM scheduled_sends WHERE id = $1 AND user_id = $2", params![&id, &a.user_id])
                .await?
                .is_some();
        return Ok(if sending {
            err(409, "This message is being sent right now")
        } else {
            err(404, "Scheduled mail not found")
        });
    };
    let account: uuid::Uuid = row.get("account_id");
    notify(&s, &a.user_id, &account.to_string(), false);
    Ok(ok(json!({ "ok": true })))
}

/// The error text of a failed send response.
async fn error_text(res: Response) -> String {
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap_or_default();
    serde_json::from_slice::<Map<String, Value>>(&body)
        .ok()
        .and_then(|m| m.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| format!("Sending failed (HTTP {})", status.as_u16()))
}

async fn send_due(s: &AppState) -> Result<()> {
    let due =
        s.db.rows(
            "UPDATE scheduled_sends SET status = 'sending' WHERE id IN ( \
               SELECT id FROM scheduled_sends WHERE status = 'scheduled' AND send_at <= NOW() \
               ORDER BY send_at LIMIT 5 FOR UPDATE SKIP LOCKED) \
             RETURNING id, user_id, account_id, payload, revision",
            params![],
        )
        .await?;
    for row in due {
        let id: uuid::Uuid = row.get("id");
        let user = row.get::<_, uuid::Uuid>("user_id").to_string();
        let account = row.get::<_, uuid::Uuid>("account_id").to_string();
        let payload: Value = row.get("payload");
        let revision: i32 = row.get("revision");
        let key = if revision == 0 { format!("scheduled:{id}") } else { format!("scheduled:{id}:{revision}") };
        let outcome = compose::send_as(s, user.clone(), Some(key), Body(payload)).await;
        let failure = match outcome {
            Ok(res) if res.status().is_success() => None,
            Ok(res) => Some(error_text(res).await),
            Err(e) => Some(e.to_string()),
        };
        match &failure {
            None => {
                tracing::info!("Scheduled mail {id} sent");
                s.db.exec("DELETE FROM scheduled_sends WHERE id = $1", params![id]).await?;
            }
            Some(e) => {
                tracing::warn!("Scheduled mail {id} failed: {e}");
                s.db.exec(
                    "UPDATE scheduled_sends SET status = 'failed', last_error = $2 WHERE id = $1",
                    params![id, e.chars().take(500).collect::<String>()],
                )
                .await?;
            }
        }
        notify(s, &user, &account, failure.is_none());
    }
    Ok(())
}

/// How long to sleep before the next scheduled mail is due (at most [`MAX_SLEEP`]).
async fn until_next(s: &AppState) -> Duration {
    let next =
        s.db.opt("SELECT MIN(send_at) AS next FROM scheduled_sends WHERE status = 'scheduled'", params![])
            .await
            .ok()
            .flatten()
            .and_then(|r| r.get::<_, Option<DateTime<Utc>>>("next"));
    match next {
        Some(at) => (at - Utc::now()).to_std().unwrap_or(Duration::ZERO).min(MAX_SLEEP),
        None => MAX_SLEEP,
    }
}

/// Start the worker. Rows a crash left in `sending` go back to `scheduled`: the idempotency
/// key turns a send that already went out into a no-op (or an "uncertain" failure).
pub fn start_worker(s: AppState) {
    tokio::spawn(async move {
        if let Err(e) =
            s.db.exec("UPDATE scheduled_sends SET status = 'scheduled' WHERE status = 'sending'", params![]).await
        {
            tracing::warn!("Scheduled send recovery failed: {e}");
        }
        loop {
            if let Err(e) = send_due(&s).await {
                tracing::warn!("Scheduled send tick failed: {e}");
            }
            // At least a short pause, so a failing query cannot spin.
            let wait = until_next(&s).await.max(Duration::from_millis(250));
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = WAKE.notified() => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_non_empty_recipients() {
        assert_eq!(recipients(Some(&json!(["a@b.c", " ", "d@e.f"]))), 2);
        assert_eq!(recipients(Some(&json!("a@b.c"))), 1);
        assert_eq!(recipients(Some(&json!(""))), 0);
        assert_eq!(recipients(None), 0);
    }
}
