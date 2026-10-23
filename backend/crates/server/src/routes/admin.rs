//! `/api/admin`: users, system settings, auth events, invites, system email and OIDC
//! providers (port of `routes/admin.ts`). The whole prefix is behind `requireAdmin`.

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{delete, get, patch, post};
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Error, Result, crypto, params};
use serde_json::{Map, Value, json};

use super::util::{err, ok};
use crate::http::{Admin, Body, Qs};
use crate::services::dav_sync::{ADDRESS_BOOKS, CALENDARS};
use crate::services::dav_xml::js_string;
use crate::services::{auth as auth_svc, dav_client, oidc, smtp};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users", get(list_users))
        .route("/users/{id}/totp/disable", post(disable_user_totp))
        .route("/users/{id}", patch(update_user).delete(delete_user))
        .route("/settings", get(get_settings).patch(patch_settings))
        .route("/auth-events", get(auth_events))
        .route("/invites", get(list_invites).post(create_invite))
        .route("/invites/{id}", delete(delete_invite))
        .route("/system-email", get(get_system_email).post(save_system_email).delete(delete_system_email))
        .route("/system-email/test", post(test_system_email))
        .route("/oidc", get(list_oidc).post(create_oidc))
        .route("/oidc/{id}", patch(update_oidc).delete(delete_oidc))
        .merge(super::ai::admin_router())
        .merge(super::imap_limits::router())
}

fn truthy(v: Option<&Value>) -> bool {
    inboxora_core::json::truthy(v)
}

fn js_trim(s: &str) -> &str {
    inboxora_mail::text::js_trim(s)
}

/// `router.param('id', uuidParam('id'))`.
fn bad_id(id: &str) -> Option<Response> {
    (!inboxora_mail::util::is_uuid(id)).then(|| err(400, "Invalid id"))
}

/// `queryInt(value, fallback)`.
fn query_int(q: &Qs, key: &str, fallback: i64) -> i64 {
    q.first(key).and_then(|v| inboxora_pim::ical::js_parse_int(&v)).unwrap_or(fallback)
}

fn page(q: &Qs, max: i64) -> (i64, i64) {
    (query_int(q, "limit", 100).min(max), query_int(q, "offset", 0).max(0))
}

async fn total(s: &AppState, table: &str) -> Result<i64> {
    Ok(s.db.opt(&format!("SELECT COUNT(*) AS total FROM {table}"), params![]).await?.map_or(0, |r| r.get("total")))
}

// ── users ────────────────────────────────────────────────────────────────────────────

async fn list_users(State(s): State<AppState>, _a: Admin, q: Qs) -> Result<Response> {
    let (limit, offset) = page(&q, 200);
    let lim_off = [Val::from(limit), Val::from(offset)];
    let (rows, count) = tokio::join!(
        s.db.json_rows("SELECT id, username, is_admin, totp_enabled, created_at FROM users ORDER BY created_at ASC LIMIT $1 OFFSET $2", &lim_off),
        total(&s, "users")
    );
    let users: Vec<Value> = rows?
        .into_iter()
        .map(|mut u| {
            if let Value::Object(m) = &mut u {
                let (admin, totp) = (
                    m.get("is_admin").cloned().unwrap_or(Value::Null),
                    m.get("totp_enabled").cloned().unwrap_or(Value::Null),
                );
                m.insert("isAdmin".into(), admin);
                m.insert("totpEnabled".into(), totp);
            }
            u
        })
        .collect();
    Ok(ok(json!({ "users": users, "total": count? })))
}

fn admin_name(a: &Admin) -> String {
    a.session.get_str("username").unwrap_or_default()
}

async fn target_username(s: &AppState, id: &str) -> Result<Option<String>> {
    Ok(s.db.opt("SELECT username FROM users WHERE id = $1", params![id]).await?.map(|r| r.get("username")))
}

