//! `/api/admin/imap-limits`: per IMAP server start value and auto-lower switch, the learned
//! per-account limits and live connection numbers, and resets (after the server's connection
//! settings change). Mounted under the admin router, so every route requires an admin.

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, patch, post};
use inboxora_core::pg::Q;
use inboxora_core::{Result, params};
use serde_json::{Value, json};

use super::util::{err, ok};
use crate::engine::limits::{LIMIT_FLOOR, LIMIT_MAX};
use crate::http::{Admin, Body};
use crate::services::auth::log_auth_event;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/imap-limits", get(list))
        .route("/imap-limits/servers/{host}/{port}", patch(update_server))
        .route("/imap-limits/servers/{host}/{port}/reset", post(reset_server))
        .route("/imap-limits/accounts/{id}/reset", post(reset_account))
}

fn valid_host(host: &str) -> bool {
    !host.is_empty() && host.len() <= 255 && host.chars().all(|c| c.is_ascii_alphanumeric() || ".-_:[]".contains(c))
}

/// Every IMAP server in use with its setting and accounts.
async fn list(State(s): State<AppState>, _a: Admin) -> Result<Response> {
    let accounts = s
        .db
        .json_rows(
            "SELECT e.id, e.email_address, lower(e.imap_host) AS host, COALESCE(e.imap_port, 993) AS port, \
               e.imap_conn_limit, e.imap_conn_limit_at, e.imap_conn_limit_reason, e.enabled, u.username AS owner \
             FROM email_accounts e JOIN users u ON u.id = e.user_id \
             WHERE e.protocol = 'imap' AND COALESCE(e.imap_host, '') <> '' ORDER BY lower(e.imap_host), e.email_address",
            params![],
        )
        .await?;
    let settings =
        s.db.json_rows("SELECT host, port, start_limit, auto_lower, updated_at FROM imap_server_limits", params![])
            .await?;
    let default = s.mail.default_conn_limit();
    let mut servers: Vec<Value> = Vec::new();
    for acc in accounts {
        let host = acc.get("host").and_then(Value::as_str).unwrap_or_default().to_owned();
        let port = acc.get("port").and_then(Value::as_i64).unwrap_or(993);
        let idx = match servers.iter().position(|v| v["host"] == json!(host) && v["port"] == json!(port)) {
            Some(i) => i,
            None => {
                let setting = settings
                    .iter()
                    .find(|r| r.get("host") == Some(&json!(host)) && r.get("port") == Some(&json!(port)));
                let start = setting.and_then(|r| r.get("start_limit")).filter(|v| !v.is_null()).cloned();
                servers.push(json!({
                    "host": host,
                    "port": port,
                    "startLimit": start,
                    "effectiveStartLimit": start.as_ref().and_then(Value::as_i64).map_or(default as i64, |v| v),
                    "autoLower": setting.and_then(|r| r.get("auto_lower")).and_then(Value::as_bool).unwrap_or(true),
                    "updatedAt": setting.and_then(|r| r.get("updated_at")).cloned(),
                    "accounts": [],
                }));
                servers.len() - 1
            }
        };
        let id = acc.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let live = s.mail.limit_snapshot(&id);
        if let Some(list) = servers[idx]["accounts"].as_array_mut() {
            list.push(json!({
                "id": id,
                "email": acc.get("email_address"),
                "owner": acc.get("owner"),
                "enabled": acc.get("enabled"),
                "learnedLimit": acc.get("imap_conn_limit"),
                "learnedAt": acc.get("imap_conn_limit_at"),
                "learnedReason": acc.get("imap_conn_limit_reason"),
                "live": live,
            }));
        }
    }
    Ok(ok(json!({ "defaultStartLimit": default, "min": LIMIT_FLOOR, "max": LIMIT_MAX, "servers": servers })))
}

async fn accounts_on(s: &AppState, host: &str, port: i64) -> Result<Vec<String>> {
    Ok(s.db
        .rows(
            "SELECT id FROM email_accounts WHERE protocol = 'imap' AND lower(imap_host) = $1 AND COALESCE(imap_port, 993) = $2",
            params![host, port as i32],
        )
        .await?
        .iter()
        .map(|r| r.get::<_, uuid::Uuid>("id").to_string())
        .collect())
}

