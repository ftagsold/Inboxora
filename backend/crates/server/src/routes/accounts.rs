//! `/api/accounts`: mail accounts, aliases, folders and reindex (port of `routes/accounts.ts`).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post, put};
use inboxora_core::config::env;
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Error, Result, crypto, params};
use inboxora_mail::sanitize::sanitize_signature;
use serde_json::{Map, Value, json};

use crate::engine::connect::{Account, load_account};
use crate::http::{Auth, Body};
use crate::routes::util::{err, ok};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{id}", put(update).delete(remove))
        .route("/{id}/reconnect", post(reconnect))
        .route("/{id}/aliases", get(list_aliases).post(create_alias))
        .route("/{id}/aliases/{alias_id}", put(update_alias).delete(delete_alias))
        .route("/{id}/folders", get(folders))
        .route("/{id}/reindex", post(reindex))
}

const SAFE_FIELDS: [&str; 24] = [
    "id",
    "name",
    "sender_name",
    "email_address",
    "color",
    "protocol",
    "imap_host",
    "imap_port",
    "imap_skip_tls_verify",
    "smtp_host",
    "smtp_port",
    "smtp_tls",
    "auth_user",
    "smtp_auth_user",
    "oauth_provider",
    "enabled",
    "include_in_unified_inbox",
    "last_sync",
    "sync_error",
    "sort_order",
    "folder_mappings",
    "signature",
    "created_at",
    "categorization_enabled",
];

/// Per-account FIFO for reconnect side effects (`createKeyedSerializer`).
static RECONNECT: LazyLock<parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

fn serialize_reconnect(id: String, work: impl Future<Output = ()> + Send + 'static) {
    let lock = RECONNECT.lock().entry(id).or_default().clone();
    tokio::spawn(async move {
        let _g = lock.lock().await;
        work.await;
    });
}

fn uuid_check(name: &str, v: &str) -> Option<Response> {
    (!inboxora_mail::util::is_uuid(v)).then(|| err(400, &format!("Invalid {name}")))
}

fn sig(v: &Value) -> Value {
    match v {
        Value::String(s) if !s.is_empty() => Value::String(sanitize_signature(s)),
        other => other.clone(),
    }
}

/// `sanitizeSignature(x) || null`.
fn sig_or_null(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) if !s.is_empty() => {
            let out = sanitize_signature(s);
            if out.is_empty() { Value::Null } else { Value::String(out) }
        }
        Some(v) if inboxora_core::json::truthy(Some(v)) => v.clone(),
        _ => Value::Null,
    }
}

fn safe_account(row: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for k in SAFE_FIELDS {
        out.insert(k.into(), row.get(k).cloned().unwrap_or(Value::Null));
    }
    if let Some(s) = out.get("signature").cloned() {
        out.insert("signature".into(), sig(&s));
    }
    Value::Object(out)
}

fn has_injection(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(|s| s.contains(['\r', '\n', '\0']))
}

/// `encrypt(x)`: falsy values pass through unchanged.
fn enc(v: Option<&Value>) -> Result<Val> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Ok(Val::Str(crypto::encrypt(s).map_err(Error::internal)?)),
        Some(v) if inboxora_core::json::truthy(Some(v)) => Ok(Val::from(v)),
        Some(v) => Ok(Val::from(v)),
        None => Ok(Val::Null),
    }
}

fn js_number(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) if s.trim().is_empty() => 0.0,
        Value::String(s) => s.trim().parse().unwrap_or(f64::NAN),
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Null => 0.0,
        _ => f64::NAN,
    }
}

fn validate_port(port: &Value, allowed: &[u16]) -> Option<String> {
    let n = js_number(port);
    let shown = inboxora_mail::text::js_string(port);
    if n.fract() != 0.0 || !n.is_finite() || !(1.0..=65535.0).contains(&n) {
        return Some(format!("Port {shown} is not a valid port number"));
    }
    if env("ALLOW_PRIVATE_IMAP_HOSTS").as_deref() == Some("true") {
        return None;
    }
    if !allowed.contains(&(n as u16)) {
        return Some(format!(
            "Port {shown} is not allowed. Allowed: {}",
            allowed.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
        ));
    }
    None
}