async fn disable_user_totp(State(s): State<AppState>, a: Admin, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = bad_id(&id) {
        return Ok(r);
    }
    if id == a.user_id {
        return Ok(err(400, "Use your account settings to manage your own 2FA."));
    }
    let Some(name) = target_username(&s, &id).await? else { return Ok(err(404, "User not found")) };
    s.db.exec("UPDATE users SET totp_secret = NULL, totp_enabled = false WHERE id = $1", params![&id]).await?;
    tracing::info!("[admin] {} disabled 2FA for user {name} ({id})", admin_name(&a));
    Ok(ok(json!({ "ok": true })))
}

async fn update_user(State(s): State<AppState>, a: Admin, Path(id): Path<String>, body: Body) -> Result<Response> {
    if let Some(r) = bad_id(&id) {
        return Ok(r);
    }
    let is_admin = body.get("isAdmin").cloned().unwrap_or(Value::Null);
    if id == a.user_id && is_admin == Value::Bool(false) {
        return Ok(err(400, "Cannot remove your own admin status"));
    }
    let Some(name) = target_username(&s, &id).await? else { return Ok(err(404, "User not found")) };
    s.db.exec("UPDATE users SET is_admin = $1 WHERE id = $2", &[Val::from(&is_admin), Val::from(&id)]).await?;
    tracing::info!("[admin] {} set is_admin={} for user {name} ({id})", admin_name(&a), js_string(&is_admin));
    Ok(ok(json!({ "ok": true })))
}

async fn delete_user(State(s): State<AppState>, a: Admin, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = bad_id(&id) {
        return Ok(r);
    }
    if id == a.user_id {
        return Ok(err(400, "Cannot delete your own account"));
    }
    let Some(name) = target_username(&s, &id).await? else { return Ok(err(404, "User not found")) };
    if let Some(hook) = &s.on_user_disconnect {
        hook(id.clone());
    }
    let sources = s.db.rows("SELECT id FROM calendar_import_sources WHERE user_id = $1", params![&id]).await?;
    let ids: Vec<String> = sources.iter().map(|r| r.get::<_, uuid::Uuid>("id").to_string()).collect();
    futures_util::future::join_all(ids.iter().flat_map(|sid| [CALENDARS.stop(sid), ADDRESS_BOOKS.stop(sid)])).await;
    s.db.exec("DELETE FROM users WHERE id = $1", params![&id]).await?;
    tracing::info!("[admin] {} deleted user {name} ({id})", admin_name(&a));
    Ok(ok(json!({ "ok": true })))
}

// ── settings ─────────────────────────────────────────────────────────────────────────

async fn get_settings(State(s): State<AppState>, _a: Admin) -> Result<Response> {
    let rows = s.db.rows("SELECT key, value FROM system_settings", params![]).await?;
    let mut settings = Map::new();
    for r in &rows {
        settings.insert(r.get("key"), Value::String(r.get("value")));
    }
    let locked = super::auth::password_login_disabled_by_env();
    if locked {
        settings.insert("internal_auth_disabled".into(), Value::String("true".into()));
    }
    Ok(ok(json!({ "settings": settings, "passwordLoginLockedByEnv": locked })))
}

async fn auth_events(State(s): State<AppState>, _a: Admin, q: Qs) -> Result<Response> {
    let (limit, offset) = page(&q, 500);
    let lim_off = [Val::from(limit), Val::from(offset)];
    let (rows, count) = tokio::join!(
        s.db.json_rows(
            "SELECT id, event_type, username, user_id, ip, success, created_at FROM auth_events ORDER BY created_at DESC LIMIT $1 OFFSET $2",
            &lim_off
        ),
        total(&s, "auth_events")
    );
    Ok(ok(json!({ "events": rows?, "total": count? })))
}

async fn put_setting(s: &AppState, key: &str, value: &str) -> Result<()> {
    s.db.exec(
        "INSERT INTO system_settings (key, value, updated_at) VALUES ($1, $2, NOW()) ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = NOW()",
        params![key, value],
    )
    .await?;
    Ok(())
}

/// `parseInt(value)` (of `String(value)`).
fn parse_int(v: &Value) -> Option<i64> {
    inboxora_pim::ical::js_parse_int(&js_string(v))
}

