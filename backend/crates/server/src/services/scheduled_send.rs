//! Scheduled send: `POST /mail/send` with `sendAt` stores the request in `scheduled_sends`,
//! and a worker sends it through the regular send path once it is due. Each send uses the
//! idempotency key `scheduled:<id>`, so a crash mid-send never dispatches the mail twice. The
//! list/detail/cancel routes back the scheduled rows the web app shows in the Sent folder.

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

/// How often the worker looks for due mail.
const TICK: Duration = Duration::from_secs(15);
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

fn notify(s: &AppState, user: &str, account_id: &str) {
    s.mail.broadcast(json!({ "type": "scheduled_changed", "accountId": account_id }), Some(user));
}

fn recipients(v: Option<&Value>) -> usize {
    match v {
        Some(Value::Array(a)) => a.iter().filter(|x| x.as_str().is_some_and(|s| !s.trim().is_empty())).count(),
        Some(Value::String(s)) => usize::from(!s.trim().is_empty()),
        _ => 0,
    }
}

/// `POST /mail/send` with `sendAt`: validate and store the request.
pub async fn schedule(s: &AppState, user: &str, mut payload: Value) -> Result<Response> {
    let Some(obj) = payload.as_object_mut() else { return Ok(err(400, "Invalid request body")) };
    let Some(send_at) = obj
        .remove("sendAt")
        .and_then(|v| v.as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()))
        .map(|t| t.with_timezone(&Utc))
    else {
        return Ok(err(400, "sendAt must be an ISO 8601 date-time"));
    };
    let now = Utc::now();
    if send_at < now + chrono::Duration::seconds(MIN_LEAD_SECS) {
        return Ok(err(400, "The scheduled time must be in the future"));
    }
    if send_at > now + chrono::Duration::days(MAX_AHEAD_DAYS) {
        return Ok(err(400, "Mail can be scheduled at most one year ahead"));
    }
    let Some(account_id) = obj.get("accountId").and_then(Value::as_str).filter(|v| is_uuid(v)).map(str::to_owned)
    else {
        return Ok(err(400, "accountId required"));
    };
    if recipients(obj.get("to")) + recipients(obj.get("cc")) + recipients(obj.get("bcc")) == 0 {
        return Ok(err(400, "At least one recipient is required"));
    }
    if account_for_user(s, &account_id, user).await?.is_none() {
        return Ok(err(404, "Account not found"));
    }
    if payload.to_string().len() > MAX_PAYLOAD_BYTES {
        return Ok(err(400, "Total attachment size exceeds 25 MB"));
    }
    let row =
        s.db.opt(
            "INSERT INTO scheduled_sends (user_id, account_id, send_at, payload) VALUES ($1, $2, $3, $4) RETURNING id",
            params![user, &account_id, send_at, payload],
        )
        .await?
        .ok_or_else(|| inboxora_core::Error::Internal("scheduled send insert returned no row".into()))?;
    let id: uuid::Uuid = row.get("id");
    notify(s, user, &account_id);
    Ok(ok(json!({
        "ok": true,
        "scheduled": { "id": id.to_string(), "sendAt": inboxora_core::json::js_iso(&send_at) },
    })))
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
pub async fn detail(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(404, "Scheduled mail not found"));
    }
    let row = s
        .db
        .json_opt(
            &format!(
                "SELECT {SUMMARY}, payload->>'body' AS body, COALESCE((payload->>'bodyIsHtml')::boolean, false) AS body_is_html \
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
    notify(&s, &a.user_id, &account.to_string());
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
             RETURNING id, user_id, account_id, payload",
            params![],
        )
        .await?;
    for row in due {
        let id: uuid::Uuid = row.get("id");
        let user = row.get::<_, uuid::Uuid>("user_id").to_string();
        let account = row.get::<_, uuid::Uuid>("account_id").to_string();
        let payload: Value = row.get("payload");
        let outcome = compose::send_as(s, user.clone(), Some(format!("scheduled:{id}")), Body(payload)).await;
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
        notify(s, &user, &account);
    }
    Ok(())
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
        let mut iv = tokio::time::interval(TICK);
        loop {
            iv.tick().await;
            if let Err(e) = send_due(&s).await {
                tracing::warn!("Scheduled send tick failed: {e}");
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
