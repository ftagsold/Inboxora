//! `/api/push` (port of routes/push.ts): device registry (session auth) and the native
//! background API authenticated by device tokens.

use axum::Router;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::header;
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::get;
use inboxora_core::pg::Q;
use inboxora_core::{Error, Result, params};
use serde_json::{Map, Value, json};

use crate::http::{Auth, Body};
use crate::routes::util::{err, json, ok};
use crate::services::push;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/push/devices", get(list).post(register).delete(remove_all))
        .route("/api/push/devices/{device_id}", axum::routing::delete(remove_one))
        .route("/api/push/status", get(status))
        .route("/api/push/native/messages/{id}", get(native_message))
        .route("/api/push/native/inbox", get(native_inbox))
}

/// `requireDeviceAuth`.
pub struct DeviceAuth(pub push::Device);

impl FromRequestParts<AppState> for DeviceAuth {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let token = push::bearer(parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()));
        let device = match token {
            Some(t) => push::authenticate(&state.db, &t).await?,
            None => None,
        };
        device.map(DeviceAuth).ok_or_else(Error::unauthorized)
    }
}

fn device_json(d: &Map<String, Value>, disabled: Option<bool>) -> Value {
    let mut m = Map::new();
    for (k, src) in [
        ("id", "id"),
        ("deviceId", "device_id"),
        ("platform", "platform"),
        ("transport", "transport"),
        ("appVersion", "app_version"),
        ("createdAt", "created_at"),
        ("updatedAt", "updated_at"),
        ("lastSeen", "last_seen"),
    ] {
        m.insert(k.into(), d.get(src).cloned().unwrap_or(Value::Null));
    }
    if let Some(dis) = disabled {
        m.insert("disabled".into(), Value::Bool(dis));
    }
    Value::Object(m)
}

async fn list(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let rows = s
        .db
        .json_rows(
            "SELECT id, device_id, platform, transport, app_version, created_at, updated_at, last_seen, disabled_at FROM push_devices \
             WHERE user_id = $1 ORDER BY created_at DESC",
            params![&auth.user_id],
        )
        .await?;
    let devices: Vec<Value> = rows
        .iter()
        .filter_map(Value::as_object)
        .map(|d| device_json(d, Some(d.get("disabled_at").is_some_and(|v| !v.is_null()))))
        .collect();
    Ok(ok(json!({ "devices": devices })))
}

async fn register(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let input = if body.0.is_object() { body.0.clone() } else { json!({}) };
    let transport = input
        .get("transport")
        .map(inboxora_mail::text::js_string)
        .filter(|t| !t.is_empty() && t != "null" && t != "false" && t != "0")
        .unwrap_or_default();
    if transport.to_lowercase() == "unifiedpush" {
        let endpoint = input
            .get("endpoint")
            .filter(|v| inboxora_core::json::truthy(Some(v)))
            .map(inboxora_mail::text::js_string)
            .unwrap_or_default();
        let Ok(url) = url::Url::parse(&endpoint) else {
            return Ok(err(400, "UnifiedPush endpoint must be a valid URL"));
        };
        let allow_private = push::allow_private_endpoints();
        if url.scheme() != "https" && !(allow_private && url.scheme() == "http") {
            return Ok(err(400, "UnifiedPush endpoint must use HTTPS"));
        }
        if inboxora_net::validate_host(url.host_str().unwrap_or(""), allow_private).await.is_some() {
            return Ok(err(400, "UnifiedPush endpoint host is not allowed"));
        }
    }
    let reg = match push::validate_registration(&input) {
        Ok(r) => r,
        Err(m) => return Ok(err(400, m)),
    };
    let (device, token) = push::register(&s.db, &auth.user_id, reg).await?;
    let db = s.db.clone();
    tokio::spawn(async move {
        let _ = push::prune_stale(&db).await;
    });
    Ok(json(201, json!({ "device": device_json(&device, None), "deviceToken": token })))
}

async fn remove_all(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let n = s.db.exec("DELETE FROM push_devices WHERE user_id = $1", params![&auth.user_id]).await?;
    Ok(ok(json!({ "ok": true, "removed": n })))
}