async fn patch_settings(State(s): State<AppState>, a: Admin, body: Body) -> Result<Response> {
    let b = |k: &str| body.get(k).filter(|v| !v.is_null());
    if let Some(Value::Bool(open)) = body.get("registration_open") {
        put_setting(&s, "registration_open", if *open { "true" } else { "false" }).await?;
    }
    if body.get("internal_auth_disabled") == Some(&Value::Bool(false)) && super::auth::password_login_disabled_by_env()
    {
        return Ok(err(409, "Password login is disabled by PASSWORD_LOGIN_DISABLED in the server environment."));
    }
    if let Some(Value::Bool(disabled)) = body.get("internal_auth_disabled") {
        if *disabled {
            let providers: i64 =
                s.db.opt("SELECT COUNT(*) AS count FROM oidc_providers WHERE enabled = true", params![])
                    .await?
                    .map_or(0, |r| r.get("count"));
            if providers == 0 {
                return Ok(err(400, "Cannot disable password login: no enabled SSO providers are configured."));
            }
            let ids: i64 =
                s.db.opt("SELECT COUNT(*) AS count FROM user_identities WHERE user_id = $1", params![&a.user_id])
                    .await?
                    .map_or(0, |r| r.get("count"));
            if ids == 0 {
                return Ok(err(
                    400,
                    "Cannot disable password login: link your account to an SSO provider first so you can still sign in.",
                ));
            }
        }
        put_setting(&s, "internal_auth_disabled", if *disabled { "true" } else { "false" }).await?;
        tracing::info!("[admin] {} set internal_auth_disabled={disabled}", admin_name(&a));
    }
    for (key, lo, hi, message) in [
        ("auth_max_attempts", 1, 100, "auth_max_attempts must be between 1 and 100"),
        ("auth_window_minutes", 1, 1440, "auth_window_minutes must be between 1 and 1440"),
    ] {
        if let Some(v) = b(key) {
            let Some(n) = parse_int(v).filter(|n| (lo..=hi).contains(n)) else { return Ok(err(400, message)) };
            put_setting(&s, key, &n.to_string()).await?;
        }
    }
    if b("auth_max_attempts").is_some() || b("auth_window_minutes").is_some() {
        auth_svc::reload_auth_settings(&s.db).await;
    }
    for key in ["allow_private_hosts", "allow_insecure_tls", "allow_nonstandard_ports"] {
        if let Some(Value::Bool(v)) = body.get(key) {
            put_setting(&s, key, if *v { "true" } else { "false" }).await?;
            tracing::info!("[admin] {} set {key}={v}", admin_name(&a));
        }
    }
    if let Some(v) = b("mfa_enforcement") {
        let Some(m) = v.as_str().filter(|m| matches!(*m, "off" | "required")) else {
            return Ok(err(400, "mfa_enforcement must be \"off\" or \"required\""));
        };
        put_setting(&s, "mfa_enforcement", m).await?;
        tracing::info!("[admin] {} set mfa_enforcement={m}", admin_name(&a));
    }
    if let Some(v) = b("mfa_device_trust") {
        let Some(m) = v.as_str().filter(|m| matches!(*m, "never" | "7d" | "30d" | "permanent")) else {
            return Ok(err(400, "mfa_device_trust must be \"never\", \"7d\", \"30d\", or \"permanent\""));
        };
        put_setting(&s, "mfa_device_trust", m).await?;
        tracing::info!("[admin] {} set mfa_device_trust={m}", admin_name(&a));
    }
    if let Some(v) = body.get("custom_css") {
        let Some(css) = v.as_str() else { return Ok(err(400, "custom_css must be a string")) };
        if css.encode_utf16().count() > 50_000 {
            return Ok(err(400, "custom_css must not exceed 50,000 characters"));
        }
        let sanitized = css.replace('\0', "");
        put_setting(&s, "custom_css", &sanitized).await?;
        tracing::info!("[admin] {} updated custom_css ({} chars)", admin_name(&a), sanitized.encode_utf16().count());
    }
    dav_client::invalidate_connection_policy();
    Ok(ok(json!({ "ok": true })))
}