const IMAP_PORTS: [u16; 2] = [143, 993];
const SMTP_PORTS: [u16; 2] = [465, 587];

async fn list(State(s): State<AppState>, a: Auth) -> Result<Response> {
    let rows = s
        .db
        .json_rows(
            "SELECT id, name, sender_name, email_address, color, protocol, imap_host, imap_port, imap_tls, imap_skip_tls_verify, \
                    smtp_host, smtp_port, smtp_tls, auth_user, smtp_auth_user, oauth_provider, enabled, include_in_unified_inbox, \
                    last_sync, sync_error, sort_order, folder_mappings, signature, created_at, categorization_enabled \
             FROM email_accounts WHERE user_id = $1 ORDER BY sort_order, created_at",
            params![&a.user_id],
        )
        .await?;
    let ids: Vec<String> = rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_owned)).collect();
    let mut aliases: HashMap<String, Vec<Value>> = HashMap::new();
    if !ids.is_empty() {
        for alias in s
            .db
            .json_rows(
                "SELECT id, account_id, name, email, reply_to, signature, created_at FROM account_aliases WHERE account_id = ANY($1) ORDER BY created_at",
                params![ids],
            )
            .await?
        {
            let key = alias.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
            let mut alias = alias;
            if let Some(sg) = alias.get("signature").cloned() {
                alias["signature"] = sig(&sg);
            }
            aliases.entry(key).or_default().push(alias);
        }
    }
    let out: Vec<Value> = rows
        .into_iter()
        .map(|mut r| {
            let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
            if let Some(sg) = r.get("signature").cloned() {
                r["signature"] = sig(&sg);
            }
            r["aliases"] = Value::Array(aliases.remove(&id).unwrap_or_default());
            r
        })
        .collect();
    Ok(ok(Value::Array(out)))
}