async fn remove_one(State(s): State<AppState>, auth: Auth, Path(device_id): Path<String>) -> Result<Response> {
    let id = inboxora_mail::text::js_substring(&device_id, 128);
    let row =
        s.db.json_opt(
            "DELETE FROM push_devices WHERE user_id = $1 AND device_id = $2 RETURNING id, device_id, transport",
            params![&auth.user_id, &id],
        )
        .await?;
    Ok(match row {
        Some(d) => ok(json!({ "ok": true, "device": { "id": d.get("id"), "deviceId": d.get("device_id") } })),
        None => err(404, "Push device not found"),
    })
}

async fn status(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let row = s
        .db
        .json_opt(
            "SELECT COUNT(*)::int AS total, COUNT(*) FILTER (WHERE disabled_at IS NULL)::int AS active FROM push_devices WHERE user_id = $1",
            params![&auth.user_id],
        )
        .await?;
    Ok(ok(json!({
        "webPushConfigured": push::web_push_configured(),
        "nativeTransports": { "unifiedpush": true, "fcm": push::fcm_configured() },
        "pushBaseUrl": push::push_base_url(),
        "devices": row.map_or_else(|| json!({ "total": 0, "active": 0 }), Value::Object),
    })))
}

const UNREAD_TOTAL: &str = "SELECT COUNT(*)::int AS total FROM messages m JOIN email_accounts a ON a.id = m.account_id \
    WHERE a.user_id = $1 AND a.enabled = true AND m.folder = 'INBOX' AND m.is_read = false AND m.is_deleted = false";

fn summary(r: &Map<String, Value>) -> Value {
    let pick = |k: &str| r.get(k).filter(|v| inboxora_core::json::truthy(Some(v))).cloned();
    json!({
        "messageId": r.get("id"),
        "accountId": r.get("account_id"),
        "folder": r.get("folder"),
        "title": pick("from_name").or_else(|| pick("from_email")).unwrap_or_else(|| json!("New mail")),
        "body": pick("subject").unwrap_or_else(|| json!("(no subject)")),
    })
}

async fn unread(s: &AppState, user: &str) -> Result<Value> {
    Ok(s.db.json_opt(UNREAD_TOTAL, params![user]).await?.and_then(|r| r.get("total").cloned()).unwrap_or(json!(0)))
}

async fn native_message(
    State(s): State<AppState>,
    DeviceAuth(d): DeviceAuth,
    Path(id): Path<String>,
) -> Result<Response> {
    if !inboxora_mail::util::is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let row = s
        .db
        .json_opt(
            "SELECT m.id, m.subject, m.from_name, m.from_email, m.account_id, m.folder FROM messages m JOIN email_accounts a ON a.id = m.account_id \
             WHERE m.id = $1 AND a.user_id = $2 AND m.is_deleted = false",
            params![&id, &d.user_id],
        )
        .await?;
    let Some(row) = row else { return Ok(err(404, "Message not found")) };
    Ok(ok(json!({ "eventId": id, "message": summary(&row), "unreadCount": unread(&s, &d.user_id).await? })))
}

async fn native_inbox(State(s): State<AppState>, DeviceAuth(d): DeviceAuth) -> Result<Response> {
    let row = s
        .db
        .json_opt(
            "SELECT m.id, m.subject, m.from_name, m.from_email, m.account_id, m.folder FROM messages m JOIN email_accounts a ON a.id = m.account_id \
             WHERE a.user_id = $1 AND a.enabled = true AND m.folder = 'INBOX' AND m.is_read = false AND m.is_deleted = false \
             ORDER BY m.date DESC NULLS LAST, m.id DESC LIMIT 1",
            params![&d.user_id],
        )
        .await?;
    let unread_count = unread(&s, &d.user_id).await?;
    Ok(ok(json!({
        "message": row.as_ref().map(summary),
        "eventId": row.as_ref().and_then(|r| r.get("id").cloned()),
        "unreadCount": unread_count,
    })))
}