// ── invites ──────────────────────────────────────────────────────────────────────────

async fn list_invites(State(s): State<AppState>, _a: Admin, q: Qs) -> Result<Response> {
    let (limit, offset) = page(&q, 200);
    let lim_off = [Val::from(limit), Val::from(offset)];
    let (rows, count) = tokio::join!(
        s.db.json_rows(
            "SELECT i.id, i.email, i.token, i.created_at, i.expires_at, i.used_at, u.username as used_by_username \
             FROM invites i LEFT JOIN users u ON i.used_by = u.id ORDER BY i.created_at DESC LIMIT $1 OFFSET $2",
            &lim_off
        ),
        total(&s, "invites")
    );
    Ok(ok(json!({ "invites": rows?, "total": count? })))
}

const INVITE_HTML: &str = r#"
          <div style="font-family: -apple-system, Arial, sans-serif; max-width: 480px; margin: 0 auto; padding: 32px; color: #1a1a1a;">
            <div style="margin-bottom: 24px;">
              <span style="font-size: 22px; font-weight: 700; color: #1a1a1a;">Inboxora</span>
            </div>
            <h2 style="margin: 0 0 12px; font-size: 18px; font-weight: 600;">You've been invited</h2>
            <p style="color: #555; line-height: 1.6; margin: 0 0 24px;">
              You've been invited to join Inboxora. Click the button below to create your account.
            </p>
            <a href="{url}" style="display: inline-block; padding: 12px 24px; background: #7c6af7; color: white; text-decoration: none; border-radius: 8px; font-weight: 500; font-size: 14px;">
              Accept Invite
            </a>
            <p style="color: #999; font-size: 12px; margin: 24px 0 0;">
              This invite expires in 7 days and can only be used once.<br>
              If you weren't expecting this, you can ignore this email.
            </p>
          </div>
        "#;

async fn create_invite(State(s): State<AppState>, a: Admin, body: Body) -> Result<Response> {
    static EMAIL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[^@\s]+@[^@\s]+\.[^@\s]+$").unwrap());
    // `email.trim()` throws for non-strings, which Node reports as an unhandled error.
    let email = match body.get("email") {
        Some(Value::String(e)) if !e.is_empty() => e.clone(),
        Some(v) if truthy(Some(v)) => return Err(Error::internal("email.trim is not a function")),
        _ => return Ok(err(400, "Valid email address required")),
    };
    if !EMAIL.is_match(js_trim(&email)) {
        return Ok(err(400, "Valid email address required"));
    }
    let token = crypto::random_hex(32);
    let expires = chrono::Utc::now() + chrono::Duration::days(7);
    s.db.exec(
        "INSERT INTO invites (email, token, created_by, expires_at) VALUES ($1, $2, $3, $4)",
        params![js_trim(&email).to_lowercase(), &token, &a.user_id, expires],
    )
    .await?;
    let Some(app) = inboxora_core::config::env("APP_URL").filter(|u| !u.is_empty()) else {
        return Ok(err(500, "APP_URL is not configured — set it in .env before sending invites."));
    };
    let url = format!("{app}/register?invite={token}");
    let send = async {
        let picked = smtp::system_or_account_transport(
            &s.db,
            "SELECT * FROM email_accounts WHERE user_id = $1 AND enabled = true AND smtp_host IS NOT NULL ORDER BY created_at LIMIT 1",
            &a.user_id,
        )
        .await?;
        let Some((transport, from_header, from_email)) = picked else { return Ok(false) };
        let text = format!(
            "You've been invited to join Inboxora.\n\nClick the link below to create your account:\n{url}\n\nThis invite expires in 7 days and can only be used once."
        );
        let name = from_header.rsplit_once(" <").map_or(from_header.as_str(), |(n, _)| n);
        // nodemailer's address parser trims the recipient.
        let to = js_trim(&email).to_owned();
        let message = smtp::simple_message(
            name,
            &from_email,
            &to,
            "You've been invited to Inboxora",
            &text,
            Some(&INVITE_HTML.replace("{url}", &url)),
        )
        .map_err(|e| e.message)?;
        transport.send_raw(&from_email, std::slice::from_ref(&to), &message).await.map_err(|e| e.message)?;
        Ok::<_, String>(true)
    };
    let (sent, error) = match send.await {
        Ok(sent) => (sent, None),
        Err(e) => {
            tracing::error!("Invite email failed: {e}");
            let lower = e.to_lowercase();
            let mail_server = ["econnrefused", "enotfound", "etimedout", "authentication", "535", "reject"]
                .iter()
                .any(|k| lower.contains(k));
            (
                false,
                Some(if mail_server {
                    "Mail server error. Check your SMTP account settings."
                } else {
                    "Failed to send invite email."
                }),
            )
        }
    };
    Ok(ok(json!({ "ok": true, "inviteUrl": url, "emailSent": sent, "emailError": error })))
}