async fn create(State(s): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let b = |k: &str| body.get(k);
    let truthy = |k: &str| inboxora_core::json::truthy(b(k));
    if !truthy("name") || !truthy("email_address") {
        return Ok(err(400, "Name and email required"));
    }
    if has_injection(b("name")) || has_injection(b("email_address")) {
        return Ok(err(400, "Name and email address cannot contain control characters"));
    }
    if truthy("sender_name") && has_injection(b("sender_name")) {
        return Ok(err(400, "Sender name cannot contain control characters"));
    }
    let policy = crate::services::dav_client::connection_policy(&s.db).await?;
    if b("imap_tls").is_some_and(|v| !v.is_boolean()) {
        return Ok(err(400, "IMAP TLS must be a boolean"));
    }
    let imap_port = b("imap_port").cloned().unwrap_or(json!(993));
    let smtp_port = b("smtp_port").cloned().unwrap_or(json!(587));
    if truthy("imap_host") {
        let host = body.str("imap_host").unwrap_or_default();
        let e = match inboxora_net::validate_host(host, policy.allow_private_hosts).await {
            Some(e) => Some(e.to_owned()),
            None if !policy.allow_nonstandard_ports => validate_port(&imap_port, &IMAP_PORTS),
            None => None,
        };
        if let Some(e) = e {
            return Ok(err(400, &format!("IMAP: {e}")));
        }
    }
    if truthy("smtp_host") {
        let host = body.str("smtp_host").unwrap_or_default();
        let e = match inboxora_net::validate_host(host, policy.allow_private_hosts).await {
            Some(e) => Some(e.to_owned()),
            None if !policy.allow_nonstandard_ports => validate_port(&smtp_port, &SMTP_PORTS),
            None => None,
        };
        if let Some(e) = e {
            return Ok(err(400, &format!("SMTP: {e}")));
        }
    }
    let protocol = b("protocol").cloned().unwrap_or(json!("imap"));
    let imap_tls = match b("imap_tls") {
        Some(v) => v.clone(),
        None => json!((js_number(&imap_port) % 1000.0) == 993.0),
    };
    let nul = |k: &str| if truthy(k) { Val::from(b(k).cloned().unwrap_or(Value::Null)) } else { Val::Null };
    let run = async {
        let row = s
            .db
            .json_opt(
                "INSERT INTO email_accounts (user_id, name, sender_name, email_address, color, protocol, imap_host, imap_port, imap_tls, imap_skip_tls_verify, \
                   smtp_host, smtp_port, smtp_tls, auth_user, auth_pass, smtp_auth_user, smtp_auth_pass, oauth_provider, oauth_access_token, oauth_refresh_token, signature) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21) RETURNING *",
                &[
                    Val::from(a.user_id.as_str()),
                    Val::from(b("name").cloned().unwrap_or(Value::Null)),
                    nul("sender_name"),
                    Val::from(b("email_address").cloned().unwrap_or(Value::Null)),
                    Val::from(b("color").cloned().unwrap_or(json!("#6366f1"))),
                    Val::from(protocol.clone()),
                    Val::from(b("imap_host").cloned().unwrap_or(Value::Null)),
                    Val::from(imap_port.clone()),
                    Val::from(imap_tls),
                    Val::Bool(truthy("imap_skip_tls_verify")),
                    Val::from(b("smtp_host").cloned().unwrap_or(Value::Null)),
                    Val::from(smtp_port.clone()),
                    Val::from(b("smtp_tls").cloned().unwrap_or(json!("STARTTLS"))),
                    Val::from(b("auth_user").cloned().unwrap_or(Value::Null)),
                    enc(b("auth_pass"))?,
                    nul("smtp_auth_user"),
                    if truthy("smtp_auth_pass") { enc(b("smtp_auth_pass"))? } else { Val::Null },
                    Val::from(b("oauth_provider").cloned().unwrap_or(Value::Null)),
                    enc(b("oauth_access_token"))?,
                    enc(b("oauth_refresh_token"))?,
                    Val::from(sig_or_null(b("signature"))),
                ],
            )
            .await?
            .ok_or_else(|| Error::internal("insert returned nothing"))?;
        Ok::<_, Error>(row)
    };
    match run.await {
        Ok(row) => {
            if protocol.as_str() == Some("imap") {
                let (m, acc) = (s.mail.clone(), Account::from_map(row.clone()));
                tokio::spawn(async move {
                    m.connect_account(acc).await;
                });
            }
            Ok(ok(safe_account(&row)))
        }
        Err(e) => {
            tracing::error!("{e}");
            Ok(err(500, "Failed to add account"))
        }
    }
}