/// Save a server's start value (`null` = the default) and auto-lower switch; applies live.
async fn update_server(
    State(s): State<AppState>,
    a: Admin,
    Path((host, port)): Path<(String, i64)>,
    body: Body,
) -> Result<Response> {
    let host = host.to_lowercase();
    if !valid_host(&host) || !(1..=65535).contains(&port) {
        return Ok(err(400, "Invalid server"));
    }
    let start = match body.get("startLimit") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_i64().filter(|n| (LIMIT_FLOOR as i64..=LIMIT_MAX as i64).contains(n)) {
            Some(n) => Some(n as i32),
            None => return Ok(err(400, &format!("startLimit must be between {LIMIT_FLOOR} and {LIMIT_MAX}"))),
        },
    };
    let auto_lower = match body.get("autoLower") {
        None => true,
        Some(v) => match v.as_bool() {
            Some(b) => b,
            None => return Ok(err(400, "autoLower must be a boolean")),
        },
    };
    s.db.exec(
        "INSERT INTO imap_server_limits (host, port, start_limit, auto_lower, updated_at, updated_by) VALUES ($1, $2, $3::int, $4, NOW(), $5) \
         ON CONFLICT (host, port) DO UPDATE SET start_limit = $3::int, auto_lower = $4, updated_at = NOW(), updated_by = $5",
        params![&host, port as i32, start, auto_lower, &a.user_id],
    )
    .await?;
    for id in accounts_on(&s, &host, port).await? {
        s.mail.reload_limits(&id).await;
    }
    log_auth_event(&s.db, "admin_imap_limit_update", Some(&format!("{host}:{port}")), Some(&a.user_id), None, true);
    Ok(ok(json!({ "success": true })))
}

/// Forget the learned limits of every account on a server (after its config changed).
async fn reset_server(
    State(s): State<AppState>,
    a: Admin,
    Path((host, port)): Path<(String, i64)>,
) -> Result<Response> {
    let host = host.to_lowercase();
    if !valid_host(&host) || !(1..=65535).contains(&port) {
        return Ok(err(400, "Invalid server"));
    }
    let ids = accounts_on(&s, &host, port).await?;
    s.db.exec(
        "UPDATE email_accounts SET imap_conn_limit = NULL, imap_conn_limit_at = NULL, imap_conn_limit_reason = NULL \
         WHERE protocol = 'imap' AND lower(imap_host) = $1 AND COALESCE(imap_port, 993) = $2",
        params![&host, port as i32],
    )
    .await?;
    for id in &ids {
        s.mail.reload_limits(id).await;
    }
    log_auth_event(&s.db, "admin_imap_limit_reset", Some(&format!("{host}:{port}")), Some(&a.user_id), None, true);
    Ok(ok(json!({ "success": true, "accounts": ids.len() })))
}

async fn reset_account(State(s): State<AppState>, a: Admin, Path(id): Path<String>) -> Result<Response> {
    if !inboxora_mail::util::is_uuid(&id) {
        return Ok(err(400, "Invalid id"));
    }
    let n = s
        .db
        .exec(
            "UPDATE email_accounts SET imap_conn_limit = NULL, imap_conn_limit_at = NULL, imap_conn_limit_reason = NULL WHERE id = $1",
            params![&id],
        )
        .await?;
    if n == 0 {
        return Ok(err(404, "Account not found"));
    }
    s.mail.reload_limits(&id).await;
    log_auth_event(&s.db, "admin_imap_limit_reset", Some(&id), Some(&a.user_id), None, true);
    Ok(ok(json!({ "success": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_validation() {
        assert!(valid_host("imap.tagsold.io"));
        assert!(valid_host("[::1]"));
        assert!(!valid_host(""));
        assert!(!valid_host("imap host"));
        assert!(!valid_host("a/b"));
    }
}