async fn delete_invite(State(s): State<AppState>, _a: Admin, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = bad_id(&id) {
        return Ok(r);
    }
    s.db.exec("DELETE FROM invites WHERE id = $1", params![id]).await?;
    Ok(ok(json!({ "ok": true })))
}

// ── system email ─────────────────────────────────────────────────────────────────────

const MASK: &str = "••••••••";

async fn system_email_config(s: &AppState) -> Result<Option<String>> {
    Ok(s.db
        .opt("SELECT value FROM system_settings WHERE key = 'system_email_config'", params![])
        .await?
        .map(|r| r.get("value")))
}

async fn get_system_email(State(s): State<AppState>, _a: Admin) -> Result<Response> {
    let Some(raw) = system_email_config(&s).await? else { return Ok(ok(json!({ "config": null }))) };
    let Ok(Value::Object(mut cfg)) = serde_json::from_str::<Value>(&raw) else {
        return Ok(ok(json!({ "config": null })));
    };
    let masked = if truthy(cfg.get("pass")) { MASK } else { "" };
    cfg.insert("pass".into(), Value::String(masked.into()));
    Ok(ok(json!({ "config": cfg })))
}

async fn save_system_email(State(s): State<AppState>, _a: Admin, body: Body) -> Result<Response> {
    let (host, user) = (body.get("host"), body.get("user"));
    if !truthy(host) || !truthy(user) {
        return Ok(err(400, "SMTP host and username are required"));
    }
    let (Some(host), Some(user)) = (host.and_then(Value::as_str), user.and_then(Value::as_str)) else {
        return Err(Error::internal("host.trim is not a function"));
    };
    let policy = dav_client::connection_policy(&s.db).await?;
    if let Some(e) = inboxora_net::validate_host(host, policy.allow_private_hosts).await {
        return Ok(err(400, e));
    }
    let existing_pass = system_email_config(&s)
        .await?
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|cfg| cfg.get("pass").cloned())
        .filter(|p| truthy(Some(p)));
    let pass = body.get("pass").filter(|p| truthy(Some(p)) && p.as_str() != Some(MASK));
    let encrypted = match pass {
        Some(p) => Value::String(crypto::encrypt(&js_string(p)).map_err(Error::internal)?),
        None => existing_pass.unwrap_or(Value::Null),
    };
    let port = body
        .get("port")
        .and_then(|p| inboxora_pim::ical::js_parse_int(&js_string(p)))
        .filter(|p| *p != 0)
        .unwrap_or(587);
    let opt_trim = |k: &str| {
        body.get(k).filter(|v| truthy(Some(v))).map(|v| js_trim(&js_string(v)).to_owned()).filter(|v| !v.is_empty())
    };
    let cfg = json!({
        "host": js_trim(host),
        "port": port,
        "tls": body.get("tls").filter(|t| truthy(Some(t))).cloned().unwrap_or_else(|| json!("STARTTLS")),
        "user": js_trim(user),
        "pass": encrypted,
        "fromName": opt_trim("fromName").unwrap_or_else(|| "Inboxora".into()),
        "fromEmail": opt_trim("fromEmail").unwrap_or_else(|| js_trim(user).to_owned()),
    });
    put_setting(&s, "system_email_config", &inboxora_core::json::stringify(&cfg)).await?;
    Ok(ok(json!({ "ok": true })))
}