async fn update(State(s): State<AppState>, a: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    if let Some(r) = uuid_check("id", &id) {
        return Ok(r);
    }
    if s.db
        .opt("SELECT id FROM email_accounts WHERE id = $1 AND user_id = $2", params![&id, &a.user_id])
        .await?
        .is_none()
    {
        return Ok(err(404, "Account not found"));
    }
    let updates = body.0.as_object().cloned().unwrap_or_default();
    let has = |k: &str| updates.contains_key(k);
    let g = |k: &str| updates.get(k);
    if has("name") && has_injection(g("name")) {
        return Ok(err(400, "Name cannot contain control characters"));
    }
    if has("sender_name") && inboxora_core::json::truthy(g("sender_name")) && has_injection(g("sender_name")) {
        return Ok(err(400, "Sender name cannot contain control characters"));
    }
    let policy = crate::services::dav_client::connection_policy(&s.db).await?;
    if has("imap_tls") && !g("imap_tls").is_some_and(Value::is_boolean) {
        return Ok(err(400, "IMAP TLS must be a boolean"));
    }
    for (host_key, port_key, label, allowed) in
        [("imap_host", "imap_port", "IMAP", &IMAP_PORTS[..]), ("smtp_host", "smtp_port", "SMTP", &SMTP_PORTS[..])]
    {
        if inboxora_core::json::truthy(g(host_key))
            && let Some(e) = inboxora_net::validate_host(
                g(host_key).and_then(Value::as_str).unwrap_or(""),
                policy.allow_private_hosts,
            )
            .await
        {
            return Ok(err(400, &format!("{label}: {e}")));
        }
        if let Some(p) = g(port_key).filter(|p| !p.is_null())
            && !policy.allow_nonstandard_ports
            && let Some(e) = validate_port(p, allowed)
        {
            return Ok(err(400, &format!("{label}: {e}")));
        }
    }
    const ALLOWED: [&str; 20] = [
        "name",
        "sender_name",
        "color",
        "enabled",
        "include_in_unified_inbox",
        "auth_user",
        "auth_pass",
        "sort_order",
        "imap_host",
        "imap_port",
        "imap_tls",
        "imap_skip_tls_verify",
        "smtp_host",
        "smtp_port",
        "smtp_tls",
        "smtp_auth_user",
        "smtp_auth_pass",
        "folder_mappings",
        "signature",
        "categorization_enabled",
    ];
    let mut sets = Vec::new();
    let mut values: Vec<Val> = Vec::new();
    for key in ALLOWED {
        let Some(v) = g(key) else { continue };
        sets.push(format!("{key} = ${}", sets.len() + 1));
        let truthy = inboxora_core::json::truthy(Some(v));
        values.push(match key {
            "auth_pass" | "smtp_auth_pass" if truthy => enc(Some(v))?,
            "smtp_auth_user" | "smtp_auth_pass" => {
                if truthy {
                    Val::from(v)
                } else {
                    Val::Null
                }
            }
            "signature" => Val::from(sig_or_null(Some(v))),
            "include_in_unified_inbox" => Val::Bool(truthy),
            _ => Val::from(v),
        });
    }
    if sets.is_empty() {
        return Ok(err(400, "No valid fields to update"));
    }
    values.push(Val::from(id.as_str()));
    let server_of = |r: &Map<String, Value>| {
        (
            r.get("imap_host").and_then(Value::as_str).unwrap_or_default().to_lowercase(),
            r.get("imap_port").and_then(Value::as_i64),
            r.get("auth_user").and_then(Value::as_str).unwrap_or_default().to_owned(),
        )
    };
    let before =
        s.db.json_opt("SELECT imap_host, imap_port, auth_user FROM email_accounts WHERE id = $1", params![id.as_str()])
            .await?
            .map(|r| server_of(&r));
    let updated =
        s.db.json_opt(
            &format!("UPDATE email_accounts SET {} WHERE id = ${} RETURNING *", sets.join(", "), sets.len() + 1),
            &values,
        )
        .await?
        .ok_or_else(|| Error::internal("update returned nothing"))?;
    // A different server or login has its own connection limit: forget the learned one.
    if before.is_some_and(|b| b != server_of(&updated)) {
        s.db.exec(
            "UPDATE email_accounts SET imap_conn_limit = NULL, imap_conn_limit_at = NULL, imap_conn_limit_reason = NULL WHERE id = $1",
            params![id.as_str()],
        )
        .await?;
        s.mail.reload_limits(&id).await;
    }
    let disabling = has("enabled") && !inboxora_core::json::truthy(g("enabled"));
    let needs_reconnect = !disabling
        && ["enabled", "auth_user", "auth_pass", "imap_host", "imap_port", "imap_tls", "imap_skip_tls_verify"]
            .iter()
            .any(|k| has(k));
    let is_imap = updated.get("protocol").and_then(Value::as_str) == Some("imap");
    let enabled = updated.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    if disabling {
        let (m, i) = (s.mail.clone(), id.clone());
        serialize_reconnect(id.clone(), async move { m.disconnect_account(&i).await });
    } else if needs_reconnect && is_imap && enabled {
        let (m, db, i) = (s.mail.clone(), s.db.clone(), id.clone());
        serialize_reconnect(id.clone(), async move {
            m.disconnect_account(&i).await;
            if let Ok(Some(acc)) = load_account(&db, &i).await {
                m.connect_account(acc).await;
            }
        });
    }
    Ok(ok(safe_account(&updated)))
}