async fn test_system_email(State(s): State<AppState>, _a: Admin) -> Result<Response> {
    let Some(raw) = system_email_config(&s).await? else { return Ok(err(400, "No system email configured")) };
    let Ok(cfg) = serde_json::from_str::<Value>(&raw) else { return Ok(err(500, "Corrupted system email config")) };
    let pass = cfg
        .get("pass")
        .filter(|p| truthy(Some(p)))
        .and_then(|p| crypto::decrypt(&js_string(p)))
        .filter(|p| !p.is_empty());
    let Some(pass) = pass else { return Ok(err(400, "No password stored — save the configuration first")) };
    let run = async {
        let policy = dav_client::connection_policy(&s.db).await.map_err(|e| e.to_string())?;
        let host = cfg.get("host").map(js_string).unwrap_or_default();
        let resolved = smtp::resolve(&host, policy.allow_private_hosts).await?;
        let port = cfg.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
        let transport = smtp::Transport {
            resolved,
            port,
            secure: port == 465,
            require_tls: false,
            ignore_tls: false,
            reject_unauthorized: true,
            auth: smtp::Auth::Password { user: cfg.get("user").map(js_string).unwrap_or_default(), pass },
        };
        transport.verify().await.map_err(|e| e.message)
    };
    match run.await {
        Ok(()) => Ok(ok(json!({ "ok": true }))),
        Err(e) => Ok(err(400, &e)),
    }
}

async fn delete_system_email(State(s): State<AppState>, _a: Admin) -> Result<Response> {
    s.db.exec("DELETE FROM system_settings WHERE key = 'system_email_config'", params![]).await?;
    Ok(ok(json!({ "ok": true })))
}

// ── OIDC providers ───────────────────────────────────────────────────────────────────

const PROVIDER_COLUMNS: &str = "id, name, slug, issuer_url, client_id, scopes, provisioning_mode, allowed_domains, enabled, require_email_verified, allow_insecure, admin_group_claim, admin_group_value, rp_initiated_logout, login_match_claim";

async fn list_oidc(State(s): State<AppState>, _a: Admin) -> Result<Response> {
    let rows =
        s.db.json_rows(
            &format!("SELECT {PROVIDER_COLUMNS}, created_at, updated_at FROM oidc_providers ORDER BY name ASC"),
            params![],
        )
        .await?;
    Ok(ok(json!({ "providers": rows })))
}

fn valid_slug(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A defined, non-blank `login_match_claim` must be a valid claim name.
fn match_claim(v: Option<&Value>) -> std::result::Result<Option<String>, &'static str> {
    match v.filter(|v| !v.is_null()) {
        Some(c) if !js_trim(&js_string(c)).is_empty() => oidc::normalize_claim_name(&js_string(c))
            .map(Some)
            .ok_or("login_match_claim must be a valid claim name (letters, digits, . _ : -)"),
        _ => Ok(None),
    }
}

async fn check_issuer(issuer: &str, allow_insecure: bool) -> Option<Response> {
    let Ok(parsed) = url::Url::parse(js_trim(issuer)) else { return Some(err(400, "Issuer URL is not a valid URL")) };
    if !allow_insecure && parsed.scheme() != "https" {
        return Some(err(400, "Issuer URL must use HTTPS"));
    }
    if !allow_insecure && let Some(e) = inboxora_net::validate_host(&dav_client::hostname(&parsed), false).await {
        return Some(err(400, &format!("Issuer URL: {e}")));
    }
    None
}

/// `value?.trim() || null` for an optional string field.
fn trimmed_or_null(v: Option<&Value>) -> Option<String> {
    v.filter(|v| !v.is_null()).map(|v| js_trim(&js_string(v)).to_owned()).filter(|v| !v.is_empty())
}