async fn remove(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = uuid_check("id", &id) {
        return Ok(r);
    }
    if s.db
        .opt("SELECT id FROM email_accounts WHERE id = $1 AND user_id = $2", params![&id, &a.user_id])
        .await?
        .is_none()
    {
        return Ok(err(404, "Account not found"));
    }
    match s.db.exec("DELETE FROM email_accounts WHERE id = $1", params![&id]).await {
        Ok(_) => {
            let m = s.mail.clone();
            tokio::spawn(async move { m.disconnect_account(&id).await });
            Ok(ok(json!({ "ok": true })))
        }
        Err(e) if e.pg_code() == Some("23503") => Ok(err(
            409,
            "This account is still used to send calendar invitations. Cancel or transfer those invitations before deleting it.",
        )),
        Err(e) => {
            tracing::error!("Account delete error: {e}");
            Ok(err(500, "Failed to delete account"))
        }
    }
}

async fn reconnect(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = uuid_check("id", &id) {
        return Ok(r);
    }
    let Some(row) =
        s.db.json_opt("SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2", params![&id, &a.user_id]).await?
    else {
        return Ok(err(404, "Account not found"));
    };
    let m = s.mail.clone();
    tokio::spawn(async move {
        m.connect_account(Account::from_map(row)).await;
    });
    Ok(ok(json!({ "ok": true })))
}

async fn owned(s: &AppState, id: &str, user: &str) -> Result<bool> {
    Ok(s.db.opt("SELECT id FROM email_accounts WHERE id = $1 AND user_id = $2", params![id, user]).await?.is_some())
}

async fn list_aliases(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = uuid_check("id", &id) {
        return Ok(r);
    }
    if !owned(&s, &id, &a.user_id).await? {
        return Ok(err(404, "Account not found"));
    }
    let rows = s
        .db
        .json_rows(
            "SELECT id, account_id, name, email, reply_to, signature, created_at FROM account_aliases WHERE account_id = $1 ORDER BY created_at",
            params![&id],
        )
        .await?;
    Ok(ok(Value::Array(
        rows.into_iter()
            .map(|mut r| {
                if let Some(sg) = r.get("signature").cloned() {
                    r["signature"] = sig(&sg);
                }
                r
            })
            .collect(),
    )))
}

fn alias_input(body: &Body) -> std::result::Result<(), Response> {
    let t = |k: &str| inboxora_core::json::truthy(body.get(k));
    if !t("name") || !t("email") {
        return Err(err(400, "Name and email required"));
    }
    if has_injection(body.get("name")) || has_injection(body.get("email")) || has_injection(body.get("reply_to")) {
        return Err(err(400, "Fields cannot contain control characters"));
    }
    Ok(())
}

fn reply_to(body: &Body) -> Val {
    if inboxora_core::json::truthy(body.get("reply_to")) {
        Val::from(body.get("reply_to").cloned().unwrap_or(Value::Null))
    } else {
        Val::Null
    }
}

async fn create_alias(State(s): State<AppState>, a: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    if let Some(r) = uuid_check("id", &id) {
        return Ok(r);
    }
    if let Err(r) = alias_input(&body) {
        return Ok(r);
    }
    if !owned(&s, &id, &a.user_id).await? {
        return Ok(err(404, "Account not found"));
    }
    let row = s
        .db
        .json_opt(
            "INSERT INTO account_aliases (account_id, name, email, reply_to, signature) VALUES ($1, $2, $3, $4, $5) RETURNING *",
            &[
                Val::from(id.as_str()),
                Val::from(body.get("name").cloned().unwrap_or(Value::Null)),
                Val::from(body.get("email").cloned().unwrap_or(Value::Null)),
                reply_to(&body),
                Val::from(sig_or_null(body.get("signature"))),
            ],
        )
        .await?;
    Ok(ok(row.map_or(Value::Null, Value::Object)))
}

async fn alias_owner(s: &AppState, alias_id: &str, user: &str, id: &str) -> Result<bool> {
    Ok(s
        .db
        .opt(
            "SELECT a.id, a.account_id FROM account_aliases a JOIN email_accounts e ON a.account_id = e.id WHERE a.id = $1 AND e.user_id = $2 AND e.id = $3",
            params![alias_id, user, id],
        )
        .await?
        .is_some())
}

async fn update_alias(
    State(s): State<AppState>,
    a: Auth,
    Path((id, alias_id)): Path<(String, String)>,
    body: Body,
) -> Result<Response> {
    if let Some(r) = uuid_check("id", &id).or_else(|| uuid_check("aliasId", &alias_id)) {
        return Ok(r);
    }
    if let Err(r) = alias_input(&body) {
        return Ok(r);
    }
    if !alias_owner(&s, &alias_id, &a.user_id, &id).await? {
        return Ok(err(404, "Alias not found"));
    }
    let row =
        s.db.json_opt(
            "UPDATE account_aliases SET name = $1, email = $2, reply_to = $3, signature = $4 WHERE id = $5 RETURNING *",
            &[
                Val::from(body.get("name").cloned().unwrap_or(Value::Null)),
                Val::from(body.get("email").cloned().unwrap_or(Value::Null)),
                reply_to(&body),
                Val::from(sig_or_null(body.get("signature"))),
                Val::from(alias_id.as_str()),
            ],
        )
        .await?;
    Ok(ok(row.map_or(Value::Null, Value::Object)))
}

async fn delete_alias(
    State(s): State<AppState>,
    a: Auth,
    Path((id, alias_id)): Path<(String, String)>,
) -> Result<Response> {
    if let Some(r) = uuid_check("id", &id).or_else(|| uuid_check("aliasId", &alias_id)) {
        return Ok(r);
    }
    if !alias_owner(&s, &alias_id, &a.user_id, &id).await? {
        return Ok(err(404, "Alias not found"));
    }
    s.db.exec("DELETE FROM account_aliases WHERE id = $1", params![&alias_id]).await?;
    Ok(ok(json!({ "ok": true })))
}

async fn folders(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = uuid_check("id", &id) {
        return Ok(r);
    }
    if !owned(&s, &id, &a.user_id).await? {
        return Ok(err(404, "Account not found"));
    }
    Ok(ok(Value::Array(
        s.db.json_rows("SELECT * FROM folders WHERE account_id = $1 ORDER BY path", params![&id]).await?,
    )))
}

async fn reindex(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = uuid_check("id", &id) {
        return Ok(r);
    }
    let row =
        s.db.json_opt(
            "SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2 AND enabled = true AND protocol = 'imap'",
            params![&id, &a.user_id],
        )
        .await;
    let row = match row {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("POST /accounts/:id/reindex error: {e}");
            return Ok(err(500, "Failed to start reindex"));
        }
    };
    let Some(row) = row else { return Ok(err(404, "Account not found")) };
    let already = s.mail.st.lock().backfill_all_running.contains(&id);
    if !already {
        let (m, acc) = (s.mail.clone(), Account::from_map(row));
        tokio::spawn(async move { m.backfill_all_folders(&acc).await });
    }
    Ok(ok(json!({ "ok": true, "alreadyRunning": already })))
}