async fn create_oidc(State(s): State<AppState>, _a: Admin, body: Body) -> Result<Response> {
    let b = |k: &str| body.get(k);
    if !["name", "slug", "issuer_url", "client_id", "client_secret"].iter().all(|k| truthy(b(k))) {
        return Ok(err(400, "name, slug, issuer_url, client_id and client_secret are required"));
    }
    let slug = js_string(b("slug").unwrap_or(&Value::Null));
    if !valid_slug(&slug) {
        return Ok(err(400, "Slug must contain only lowercase letters, numbers and hyphens"));
    }
    let claim = match match_claim(b("login_match_claim")) {
        Ok(c) => c.unwrap_or_else(|| "email".into()),
        Err(m) => return Ok(err(400, m)),
    };
    let insecure = b("allow_insecure") == Some(&Value::Bool(true));
    if let Some(r) = check_issuer(&js_string(b("issuer_url").unwrap_or(&Value::Null)), insecure).await {
        return Ok(r);
    }
    let text = |k: &str| js_trim(&js_string(b(k).unwrap_or(&Value::Null))).to_owned();
    let inserted = s
        .db
        .json_opt(
            &format!(
                "INSERT INTO oidc_providers (name, slug, issuer_url, client_id, client_secret, scopes, provisioning_mode, allowed_domains, enabled, require_email_verified, allow_insecure, admin_group_claim, admin_group_value, rp_initiated_logout, login_match_claim) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) RETURNING {PROVIDER_COLUMNS}"
            ),
            &[
                Val::from(text("name")),
                Val::from(js_trim(&slug)),
                Val::from(text("issuer_url")),
                Val::from(text("client_id")),
                Val::from(crypto::encrypt(&js_string(b("client_secret").unwrap_or(&Value::Null))).map_err(Error::internal)?),
                Val::from(trimmed_or_null(b("scopes")).unwrap_or_else(|| "openid email profile".into())),
                Val::from(b("provisioning_mode").filter(|m| truthy(Some(m))).cloned().unwrap_or_else(|| json!("login_existing_only"))),
                Val::from(trimmed_or_null(b("allowed_domains"))),
                Val::from(b("enabled") != Some(&Value::Bool(false))),
                Val::from(b("require_email_verified") != Some(&Value::Bool(false))),
                Val::from(insecure),
                Val::from(trimmed_or_null(b("admin_group_claim"))),
                Val::from(trimmed_or_null(b("admin_group_value"))),
                Val::from(b("rp_initiated_logout") == Some(&Value::Bool(true))),
                Val::from(claim),
            ],
        )
        .await;
    match inserted {
        Ok(row) => Ok(ok(json!({ "provider": row }))),
        Err(e) if e.is_unique_violation() => Ok(err(409, "A provider with this slug already exists")),
        Err(e) => Err(e),
    }
}

/// `wouldLockOut(providerId)`: the last enabled provider cannot go while password login is off.
async fn would_lock_out(s: &AppState, id: &str) -> Result<Option<Response>> {
    if !super::auth::password_login_disabled(s).await? {
        return Ok(None);
    }
    let others: i64 =
        s.db.opt("SELECT COUNT(*) AS count FROM oidc_providers WHERE enabled = true AND id <> $1", params![id])
            .await?
            .map_or(0, |r| r.get("count"));
    Ok((others == 0).then(|| {
        err(400, "Cannot disable or delete the last enabled SSO provider while password login is off. Re-enable password login first.")
    }))
}

async fn update_oidc(State(s): State<AppState>, _a: Admin, Path(id): Path<String>, body: Body) -> Result<Response> {
    if let Some(r) = bad_id(&id) {
        return Ok(r);
    }
    let b = |k: &str| body.get(k);
    let Some(existing) = s.db.opt("SELECT allow_insecure FROM oidc_providers WHERE id = $1", params![&id]).await?
    else {
        return Ok(err(404, "Provider not found"));
    };
    let claim = match match_claim(b("login_match_claim")) {
        Ok(c) => c,
        Err(m) => return Ok(err(400, m)),
    };
    if b("enabled") == Some(&Value::Bool(false))
        && let Some(r) = would_lock_out(&s, &id).await?
    {
        return Ok(r);
    }
    if let Some(slug) = b("slug").filter(|v| truthy(Some(v)))
        && !valid_slug(&js_string(slug))
    {
        return Ok(err(400, "Slug must contain only lowercase letters, numbers and hyphens"));
    }
    if let Some(issuer) = b("issuer_url").filter(|v| truthy(Some(v))) {
        let insecure = match b("allow_insecure") {
            Some(v) => truthy(Some(v)),
            None => existing.get::<_, Option<bool>>("allow_insecure").unwrap_or(false),
        };
        if let Some(r) = check_issuer(&js_string(issuer), insecure).await {
            return Ok(r);
        }
    }
    let secret = b("client_secret")
        .filter(|v| truthy(Some(v)) && v.as_str() != Some(MASK))
        .map(|v| crypto::encrypt(&js_string(v)))
        .transpose()
        .map_err(Error::internal)?;
    let keep = |k: &str| match b(k) {
        None => Val::from("__keep__"),
        Some(v) => Val::from(trimmed_or_null(Some(v))),
    };
    let defined = |k: &str| Val::from(b(k).cloned().unwrap_or(Value::Null));
    let updated = s
        .db
        .json_opt(
            &format!(
                "UPDATE oidc_providers SET name = COALESCE($2, name), slug = COALESCE($3, slug), issuer_url = COALESCE($4, issuer_url), \
                 client_id = COALESCE($5, client_id), client_secret = COALESCE($6, client_secret), scopes = COALESCE($7, scopes), \
                 provisioning_mode = COALESCE($8, provisioning_mode), \
                 allowed_domains = CASE WHEN $9::text IS DISTINCT FROM '__keep__' THEN $9::text ELSE allowed_domains END, \
                 enabled = COALESCE($10, enabled), require_email_verified = COALESCE($11, require_email_verified), \
                 allow_insecure = COALESCE($12, allow_insecure), \
                 admin_group_claim = CASE WHEN $13::text IS DISTINCT FROM '__keep__' THEN $13::text ELSE admin_group_claim END, \
                 admin_group_value = CASE WHEN $14::text IS DISTINCT FROM '__keep__' THEN $14::text ELSE admin_group_value END, \
                 rp_initiated_logout = COALESCE($15, rp_initiated_logout), login_match_claim = COALESCE($16, login_match_claim), \
                 updated_at = NOW() WHERE id = $1 RETURNING {PROVIDER_COLUMNS}"
            ),
            &[
                Val::from(&id),
                Val::from(trimmed_or_null(b("name"))),
                Val::from(trimmed_or_null(b("slug"))),
                Val::from(trimmed_or_null(b("issuer_url"))),
                Val::from(trimmed_or_null(b("client_id"))),
                Val::from(secret),
                Val::from(trimmed_or_null(b("scopes"))),
                Val::from(b("provisioning_mode").filter(|m| truthy(Some(m))).cloned()),
                keep("allowed_domains"),
                defined("enabled"),
                defined("require_email_verified"),
                defined("allow_insecure"),
                keep("admin_group_claim"),
                keep("admin_group_value"),
                defined("rp_initiated_logout"),
                Val::from(claim),
            ],
        )
        .await;
    match updated {
        Ok(Some(row)) => Ok(ok(json!({ "provider": row }))),
        Ok(None) => Ok(err(404, "Provider not found")),
        Err(e) if e.is_unique_violation() => Ok(err(409, "A provider with this slug already exists")),
        Err(e) => Err(e),
    }
}

async fn delete_oidc(State(s): State<AppState>, _a: Admin, Path(id): Path<String>) -> Result<Response> {
    if let Some(r) = bad_id(&id) {
        return Ok(r);
    }
    if let Some(r) = would_lock_out(&s, &id).await? {
        return Ok(r);
    }
    s.db.exec("DELETE FROM oidc_providers WHERE id = $1", params![id]).await?;
    Ok(ok(json!({ "ok": true })))
}
