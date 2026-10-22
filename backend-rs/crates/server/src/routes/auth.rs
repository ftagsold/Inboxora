//! `/api/auth`: password sign-in, two-factor flows, screen lock, profile and preferences
//! (port of `routes/auth.ts`).

use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use axum::routing::{get, patch, post};
use fred::prelude::*;
use inboxora_core::pg::{Q, Val};
use inboxora_core::session::Session;
use inboxora_core::{Error, Result, crypto, params};
use serde_json::{Map, Value, json};

use super::util::{err, json, ok};
use crate::http::{Body, ClientIp, Sess};
use crate::services::auth::{
    self as svc, auth_limits, bcrypt_hash, bcrypt_verify, log_auth_event, rate_consume, rate_reset,
};
use crate::services::dav_xml::js_string;
use crate::services::{smtp, totp};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/2fa/challenge", post(totp_challenge))
        .route("/2fa/send-email-otp", post(send_email_otp))
        .route("/2fa/verify-email-otp", post(verify_email_otp))
        .route("/2fa/enrollment/setup", get(enrollment_setup))
        .route("/2fa/enrollment/enable", post(enrollment_enable))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/lock", post(lock))
        .route("/unlock", post(unlock))
        .route("/lock-pin", post(set_lock_pin).delete(clear_lock_pin))
        .route("/profile", patch(update_profile))
        .route("/avatar", post(set_avatar).delete(clear_avatar))
        .route("/registration-status", get(registration_status))
        .route("/invite/{token}", get(invite))
        .route("/preferences", get(get_preferences).patch(patch_preferences))
        .route("/preferences/whitelist-add", post(whitelist_add))
        .route("/profile/recovery-email", get(get_recovery_email).patch(set_recovery_email))
        .route("/forgot-password", post(forgot_password))
        .route("/reset-password", post(reset_password))
        .route("/push/vapid-key", get(vapid_key))
        .route("/push/subscribe", post(push_subscribe))
        .route("/push/unsubscribe", post(push_unsubscribe))
}

static DUMMY_HASH: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| bcrypt::hash("mailflow-timing-equalizer", 12).unwrap_or_default());

// ── helpers ──────────────────────────────────────────────────────────────────────────

fn truthy(v: Option<&Value>) -> bool {
    inboxora_core::json::truthy(v)
}

/// JS string length (UTF-16 code units).
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s.slice(0, n)` in UTF-16 units (a split surrogate pair becomes U+FFFD, as UTF-8 encoding does).
fn js_slice(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

fn js_trim(s: &str) -> &str {
    inboxora_mail::text::js_trim(s)
}

fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

/// `authLimiter`: per-IP budget shared with the DAV servers' failure counter.
async fn auth_limit(s: &AppState, ip: &str) -> std::result::Result<(), Response> {
    let l = auth_limits();
    let (limited, reset_ms) = rate_consume(&s.redis, &format!("auth:{ip}"), l.max_requests, l.window).await;
    if limited {
        let mut res = err(429, "Too many attempts. Please try again later.");
        res.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(reset_ms.div_ceil(1000)));
        return Err(res);
    }
    Ok(())
}

fn limited(reset_ms: u64, message: &str) -> Response {
    let mut res = err(429, message);
    res.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(reset_ms.div_ceil(1000)));
    res
}

fn user_json(u: &Map<String, Value>, totp_override: Option<bool>) -> Value {
    let f = |k: &str| u.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "user": {
            "id": f("id"),
            "username": f("username"),
            "displayName": f("display_name"),
            "avatar": f("avatar"),
            "isAdmin": f("is_admin"),
            "totpEnabled": totp_override.map_or_else(|| f("totp_enabled"), Value::Bool),
        }
    })
}

fn sign_in(session: &Session, u: &Map<String, Value>) {
    session.set("userId", u.get("id").cloned().unwrap_or(Value::Null));
    session.set("username", u.get("username").cloned().unwrap_or(Value::Null));
    session.set("isAdmin", u.get("is_admin").cloned().unwrap_or(Value::Null));
}

fn connect_user(s: &AppState, user_id: &str) {
    if let Some(hook) = &s.on_user_connect {
        hook(user_id.to_owned());
    }
}

fn str_of(u: &Map<String, Value>, k: &str) -> Option<String> {
    u.get(k).and_then(Value::as_str).map(str::to_owned)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).unwrap_or("");
    raw.split(';').map(str::trim).find_map(|c| c.strip_prefix(&format!("{name}=")).map(str::to_owned))
}

fn set_cookie(res: &mut Response, value: String) {
    if let Ok(v) = HeaderValue::from_str(&value) {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
}

fn trust_ms(setting: &str) -> i64 {
    match setting {
        "7d" => 7 * 24 * 3600 * 1000,
        "30d" => 30 * 24 * 3600 * 1000,
        "permanent" => 365 * 24 * 3600 * 1000,
        _ => 0,
    }
}

async fn setting(s: &AppState, key: &str) -> Result<Option<String>> {
    Ok(s.db.opt("SELECT value FROM system_settings WHERE key = $1", params![key]).await?.map(|r| r.get("value")))
}

/// `createTrustedDevice`: remember this browser for the configured MFA trust period.
async fn create_trusted_device(s: &AppState, user_id: &str, secure: bool) -> Result<Option<String>> {
    let trust = setting(s, "mfa_device_trust").await?.filter(|v| !v.is_empty()).unwrap_or_else(|| "30d".into());
    let ms = trust_ms(&trust);
    if ms == 0 {
        return Ok(None);
    }
    let token = crypto::random_hex(32);
    let expires = chrono::Utc::now() + chrono::Duration::milliseconds(ms);
    s.db.exec(
        "INSERT INTO trusted_devices (user_id, token_hash, expires_at) VALUES ($1, $2, $3)",
        params![user_id, crypto::sha256_hex(&token), expires],
    )
    .await?;
    let expires_text = expires.format("%a, %d %b %Y %H:%M:%S GMT");
    Ok(Some(format!(
        "mf_td={token}; Max-Age={}; Path=/; Expires={expires_text}; HttpOnly{}; SameSite=Lax",
        ms / 1000,
        if secure { "; Secure" } else { "" }
    )))
}

fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else { return email.to_owned() };
    let domain = domain.split('@').next().unwrap_or(domain);
    let chars: Vec<char> = local.chars().collect();
    let masked = if js_len(local) <= 2 {
        format!("{}*", chars.first().map(char::to_string).unwrap_or_else(|| "undefined".into()))
    } else {
        format!("{}{}{}", chars[0], "*".repeat((js_len(local) - 2).min(4)), chars[chars.len() - 1])
    };
    format!("{masked}@{domain}")
}

const OTP_HTML: &str = r#"
      <div style="font-family:-apple-system,Arial,sans-serif;max-width:480px;margin:0 auto;padding:32px;color:#1a1a1a;">
        <div style="margin-bottom:24px;">
          <span style="font-size:22px;font-weight:700;color:#1a1a1a;">Mail</span><span style="font-size:22px;font-weight:600;color:#7c6af7;">Flow</span>
        </div>
        <h2 style="margin:0 0 12px;font-size:18px;font-weight:600;">Your sign-in code</h2>
        <p style="color:#555;line-height:1.6;margin:0 0 24px;">Use the code below to sign in to Inboxora. It expires in 10 minutes.</p>
        <div style="font-size:36px;font-weight:700;letter-spacing:0.2em;text-align:center;padding:20px;background:#f5f4ff;border-radius:8px;color:#7c6af7;margin-bottom:24px;">{code}</div>
        <p style="color:#999;font-size:12px;margin:0;">If you did not request this code, you can ignore this email.</p>
      </div>
    "#;

/// `sendEmailOtpCode(userId, email)`.
async fn send_email_otp_code(s: &AppState, user_id: &str, to: &str) -> std::result::Result<(), String> {
    let b = crypto::random_bytes::<3>();
    let code = ((u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2])) % 900_000 + 100_000).to_string();
    let expires = chrono::Utc::now() + chrono::Duration::minutes(10);
    let e = |e: Error| e.to_string();
    s.db.exec("DELETE FROM email_otp_tokens WHERE user_id = $1 AND used_at IS NULL", params![user_id])
        .await
        .map_err(e)?;
    s.db.exec(
        "INSERT INTO email_otp_tokens (user_id, code_hash, expires_at) VALUES ($1, $2, $3)",
        params![user_id, crypto::sha256_hex(&code), expires],
    )
    .await
    .map_err(e)?;
    let text = format!(
        "Your sign-in code is: {code}\n\nThis code expires in 10 minutes.\n\nIf you did not request this code, you can ignore this email."
    );
    smtp::send_system_email(&s.db, to, "Your Inboxora sign-in code", &text, Some(&OTP_HTML.replace("{code}", &code)))
        .await
}

/// `PASSWORD_LOGIN_DISABLED` (env) forces SSO regardless of the admin setting.
pub fn password_login_disabled_by_env() -> bool {
    inboxora_core::config::env("PASSWORD_LOGIN_DISABLED")
        .map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

pub async fn password_login_disabled(s: &AppState) -> Result<bool> {
    if password_login_disabled_by_env() {
        return Ok(true);
    }
    Ok(setting(s, "internal_auth_disabled").await?.as_deref() == Some("true"))
}

fn session_user(session: &Session) -> std::result::Result<String, Response> {
    session.user_id().filter(|u| !u.is_empty()).ok_or_else(|| err(401, "Not authenticated"))
}

async fn load_user(s: &AppState, id: &str) -> Result<Option<Map<String, Value>>> {
    s.db.json_opt("SELECT * FROM users WHERE id = $1", params![id]).await
}

// ── registration and sign-in ─────────────────────────────────────────────────────────

const SSO_ONLY: &str = "Password-based registration is disabled. Please sign in with your SSO provider.";

enum Registered {
    Done(Map<String, Value>),
    Refused(&'static str),
}

async fn register(
    State(s): State<AppState>,
    ClientIp(ip): ClientIp,
    Sess(session): Sess,
    body: Body,
) -> Result<Response> {
    if let Err(r) = auth_limit(&s, &ip).await {
        return Ok(r);
    }
    let (Some(username), Some(password)) =
        (body.str("username").filter(|u| !u.is_empty()), body.str("password").filter(|p| !p.is_empty()))
    else {
        return Ok(err(400, "Username and password required"));
    };
    let trimmed = js_trim(&username.to_lowercase()).to_owned();
    if !(1..=120).contains(&js_len(&trimmed)) {
        return Ok(err(400, "Username must be between 1 and 120 characters"));
    }
    if trimmed.chars().any(|c| (c as u32) <= 0x1f || c as u32 == 0x7f) {
        return Ok(err(400, "Username contains invalid characters"));
    }
    if password_login_disabled_by_env() {
        return Ok(err(403, SSO_ONLY));
    }
    let hash = bcrypt_hash(password.to_owned(), 12).await?;
    let invite = body.get("inviteToken").filter(|t| truthy(Some(t))).cloned();
    let run = s
        .db
        .transaction(false, 0, |tx| {
            let (trimmed, hash, invite) = (trimmed.clone(), hash.clone(), invite.clone());
            Box::pin(async move {
                tx.exec("SELECT pg_advisory_xact_lock(7936352)", params![]).await?;
                let count: i64 = tx.opt("SELECT COUNT(*) AS count FROM users", params![]).await?.map_or(0, |r| r.get("count"));
                let first = count == 0;
                if !first {
                    let rows = tx
                        .rows("SELECT key, value FROM system_settings WHERE key IN ('registration_open', 'internal_auth_disabled')", params![])
                        .await?;
                    let get = |k: &str| rows.iter().find(|r| r.get::<_, String>("key") == k).map(|r| r.get::<_, String>("value"));
                    if get("internal_auth_disabled").as_deref() == Some("true") {
                        return Ok(Registered::Refused(SSO_ONLY));
                    }
                    let open = get("registration_open").as_deref() == Some("true");
                    if !open && invite.is_none() {
                        return Ok(Registered::Refused("Registration is currently by invitation only."));
                    }
                    if let Some(token) = &invite {
                        let found = tx
                            .opt("SELECT id FROM invites WHERE token = $1 AND used_at IS NULL AND expires_at > NOW() FOR UPDATE", &[Val::from(token)])
                            .await?;
                        if found.is_none() {
                            return Ok(Registered::Refused("Invalid or expired invite link."));
                        }
                    }
                }
                let user = tx
                    .json_opt("INSERT INTO users (username, password_hash, is_admin) VALUES ($1, $2, $3) RETURNING id, username, is_admin", params![&trimmed, &hash, first])
                    .await?
                    .ok_or_else(|| Error::internal("insert returned no row"))?;
                let id = user.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                svc::ensure_user_dav_resources(tx, &id).await?;
                if first {
                    tx.exec(
                        "INSERT INTO system_settings (key, value, updated_at) VALUES ('registration_open', 'false', NOW()) ON CONFLICT (key) DO UPDATE SET value = 'false', updated_at = NOW()",
                        params![],
                    )
                    .await?;
                }
                if let Some(token) = &invite {
                    let row = tx.opt("UPDATE invites SET used_by = $1, used_at = NOW() WHERE token = $2 RETURNING email", &[Val::from(&id), Val::from(token)]).await?;
                    if let Some(email) = row.and_then(|r| r.get::<_, Option<String>>("email")).filter(|e| !e.is_empty()) {
                        tx.exec("UPDATE users SET recovery_email = $1 WHERE id = $2", params![js_trim(&email.to_lowercase()), &id]).await?;
                    }
                }
                Ok(Registered::Done(user))
            })
        })
        .await;
    let user = match run {
        Ok(Registered::Done(u)) => u,
        Ok(Registered::Refused(m)) => return Ok(err(403, m)),
        Err(e) if e.is_unique_violation() => return Ok(err(409, "Username already taken")),
        Err(e) => {
            tracing::error!("Registration error: {e}");
            return Ok(err(500, "Registration failed"));
        }
    };
    if let Err(e) = s.sessions.regenerate(&session).await {
        tracing::error!("Registration error: {e}");
        return Ok(err(500, "Registration failed"));
    }
    sign_in(&session, &user);
    connect_user(&s, user.get("id").and_then(Value::as_str).unwrap_or_default());
    let f = |k: &str| user.get(k).cloned().unwrap_or(Value::Null);
    Ok(ok(
        json!({ "user": { "id": f("id"), "username": f("username"), "displayName": null, "avatar": null, "isAdmin": f("is_admin"), "totpEnabled": false } }),
    ))
}

async fn login(
    State(s): State<AppState>,
    ClientIp(ip): ClientIp,
    Sess(session): Sess,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    if let Err(r) = auth_limit(&s, &ip).await {
        return Ok(r);
    }
    let (username, password) = (body.get("username"), body.get("password"));
    if !truthy(username) || !truthy(password) {
        return Ok(err(400, "Username and password required"));
    }
    let run = async {
        if password_login_disabled(&s).await? {
            return Ok(err(403, "Password login is disabled. Please sign in with your SSO provider."));
        }
        // `username.toLowerCase()` throws for non-strings (→ "Login failed").
        let Some(name) = username.and_then(Value::as_str).map(|u| js_trim(&u.to_lowercase()).to_owned()) else {
            return Err(Error::internal("username is not a string"));
        };
        let password = js_string(password.unwrap_or(&Value::Null));
        let Some(user) = s.db.json_opt("SELECT * FROM users WHERE username = $1", params![&name]).await? else {
            let _ = bcrypt_verify(password, DUMMY_HASH.clone()).await;
            log_auth_event(&s.db, "login_fail", Some(&name), None, Some(&ip), false);
            return Ok(err(401, "Invalid credentials"));
        };
        let uid = str_of(&user, "id").unwrap_or_default();
        let uname = str_of(&user, "username");
        let Some(hash) = str_of(&user, "password_hash").filter(|h| !h.is_empty()) else {
            let _ = bcrypt_verify(password, DUMMY_HASH.clone()).await;
            log_auth_event(&s.db, "login_fail", uname.as_deref(), Some(&uid), Some(&ip), false);
            return Ok(err(401, "This account uses single sign-on. Please sign in with your SSO provider."));
        };
        if !bcrypt_verify(password, hash).await {
            log_auth_event(&s.db, "login_fail", uname.as_deref(), Some(&uid), Some(&ip), false);
            return Ok(err(401, "Invalid credentials"));
        }
        s.sessions.regenerate(&session).await?;
        if let Some(device) = cookie_value(&headers, "mf_td").filter(|d| !d.is_empty()) {
            let row = s
                .db
                .opt(
                    "SELECT id FROM trusted_devices WHERE user_id = $1 AND token_hash = $2 AND (expires_at IS NULL OR expires_at > NOW())",
                    params![&uid, crypto::sha256_hex(&device)],
                )
                .await?;
            if let Some(row) = row {
                s.db.exec(
                    "UPDATE trusted_devices SET last_used_at = NOW() WHERE id = $1",
                    params![row.get::<_, uuid::Uuid>("id")],
                )
                .await?;
                sign_in(&session, &user);
                connect_user(&s, &uid);
                log_auth_event(&s.db, "login_success", uname.as_deref(), Some(&uid), Some(&ip), true);
                rate_reset(&s.redis, &format!("auth:{ip}")).await;
                return Ok(ok(user_json(&user, None)));
            }
        }
        let policy =
            s.db.rows(
                "SELECT key, value FROM system_settings WHERE key IN ('mfa_enforcement', 'mfa_device_trust')",
                params![],
            )
            .await?;
        let get = |k: &str| {
            policy
                .iter()
                .find(|r| r.get::<_, String>("key") == k)
                .map(|r| r.get::<_, String>("value"))
                .filter(|v| !v.is_empty())
        };
        let enforcement = get("mfa_enforcement").unwrap_or_else(|| "off".into());
        let trust_available = trust_ms(&get("mfa_device_trust").unwrap_or_else(|| "30d".into())) > 0;
        if user.get("totp_enabled").and_then(Value::as_bool).unwrap_or(false) {
            session.set("pendingUserId", uid.clone());
            session.set("pendingTOTPExpiry", now_ms() + 5.0 * 60_000.0);
            return Ok(ok(json!({ "requiresTOTP": true, "deviceTrustAvailable": trust_available })));
        }
        if enforcement == "required" {
            session.set("pendingUserId", uid.clone());
            session.set("pendingTOTPExpiry", now_ms() + 10.0 * 60_000.0);
            if let Some(recovery) = str_of(&user, "recovery_email").filter(|e| !e.is_empty()) {
                match send_email_otp_code(&s, &uid, &recovery).await {
                    Ok(()) => {
                        return Ok(ok(
                            json!({ "requiresEmailOTP": true, "emailHint": mask_email(&recovery), "deviceTrustAvailable": trust_available }),
                        ));
                    }
                    Err(e) => tracing::error!("Email OTP auto-send failed, falling back to enrollment: {e}"),
                }
            }
            session.set("pendingMFAEnrollment", true);
            return Ok(ok(json!({ "requiresMFAEnrollment": true })));
        }
        sign_in(&session, &user);
        connect_user(&s, &uid);
        log_auth_event(&s.db, "login_success", uname.as_deref(), Some(&uid), Some(&ip), true);
        rate_reset(&s.redis, &format!("auth:{ip}")).await;
        Ok(ok(user_json(&user, None)))
    };
    match run.await {
        Ok(r) => Ok(r),
        Err(_) => Ok(err(500, "Login failed")),
    }
}

/// The pending-login checks shared by the second-factor routes.
fn pending_expired(session: &Session) -> bool {
    let expiry = session.get("pendingTOTPExpiry").and_then(|v| v.as_f64()).unwrap_or(0.0);
    now_ms() > expiry
}

/// The completed second factor: who signed in, from where, and how to answer.
struct SecondFactor<'a> {
    user: &'a Map<String, Value>,
    remember: bool,
    secure: bool,
    ip: &'a str,
    pending_uid: &'a str,
    totp_override: Option<bool>,
}

async fn finish_second_factor(s: &AppState, session: &Session, f: SecondFactor<'_>) -> Result<Response> {
    let SecondFactor { user, remember, secure, ip, pending_uid, totp_override } = f;
    s.sessions.regenerate(session).await?;
    sign_in(session, user);
    let uid = str_of(user, "id").unwrap_or_default();
    let mut cookie = None;
    if remember {
        match create_trusted_device(s, &uid, secure).await {
            Ok(c) => cookie = c,
            Err(e) => tracing::error!("createTrustedDevice failed: {e}"),
        }
    }
    connect_user(s, &uid);
    log_auth_event(&s.db, "totp_success", str_of(user, "username").as_deref(), Some(&uid), Some(ip), true);
    rate_reset(&s.redis, &format!("auth:{ip}")).await;
    rate_reset(&s.redis, &format!("totp:{pending_uid}")).await;
    let mut res = ok(user_json(user, totp_override));
    if let Some(c) = cookie {
        set_cookie(&mut res, c);
    }
    Ok(res)
}

async fn totp_challenge(
    State(s): State<AppState>,
    ClientIp(ip): ClientIp,
    Sess(session): Sess,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    if let Err(r) = auth_limit(&s, &ip).await {
        return Ok(r);
    }
    let Some(code) = body.get("code").filter(|c| truthy(Some(c))) else { return Ok(err(400, "Code required")) };
    let Some(uid) = session.get("pendingUserId").filter(|v| truthy(Some(v))).map(|v| js_string(&v)) else {
        return Ok(err(400, "No pending authentication"));
    };
    if pending_expired(&session) {
        session.remove("pendingUserId");
        session.remove("pendingTOTPExpiry");
        return Ok(err(400, "Authentication timed out. Please log in again."));
    }
    let (is_limited, reset) = rate_consume(&s.redis, &format!("totp:{uid}"), 5, Duration::from_secs(15 * 60)).await;
    if is_limited {
        return Ok(limited(reset, "Too many attempts. Please log in again."));
    }
    let user = load_user(&s, &uid).await?;
    let Some(user) = user.filter(|u| str_of(u, "totp_secret").is_some_and(|t| !t.is_empty())) else {
        log_auth_event(&s.db, "totp_fail", None, Some(&uid), Some(&ip), false);
        return Ok(err(401, "Authentication failed"));
    };
    let user_id = str_of(&user, "id").unwrap_or_default();
    let uname = str_of(&user, "username");
    let normalized: String = js_string(code).chars().filter(|c| !c.is_whitespace() && *c != '\u{feff}').collect();
    let secret = crypto::decrypt(&str_of(&user, "totp_secret").unwrap_or_default());
    if !secret.is_some_and(|sec| totp::verify(&normalized, &sec)) {
        log_auth_event(&s.db, "totp_fail", uname.as_deref(), Some(&user_id), Some(&ip), false);
        return Ok(err(401, "Invalid code"));
    }
    let first_use: Option<String> = s
        .redis
        .set(format!("totp_used:{user_id}:{normalized}"), "1", Some(Expiration::EX(90)), Some(SetOptions::NX), false)
        .await
        .map_err(Error::from)?;
    if first_use.is_none() {
        log_auth_event(&s.db, "totp_fail", uname.as_deref(), Some(&user_id), Some(&ip), false);
        return Ok(err(401, "Invalid code"));
    }
    let secure = crate::http::middleware::request_is_secure(&headers);
    finish_second_factor(
        &s,
        &session,
        SecondFactor {
            user: &user,
            remember: truthy(body.get("rememberDevice")),
            secure,
            ip: &ip,
            pending_uid: &uid,
            totp_override: None,
        },
    )
    .await
}

async fn send_email_otp(State(s): State<AppState>, ClientIp(ip): ClientIp, Sess(session): Sess) -> Result<Response> {
    if let Err(r) = auth_limit(&s, &ip).await {
        return Ok(r);
    }
    let uid = session.get("pendingUserId").filter(|v| truthy(Some(v))).map(|v| js_string(&v));
    let Some(uid) = uid.filter(|_| !session.get_bool("pendingMFAEnrollment")) else {
        return Ok(err(400, "No pending authentication"));
    };
    if pending_expired(&session) {
        return Ok(err(400, "Authentication timed out. Please log in again."));
    }
    let (is_limited, _) = rate_consume(&s.redis, &format!("otp-send:{uid}"), 3, Duration::from_secs(5 * 60)).await;
    if is_limited {
        return Ok(err(429, "Too many code requests. Please wait before requesting another."));
    }
    let recovery =
        s.db.opt("SELECT recovery_email FROM users WHERE id = $1", params![&uid])
            .await?
            .and_then(|r| r.get::<_, Option<String>>("recovery_email"));
    let Some(recovery) = recovery.filter(|e| !e.is_empty()) else {
        return Ok(err(400, "No recovery email configured"));
    };
    match send_email_otp_code(&s, &uid, &recovery).await {
        Ok(()) => Ok(ok(json!({ "ok": true, "emailHint": mask_email(&recovery) }))),
        Err(e) => {
            tracing::error!("Email OTP send failed: {e}");
            Ok(err(500, "Failed to send verification code. Check system email configuration."))
        }
    }
}

async fn verify_email_otp(
    State(s): State<AppState>,
    ClientIp(ip): ClientIp,
    Sess(session): Sess,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    if let Err(r) = auth_limit(&s, &ip).await {
        return Ok(r);
    }
    let Some(code) = body.get("code").filter(|c| truthy(Some(c))) else { return Ok(err(400, "Code required")) };
    let uid = session.get("pendingUserId").filter(|v| truthy(Some(v))).map(|v| js_string(&v));
    let Some(uid) = uid.filter(|_| !session.get_bool("pendingMFAEnrollment")) else {
        return Ok(err(400, "No pending authentication"));
    };
    if pending_expired(&session) {
        session.remove("pendingUserId");
        session.remove("pendingTOTPExpiry");
        return Ok(err(400, "Authentication timed out. Please log in again."));
    }
    let (is_limited, reset) = rate_consume(&s.redis, &format!("totp:{uid}"), 5, Duration::from_secs(15 * 60)).await;
    if is_limited {
        return Ok(limited(reset, "Too many attempts. Please log in again."));
    }
    let hash = crypto::sha256_hex(js_trim(&js_string(code)));
    let token = s
        .db
        .opt(
            "SELECT id FROM email_otp_tokens WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL AND expires_at > NOW() ORDER BY created_at DESC LIMIT 1",
            params![&uid, hash],
        )
        .await?;
    let Some(token) = token else {
        log_auth_event(&s.db, "totp_fail", None, Some(&uid), Some(&ip), false);
        return Ok(err(401, "Invalid or expired code"));
    };
    s.db.exec("UPDATE email_otp_tokens SET used_at = NOW() WHERE id = $1", params![token.get::<_, uuid::Uuid>("id")])
        .await?;
    let Some(user) = load_user(&s, &uid).await? else { return Ok(err(401, "Authentication failed")) };
    let secure = crate::http::middleware::request_is_secure(&headers);
    finish_second_factor(
        &s,
        &session,
        SecondFactor {
            user: &user,
            remember: truthy(body.get("rememberDevice")),
            secure,
            ip: &ip,
            pending_uid: &uid,
            totp_override: None,
        },
    )
    .await
}

async fn enrollment_setup(State(s): State<AppState>, Sess(session): Sess) -> Result<Response> {
    let uid = session.get("pendingUserId").filter(|v| truthy(Some(v))).map(|v| js_string(&v));
    let Some(uid) = uid.filter(|_| session.get_bool("pendingMFAEnrollment")) else {
        return Ok(err(400, "No pending enrollment"));
    };
    if pending_expired(&session) {
        return Ok(err(400, "Session expired. Please log in again."));
    }
    let username =
        s.db.opt("SELECT username FROM users WHERE id = $1", params![&uid])
            .await?
            .and_then(|r| r.get::<_, Option<String>>("username"))
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "user".into());
    let secret = totp::generate_secret();
    let qr = totp::qr_data_url(&totp::keyuri(&username, "Inboxora", &secret)).map_err(Error::internal)?;
    session.set("pendingTOTPSecret", secret.clone());
    session.set("pendingTOTPSetupExpiry", now_ms() + 10.0 * 60_000.0);
    Ok(ok(json!({ "secret": secret, "qrCode": qr })))
}

async fn enrollment_enable(
    State(s): State<AppState>,
    ClientIp(ip): ClientIp,
    Sess(session): Sess,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    if let Err(r) = auth_limit(&s, &ip).await {
        return Ok(r);
    }
    let Some(code) = body.get("code").filter(|c| truthy(Some(c))) else { return Ok(err(400, "Code required")) };
    let uid = session.get("pendingUserId").filter(|v| truthy(Some(v))).map(|v| js_string(&v));
    let Some(uid) = uid.filter(|_| session.get_bool("pendingMFAEnrollment")) else {
        return Ok(err(400, "No pending enrollment"));
    };
    if pending_expired(&session) {
        return Ok(err(400, "Session expired. Please log in again."));
    }
    let setup_expiry = session.get("pendingTOTPSetupExpiry").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let Some(secret) = session.get_str("pendingTOTPSecret").filter(|s| !s.is_empty() && now_ms() <= setup_expiry)
    else {
        return Ok(err(400, "Setup session expired. Start over."));
    };
    let (is_limited, reset) = rate_consume(&s.redis, &format!("totp:{uid}"), 5, Duration::from_secs(15 * 60)).await;
    if is_limited {
        return Ok(limited(reset, "Too many attempts. Please log in again."));
    }
    let normalized: String = js_string(code).chars().filter(|c| !c.is_whitespace()).collect();
    if !totp::verify(&normalized, &secret) {
        return Ok(err(400, "Invalid code — check your device clock and try again."));
    }
    s.db.exec(
        "UPDATE users SET totp_secret = $1, totp_enabled = true WHERE id = $2",
        params![crypto::encrypt(&secret).map_err(Error::internal)?, &uid],
    )
    .await?;
    let Some(user) = load_user(&s, &uid).await? else { return Ok(err(401, "Authentication failed")) };
    let secure = crate::http::middleware::request_is_secure(&headers);
    finish_second_factor(
        &s,
        &session,
        SecondFactor { user: &user, remember: false, secure, ip: &ip, pending_uid: &uid, totp_override: Some(true) },
    )
    .await
}

async fn logout(State(s): State<AppState>, Sess(session): Sess, headers: HeaderMap) -> Result<Response> {
    let user_id = session.user_id();
    let provider = session.get("oidcProviderId").filter(|v| truthy(Some(v))).map(|v| js_string(&v));
    let id_token = session.get_str("oidcIdToken");
    if let (Some(uid), Some(device)) = (user_id.clone(), cookie_value(&headers, "mf_td").filter(|d| !d.is_empty())) {
        let db = s.db.clone();
        tokio::spawn(async move {
            if let Err(e) = db
                .exec(
                    "DELETE FROM trusted_devices WHERE user_id = $1 AND token_hash = $2",
                    params![uid, crypto::sha256_hex(&device)],
                )
                .await
            {
                tracing::error!("logout: failed to delete trusted device: {e}");
            }
        });
    }
    let end = crate::services::oidc::end_session_url(&s.db, provider.as_deref(), id_token.as_deref()).await;
    if let Err(e) = s.sessions.destroy(&session).await {
        tracing::error!("Session destroy error: {e}");
    }
    let secure = if crate::http::middleware::request_is_secure(&headers) { "; Secure" } else { "" };
    let mut res = ok(json!({ "ok": true, "endSessionUrl": end }));
    set_cookie(&mut res, format!("connect.sid=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT{secure}; SameSite=Lax"));
    set_cookie(
        &mut res,
        format!("mf_td=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT; HttpOnly{secure}; SameSite=Lax"),
    );
    if let (Some(uid), Some(hook)) = (user_id, &s.on_user_disconnect) {
        hook(uid);
    }
    Ok(res)
}

async fn me(State(s): State<AppState>, Sess(session): Sess) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let user = s
        .db
        .json_opt("SELECT id, username, display_name, avatar, is_admin, totp_enabled, password_hash, lock_pin_hash FROM users WHERE id = $1", params![&uid])
        .await?;
    let Some(user) = user else { return Ok(err(401, "Not authenticated")) };
    session.set("isAdmin", user.get("is_admin").cloned().unwrap_or(Value::Null));
    let f = |k: &str| user.get(k).cloned().unwrap_or(Value::Null);
    Ok(ok(json!({ "user": {
        "id": f("id"), "username": f("username"), "displayName": f("display_name"), "avatar": f("avatar"),
        "isAdmin": f("is_admin"), "totpEnabled": f("totp_enabled"),
        "hasPassword": truthy(user.get("password_hash")), "hasLockPin": truthy(user.get("lock_pin_hash")),
        "locked": session.get_bool("locked"),
    } })))
}

// ── screen lock ──────────────────────────────────────────────────────────────────────

async fn lock(Sess(session): Sess) -> Response {
    if let Err(r) = session_user(&session) {
        return r;
    }
    session.set("locked", true);
    ok(json!({ "ok": true }))
}

/// `String(req.body?.pin ?? '')`.
fn body_text(body: &Body, key: &str) -> String {
    match body.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_string(v),
    }
}

async fn unlock(State(s): State<AppState>, Sess(session): Sess, body: Body) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let pin = body_text(&body, "pin");
    if pin.is_empty() {
        return Ok(err(400, "PIN required"));
    }
    let hash =
        s.db.opt("SELECT lock_pin_hash FROM users WHERE id = $1", params![&uid])
            .await?
            .and_then(|r| r.get::<_, Option<String>>("lock_pin_hash"));
    let Some(hash) = hash.filter(|h| !h.is_empty()) else { return Ok(err(400, "No lock PIN set for this account")) };
    let key = format!("unlock:{uid}");
    if !bcrypt_verify(pin, hash).await {
        let (is_limited, _) = rate_consume(&s.redis, &key, 4, Duration::from_secs(15 * 60)).await;
        if is_limited {
            rate_reset(&s.redis, &key).await;
            s.sessions.destroy(&session).await?;
            return Ok(json(401, json!({ "error": "Too many attempts", "signedOut": true })));
        }
        return Ok(err(401, "Incorrect PIN"));
    }
    rate_reset(&s.redis, &key).await;
    session.set("locked", false);
    Ok(ok(json!({ "ok": true })))
}

fn is_lock_pin(pin: &str) -> bool {
    (4..=6).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

async fn current_pin_ok(s: &AppState, uid: &str, body: &Body) -> Result<Option<bool>> {
    let hash =
        s.db.opt("SELECT lock_pin_hash FROM users WHERE id = $1", params![uid])
            .await?
            .and_then(|r| r.get::<_, Option<String>>("lock_pin_hash"));
    let Some(hash) = hash.filter(|h| !h.is_empty()) else { return Ok(None) };
    let current = body_text(body, "currentPin");
    Ok(Some(!current.is_empty() && bcrypt_verify(current, hash).await))
}

async fn set_lock_pin(State(s): State<AppState>, Sess(session): Sess, body: Body) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let pin = body_text(&body, "pin");
    if !is_lock_pin(&pin) {
        return Ok(err(400, "PIN must be 4 to 6 digits"));
    }
    if current_pin_ok(&s, &uid, &body).await? == Some(false) {
        return Ok(err(403, "Current PIN is incorrect"));
    }
    let hash = bcrypt_hash(pin, 10).await?;
    s.db.exec("UPDATE users SET lock_pin_hash = $1 WHERE id = $2", params![hash, &uid]).await?;
    Ok(ok(json!({ "ok": true })))
}

async fn clear_lock_pin(State(s): State<AppState>, Sess(session): Sess, body: Body) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    match current_pin_ok(&s, &uid, &body).await? {
        None => return Ok(ok(json!({ "ok": true }))),
        Some(false) => return Ok(err(403, "Current PIN is incorrect")),
        Some(true) => {}
    }
    s.db.exec("UPDATE users SET lock_pin_hash = NULL WHERE id = $1", params![&uid]).await?;
    Ok(ok(json!({ "ok": true })))
}

// ── profile ──────────────────────────────────────────────────────────────────────────

async fn update_profile(State(s): State<AppState>, Sess(session): Sess, body: Body) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let Some(name) = body.get("displayName") else { return Ok(err(400, "Nothing to update")) };
    let trimmed = js_slice(js_trim(&js_string(name)), 100);
    let value = (!trimmed.is_empty()).then_some(trimmed);
    s.db.exec("UPDATE users SET display_name = $1 WHERE id = $2", params![value.clone(), &uid]).await?;
    Ok(ok(json!({ "ok": true, "displayName": value })))
}

async fn set_avatar(State(s): State<AppState>, Sess(session): Sess, body: Body) -> Result<Response> {
    static FORMAT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^data:image/(jpeg|png|gif|webp);base64,").unwrap());
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let Some(avatar) = body.str("avatar").filter(|a| !a.is_empty()) else { return Ok(err(400, "Invalid avatar")) };
    if !FORMAT.is_match(avatar) {
        return Ok(err(400, "Invalid image format"));
    }
    if js_len(avatar) > 512 * 1024 {
        return Ok(err(400, "Image too large"));
    }
    s.db.exec("UPDATE users SET avatar = $1 WHERE id = $2", params![avatar, &uid]).await?;
    Ok(ok(json!({ "ok": true })))
}

async fn clear_avatar(State(s): State<AppState>, Sess(session): Sess) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    s.db.exec("UPDATE users SET avatar = NULL WHERE id = $1", params![&uid]).await?;
    Ok(ok(json!({ "ok": true })))
}

async fn registration_status(State(s): State<AppState>) -> Result<Response> {
    let rows =
        s.db.rows(
            "SELECT key, value FROM system_settings WHERE key IN ('registration_open', 'internal_auth_disabled')",
            params![],
        )
        .await?;
    let get = |k: &str| rows.iter().find(|r| r.get::<_, String>("key") == k).map(|r| r.get::<_, String>("value"));
    let env_off = password_login_disabled_by_env();
    Ok(ok(json!({
        "open": !env_off && get("registration_open").as_deref() == Some("true"),
        "internalAuthDisabled": env_off || get("internal_auth_disabled").as_deref() == Some("true"),
    })))
}

async fn invite(State(s): State<AppState>, Path(token): Path<String>) -> Result<Response> {
    let row =
        s.db.json_opt(
            "SELECT email, expires_at FROM invites WHERE token = $1 AND used_at IS NULL AND expires_at > NOW()",
            params![token],
        )
        .await?;
    let Some(row) = row else { return Ok(err(404, "Invalid or expired invite")) };
    Ok(ok(json!({ "valid": true, "email": row.get("email") })))
}

// ── preferences ──────────────────────────────────────────────────────────────────────

async fn get_preferences(State(s): State<AppState>, Sess(session): Sess) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let uid_param = [Val::from(&uid)];
    let (prefs, css) = tokio::join!(
        s.db.json_opt("SELECT preferences FROM users WHERE id = $1", &uid_param),
        s.db.opt("SELECT value FROM system_settings WHERE key = 'custom_css'", params![])
    );
    let mut prefs = match prefs?.and_then(|r| r.get("preferences").cloned()) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    if let Some(css) = css?.map(|r| r.get::<_, String>("value")).filter(|c| !c.is_empty()) {
        prefs.insert("customCss".into(), Value::String(css));
    }
    Ok(ok(Value::Object(prefs)))
}

/// JS `Number(value)` for a JSON value.
fn js_to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => crate::services::dav_xml::js_number(js_trim(s)),
        Some(a @ Value::Array(_)) => crate::services::dav_xml::js_number(js_trim(&js_string(a))),
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// `parseInt(value)` (of `String(value)`), `None` for NaN.
fn js_parse_int(v: Option<&Value>) -> Option<i64> {
    let text = match v {
        None => "undefined".to_owned(),
        Some(v) => js_string(v),
    };
    inboxora_pim::ical::js_parse_int(&text)
}

fn defined(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

fn json_text(v: Option<&Value>) -> Option<String> {
    defined(v).map(inboxora_core::json::stringify)
}

fn valid_work_time(v: Option<&Value>) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^(?:[01][0-9]|2[0-3]):[0-5][0-9]$").unwrap());
    v.and_then(Value::as_str).is_some_and(|s| RE.is_match(s))
}

fn work_minutes(v: &Value) -> i64 {
    let s = v.as_str().unwrap_or("");
    let (h, m) = s.split_once(':').unwrap_or(("0", "0"));
    h.parse::<i64>().unwrap_or(0) * 60 + m.parse::<i64>().unwrap_or(0)
}

fn valid_work_range(start: Option<&Value>, end: Option<&Value>) -> bool {
    valid_work_time(start) && valid_work_time(end) && work_minutes(start.unwrap()) < work_minutes(end.unwrap())
}

fn sanitize_collapsed(v: Option<&Value>) -> Option<Map<String, Value>> {
    let Value::Object(m) = v? else { return None };
    let mut clean = Map::new();
    for (k, v) in m {
        if k.is_empty() || js_len(k) > 40 {
            continue;
        }
        clean.insert(k.clone(), Value::Bool(truthy(Some(v))));
        if clean.len() >= 20 {
            break;
        }
    }
    Some(clean)
}

fn sanitize_pet_slug(v: Option<&Value>) -> Option<String> {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,63}$").unwrap());
    let s = js_trim(v?.as_str()?).to_lowercase();
    if s.is_empty() {
        return Some(String::new());
    }
    RE.is_match(&s).then_some(s)
}

fn sanitize_theme_name(v: Option<&Value>) -> Option<String> {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-z0-9_]{1,64}$").unwrap());
    let s = js_trim(v?.as_str()?);
    RE.is_match(s).then(|| s.to_owned())
}

async fn patch_preferences(State(s): State<AppState>, Sess(session): Sess, body: Body) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let b = |k: &str| body.get(k);
    for name in ["conversation_list_view_enabled", "conversation_reader_view_enabled"] {
        if b(name).is_some_and(|v| !v.is_boolean()) {
            return Ok(err(400, &format!("{name} must be a boolean")));
        }
    }
    let collapsed = sanitize_collapsed(b("gtdCollapsedSections"));
    let pet_slug = sanitize_pet_slug(b("gtdPetSlug"));
    let right_width = js_parse_int(b("rightSidebarWidth")).filter(|n| (200..=600).contains(n));
    let right_hidden = b("rightSidebarHidden").and_then(Value::as_bool);
    let theme_mode =
        b("themeMode").and_then(Value::as_str).filter(|m| matches!(*m, "system" | "light" | "dark")).map(str::to_owned);
    let theme_light = sanitize_theme_name(b("themeLight"));
    let theme_dark = sanitize_theme_name(b("themeDark"));
    if b("themeMode").is_some() && theme_mode.is_none() {
        return Ok(err(400, "themeMode must be system, light or dark"));
    }
    if b("themeLight").is_some() && theme_light.is_none() {
        return Ok(err(400, "themeLight must be a lowercase theme identifier"));
    }
    if b("themeDark").is_some() && theme_dark.is_none() {
        return Ok(err(400, "themeDark must be a lowercase theme identifier"));
    }
    let font_size = defined(b("fontSize")).map(js_string);
    let reply_default =
        b("replyDefault").and_then(Value::as_str).filter(|r| matches!(*r, "reply" | "replyAll")).map(str::to_owned);
    let sidebar_width = js_parse_int(b("sidebarWidth")).filter(|n| (160..=400).contains(n)).map(|n| n.to_string());
    let mark_read = b("markReadBehavior")
        .and_then(Value::as_str)
        .filter(|r| matches!(*r, "immediate" | "delay" | "manual"))
        .map(str::to_owned);
    let mark_delay = js_parse_int(b("markReadDelay")).filter(|n| (1..=10).contains(n)).map(|n| n.to_string());
    let lock_n = js_to_number(b("autoLockMinutes"));
    let auto_lock =
        [0.0, 1.0, 5.0, 15.0, 30.0].contains(&lock_n).then(|| inboxora_core::json::js_number_string(lock_n));
    let folder_sync_n = js_to_number(b("folderSyncInterval"));
    let folder_sync = (defined(b("folderSyncInterval")).is_some()
        && [0.0, 900.0, 1800.0, 3600.0].contains(&folder_sync_n))
    .then(|| inboxora_core::json::js_number_string(folder_sync_n));
    let ai_actions = b("aiActions").and_then(Value::as_array).map(|list| {
        let clean: Vec<Value> = list
            .iter()
            .take(30)
            .map(|a| {
                let field = |k: &str, n: usize| {
                    js_slice(&a.get(k).filter(|v| !v.is_null()).map(js_string).unwrap_or_default(), n)
                };
                json!({ "id": field("id", 64), "label": field("label", 60), "prompt": field("prompt", 2000) })
            })
            .filter(|a| ["id", "label", "prompt"].iter().all(|k| a[*k].as_str().is_some_and(|s| !s.is_empty())))
            .collect();
        inboxora_core::json::stringify(&Value::Array(clean))
    });
    let has_favicons = body.has("senderFavicons");
    if has_favicons && !b("senderFavicons").is_some_and(Value::is_boolean) {
        return Ok(err(400, "senderFavicons must be a boolean"));
    }
    if let Some(v) = b("calendarWeekStartsOn")
        && !v.as_f64().is_some_and(|n| n == 0.0 || n == 1.0)
    {
        return Ok(err(400, "calendarWeekStartsOn must be 0 or 1"));
    }
    if let Some(v) = b("mobileNavigationPosition")
        && !matches!(v.as_str(), Some("top" | "bottom"))
    {
        return Ok(err(400, "mobileNavigationPosition must be top or bottom"));
    }
    if let Some(v) = b("visibleCalendarIds") {
        let ok_ids = v
            .as_array()
            .is_some_and(|a| a.len() <= 100 && a.iter().all(|id| id.as_str().is_some_and(|s| js_len(s) <= 128)));
        if !ok_ids {
            return Ok(err(400, "visibleCalendarIds must be an array of calendar identifiers"));
        }
    }
    if let Some(v) = b("calendarWorkDays") {
        let valid = v.as_array().is_some_and(|a| {
            let days: Option<Vec<i64>> = a
                .iter()
                .map(|d| d.as_f64().filter(|n| n.fract() == 0.0 && (0.0..=6.0).contains(n)).map(|n| n as i64))
                .collect();
            days.is_some_and(|d| {
                let mut u = d.clone();
                u.sort_unstable();
                u.dedup();
                !d.is_empty() && d.len() <= 7 && u.len() == d.len()
            })
        });
        if !valid {
            return Ok(err(400, "calendarWorkDays must contain unique weekday numbers from 0 to 6"));
        }
    }
    let (start, end) = (b("calendarWorkHoursStart"), b("calendarWorkHoursEnd"));
    if start.is_some() && !valid_work_time(start) {
        return Ok(err(400, "calendarWorkHoursStart must be a valid HH:mm time"));
    }
    if end.is_some() && !valid_work_time(end) {
        return Ok(err(400, "calendarWorkHoursEnd must be a valid HH:mm time"));
    }
    let mut persisted_start = start.cloned();
    let mut persisted_end = end.cloned();
    if start.is_some() || end.is_some() {
        if start.is_some() && end.is_some() {
            if !valid_work_range(start, end) {
                return Ok(err(400, "calendar work hours must be a strictly increasing same-day range"));
            }
        } else {
            let current =
                s.db.json_opt("SELECT preferences FROM users WHERE id = $1", params![&uid])
                    .await?
                    .and_then(|r| r.get("preferences").cloned())
                    .unwrap_or(Value::Null);
            let (cs, ce) = (current.get("calendarWorkHoursStart"), current.get("calendarWorkHoursEnd"));
            let valid = valid_work_range(cs, ce);
            let base_start = if valid { cs.cloned().unwrap_or(Value::Null) } else { json!("09:00") };
            let base_end = if valid { ce.cloned().unwrap_or(Value::Null) } else { json!("17:00") };
            persisted_start = Some(start.cloned().unwrap_or(base_start));
            persisted_end = Some(end.cloned().unwrap_or(base_end));
            if !valid_work_range(persisted_start.as_ref(), persisted_end.as_ref()) {
                return Ok(err(400, "calendar work hours must be a strictly increasing same-day range"));
            }
        }
    }
    if b("conversationNewestFirst").is_some_and(|v| !v.is_boolean()) {
        return Ok(err(400, "conversationNewestFirst must be a boolean"));
    }
    if let Some(v) = b("calendarInviteAccountId") {
        let Some(id) = v.as_str().filter(|id| js_len(id) <= 128) else {
            return Ok(err(400, "calendarInviteAccountId must be an account identifier"));
        };
        if !id.is_empty()
            && s.db
                .opt("SELECT 1 FROM email_accounts WHERE id::text = $1 AND user_id = $2", params![id, &uid])
                .await?
                .is_none()
        {
            return Ok(err(400, "calendarInviteAccountId must be one of your accounts"));
        }
    }
    let visible = b("visibleCalendarIds").and_then(Value::as_array).map(|ids| {
        let mut seen: Vec<Value> = Vec::new();
        for id in ids {
            if !seen.contains(id) {
                seen.push(id.clone());
            }
        }
        inboxora_core::json::stringify(&Value::Array(seen))
    });
    let raw = |k: &str| Val::from(b(k).cloned().unwrap_or(Value::Null));
    let opt_text = |v: Option<String>| Val::from(v);
    let p: Vec<Val> = vec![
        Val::from(&uid),
        raw("theme"),
        raw("font"),
        raw("layout"),
        raw("notificationSound"),
        raw("pageSize"),
        raw("scrollMode"),
        raw("syncInterval"),
        raw("blockRemoteImages"),
        opt_text(json_text(b("imageWhitelist"))),
        opt_text(json_text(b("shortcuts"))),
        opt_text(json_text(b("hiddenFolders"))),
        raw("language"),
        raw("threadedView"),
        raw("plaintextEmail"),
        raw("hoverQuickActions"),
        opt_text(json_text(b("swipeActions"))),
        opt_text(json_text(b("expandedAccounts"))),
        opt_text(json_text(b("collapsedFolders"))),
        opt_text(json_text(b("favoriteFolders"))),
        opt_text(json_text(b("recentFolders"))),
        opt_text(font_size),
        raw("showAppBadge"),
        opt_text(reply_default),
        opt_text(sidebar_width),
        raw("categorizationEnabled"),
        opt_text(mark_read),
        opt_text(mark_delay),
        opt_text(ai_actions),
        Val::from(right_width),
        Val::from(right_hidden),
        opt_text(collapsed.map(|c| inboxora_core::json::stringify(&Value::Object(c)))),
        opt_text(pet_slug),
        opt_text(auto_lock),
        raw("showMobileAvatars"),
        raw("gravatarAvatars"),
        opt_text(folder_sync.clone()),
        opt_text(json_text(b("folderOrder"))),
        Val::from(if has_favicons { b("senderFavicons").and_then(Value::as_bool) } else { None }),
        raw("showMessagePreviews"),
        raw("conversation_list_view_enabled"),
        raw("conversation_reader_view_enabled"),
        raw("calendarWeekStartsOn"),
        raw("mobileNavigationPosition"),
        opt_text(visible),
        opt_text(b("calendarWorkDays").map(inboxora_core::json::stringify)),
        Val::from(persisted_start.filter(|v| !v.is_null())),
        Val::from(persisted_end.filter(|v| !v.is_null())),
        opt_text(theme_mode),
        opt_text(theme_light),
        opt_text(theme_dark),
        raw("calendarInviteAccountId"),
        raw("conversationNewestFirst"),
    ];
    s.db.exec(PREFERENCES_SQL, &p).await?;
    if let Some(v) = defined(b("categorizationEnabled")) {
        let _ = v;
        crate::services::categorizer::invalidate_global_categorization_cache(&uid);
    }
    // Sync-interval changes reach the mail engine through its hooks once it runs here.
    let _ = folder_sync;
    Ok(ok(json!({ "ok": true })))
}

const PREFERENCES_SQL: &str = "UPDATE users SET preferences = preferences
  || CASE WHEN $2::text IS NOT NULL THEN jsonb_build_object('theme', $2::text) ELSE '{}'::jsonb END
  || CASE WHEN $3::text IS NOT NULL THEN jsonb_build_object('font', $3::text) ELSE '{}'::jsonb END
  || CASE WHEN $4::text IS NOT NULL THEN jsonb_build_object('layout', $4::text) ELSE '{}'::jsonb END
  || CASE WHEN $5::text IS NOT NULL THEN jsonb_build_object('notificationSound', $5::text) ELSE '{}'::jsonb END
  || CASE WHEN $6::text IS NOT NULL THEN jsonb_build_object('pageSize', $6::text) ELSE '{}'::jsonb END
  || CASE WHEN $7::text IS NOT NULL THEN jsonb_build_object('scrollMode', $7::text) ELSE '{}'::jsonb END
  || CASE WHEN $8::text IS NOT NULL THEN jsonb_build_object('syncInterval', $8::text) ELSE '{}'::jsonb END
  || CASE WHEN $9::boolean IS NOT NULL THEN jsonb_build_object('blockRemoteImages', $9::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $10::jsonb IS NOT NULL THEN jsonb_build_object('imageWhitelist', $10::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $11::jsonb IS NOT NULL THEN jsonb_build_object('shortcuts', $11::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $12::jsonb IS NOT NULL THEN jsonb_build_object('hiddenFolders', $12::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $13::text IS NOT NULL THEN jsonb_build_object('language', $13::text) ELSE '{}'::jsonb END
  || CASE WHEN $14::boolean IS NOT NULL THEN jsonb_build_object('threadedView', $14::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $15::boolean IS NOT NULL THEN jsonb_build_object('plaintextEmail', $15::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $16::boolean IS NOT NULL THEN jsonb_build_object('hoverQuickActions', $16::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $17::jsonb IS NOT NULL THEN jsonb_build_object('swipeActions', $17::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $18::jsonb IS NOT NULL THEN jsonb_build_object('expandedAccounts', $18::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $19::jsonb IS NOT NULL THEN jsonb_build_object('collapsedFolders', $19::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $20::jsonb IS NOT NULL THEN jsonb_build_object('favoriteFolders', $20::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $21::jsonb IS NOT NULL THEN jsonb_build_object('recentFolders', $21::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $22::text IS NOT NULL THEN jsonb_build_object('fontSize', $22::text) ELSE '{}'::jsonb END
  || CASE WHEN $23::boolean IS NOT NULL THEN jsonb_build_object('showAppBadge', $23::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $24::text IS NOT NULL THEN jsonb_build_object('replyDefault', $24::text) ELSE '{}'::jsonb END
  || CASE WHEN $25::text IS NOT NULL THEN jsonb_build_object('sidebarWidth', $25::text) ELSE '{}'::jsonb END
  || CASE WHEN $26::boolean IS NOT NULL THEN jsonb_build_object('categorizationEnabled', $26::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $27::text IS NOT NULL THEN jsonb_build_object('markReadBehavior', $27::text) ELSE '{}'::jsonb END
  || CASE WHEN $28::text IS NOT NULL THEN jsonb_build_object('markReadDelay', $28::text) ELSE '{}'::jsonb END
  || CASE WHEN $29::jsonb IS NOT NULL THEN jsonb_build_object('aiActions', $29::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $30::int IS NOT NULL THEN jsonb_build_object('rightSidebarWidth', $30::int) ELSE '{}'::jsonb END
  || CASE WHEN $31::boolean IS NOT NULL THEN jsonb_build_object('rightSidebarHidden', $31::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $32::jsonb IS NOT NULL THEN jsonb_build_object('gtdCollapsedSections', $32::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $33::text IS NOT NULL THEN jsonb_build_object('gtdPetSlug', $33::text) ELSE '{}'::jsonb END
  || CASE WHEN $34::text IS NOT NULL THEN jsonb_build_object('autoLockMinutes', $34::text) ELSE '{}'::jsonb END
  || CASE WHEN $35::boolean IS NOT NULL THEN jsonb_build_object('showMobileAvatars', $35::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $36::boolean IS NOT NULL THEN jsonb_build_object('gravatarAvatars', $36::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $37::text IS NOT NULL THEN jsonb_build_object('folderSyncInterval', $37::text) ELSE '{}'::jsonb END
  || CASE WHEN $38::jsonb IS NOT NULL THEN jsonb_build_object('folderOrder', $38::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $39::boolean IS NOT NULL THEN jsonb_build_object('senderFavicons', $39::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $40::boolean IS NOT NULL THEN jsonb_build_object('showMessagePreviews', $40::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $41::boolean IS NOT NULL THEN jsonb_build_object('conversation_list_view_enabled', $41::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $42::boolean IS NOT NULL THEN jsonb_build_object('conversation_reader_view_enabled', $42::boolean) ELSE '{}'::jsonb END
  || CASE WHEN $43::int IS NOT NULL THEN jsonb_build_object('calendarWeekStartsOn', $43::int) ELSE '{}'::jsonb END
  || CASE WHEN $44::text IS NOT NULL THEN jsonb_build_object('mobileNavigationPosition', $44::text) ELSE '{}'::jsonb END
  || CASE WHEN $45::jsonb IS NOT NULL THEN jsonb_build_object('visibleCalendarIds', $45::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $46::jsonb IS NOT NULL THEN jsonb_build_object('calendarWorkDays', $46::jsonb) ELSE '{}'::jsonb END
  || CASE WHEN $47::text IS NOT NULL THEN jsonb_build_object('calendarWorkHoursStart', $47::text) ELSE '{}'::jsonb END
  || CASE WHEN $48::text IS NOT NULL THEN jsonb_build_object('calendarWorkHoursEnd', $48::text) ELSE '{}'::jsonb END
  || CASE WHEN $49::text IS NOT NULL THEN jsonb_build_object('themeMode', $49::text) ELSE '{}'::jsonb END
  || CASE WHEN $50::text IS NOT NULL THEN jsonb_build_object('themeLight', $50::text) ELSE '{}'::jsonb END
  || CASE WHEN $51::text IS NOT NULL THEN jsonb_build_object('themeDark', $51::text) ELSE '{}'::jsonb END
  || CASE WHEN $52::text IS NOT NULL THEN jsonb_build_object('calendarInviteAccountId', $52::text) ELSE '{}'::jsonb END
  || CASE WHEN $53::boolean IS NOT NULL THEN jsonb_build_object('conversationNewestFirst', $53::boolean) ELSE '{}'::jsonb END
WHERE id = $1";

async fn whitelist_add(State(s): State<AppState>, Sess(session): Sess, body: Body) -> Result<Response> {
    static DOMAIN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*\.[a-z]{2,}$")
            .unwrap()
    });
    static EMAIL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").unwrap());
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let kind = body.str("type");
    let value = body.str("value").filter(|v| !js_trim(v).is_empty());
    let (Some(kind @ ("address" | "domain")), Some(value)) = (kind, value) else {
        return Ok(err(400, "type must be \"address\" or \"domain\" and value must be a non-empty string"));
    };
    let normalized = js_trim(value).to_lowercase();
    if kind == "domain" {
        let bare = normalized.strip_prefix('.').unwrap_or(&normalized);
        if !DOMAIN.is_match(bare) {
            return Ok(err(400, "Invalid domain format"));
        }
    } else if !EMAIL.is_match(&normalized) {
        return Ok(err(400, "Invalid email address format"));
    }
    let key = if kind == "address" { "addresses" } else { "domains" };
    s.db.exec(
        "UPDATE users SET preferences = jsonb_set(
           jsonb_set(COALESCE(preferences, '{}'::jsonb), '{imageWhitelist}', COALESCE(preferences->'imageWhitelist', '{}'::jsonb)),
           ARRAY['imageWhitelist', $2::text],
           (SELECT COALESCE(jsonb_agg(DISTINCT val), '[]'::jsonb)
              FROM jsonb_array_elements_text(COALESCE(preferences->'imageWhitelist'->$2::text, '[]'::jsonb) || jsonb_build_array($3::text)) AS val))
         WHERE id = $1",
        params![&uid, key, normalized],
    )
    .await?;
    Ok(ok(json!({ "ok": true })))
}

async fn get_recovery_email(State(s): State<AppState>, Sess(session): Sess) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let email =
        s.db.opt("SELECT recovery_email FROM users WHERE id = $1", params![&uid])
            .await?
            .and_then(|r| r.get::<_, Option<String>>("recovery_email"))
            .filter(|e| !e.is_empty());
    Ok(ok(json!({ "email": email })))
}

fn is_email(s: &str) -> bool {
    static EMAIL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").unwrap());
    EMAIL.is_match(s)
}

async fn set_recovery_email(State(s): State<AppState>, Sess(session): Sess, body: Body) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let Some(email) = body.get("email") else { return Ok(err(400, "email required")) };
    let trimmed = truthy(Some(email)).then(|| js_trim(&js_string(email)).to_lowercase());
    if let Some(t) = trimmed.as_deref().filter(|t| !t.is_empty())
        && !is_email(t)
    {
        return Ok(err(400, "Invalid email address"));
    }
    s.db.exec("UPDATE users SET recovery_email = $1 WHERE id = $2", params![trimmed.filter(|t| !t.is_empty()), &uid])
        .await?;
    Ok(ok(json!({ "ok": true })))
}

// ── password reset ───────────────────────────────────────────────────────────────────

const RESET_HTML: &str = r#"
        <div style="font-family:-apple-system,Arial,sans-serif;max-width:480px;margin:0 auto;padding:32px;color:#1a1a1a;">
          <div style="margin-bottom:24px;">
            <span style="font-size:22px;font-weight:700;color:#1a1a1a;">Mail</span><span style="font-size:22px;font-weight:600;color:#7c6af7;">Flow</span>
          </div>
          <h2 style="margin:0 0 12px;font-size:18px;font-weight:600;">Reset your password</h2>
          <p style="color:#555;line-height:1.6;margin:0 0 24px;">Click the button below to set a new password. This link expires in 1 hour.</p>
          <a href="{url}" style="display:inline-block;padding:12px 24px;background:#7c6af7;color:white;border-radius:8px;text-decoration:none;font-weight:500;font-size:14px;margin-bottom:24px;">Reset password</a>
          <p style="color:#999;font-size:12px;margin:0;">If you did not request a password reset, you can ignore this email. Your password will not change.</p>
        </div>
      "#;

/// The transport for a reset email: the system mailer, else the user's (or an admin's) account.
async fn reset_transport(
    s: &AppState,
    user_id: &str,
) -> std::result::Result<Option<(smtp::Transport, String, String)>, String> {
    if let Ok((t, name, email)) = smtp::system_transport(&s.db).await {
        return Ok(Some((t, format!("{name} <{email}>"), email)));
    }
    let acct =
        s.db.json_opt(
            "SELECT ea.* FROM email_accounts ea JOIN users u ON ea.user_id = u.id
             WHERE ea.enabled = true AND ea.smtp_host IS NOT NULL AND (ea.user_id = $1 OR u.is_admin = true)
             ORDER BY (ea.user_id = $1) DESC, ea.created_at LIMIT 1",
            params![user_id],
        )
        .await
        .map_err(|e| e.to_string())?;
    let Some(a) = acct else { return Ok(None) };
    let text = |k: &str| a.get(k).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.is_empty());
    let provider = text("oauth_provider");
    let auth = match (provider.as_deref(), text("oauth_access_token")) {
        (Some("microsoft" | "google"), Some(token)) => smtp::Auth::OAuth2 {
            user: text("auth_user").or_else(|| text("email_address")).unwrap_or_default(),
            token: crypto::decrypt(&token).unwrap_or_default(),
        },
        _ => smtp::Auth::Password {
            user: text("auth_user").unwrap_or_default(),
            pass: text("auth_pass").and_then(|p| crypto::decrypt(&p)).unwrap_or_default(),
        },
    };
    let policy = crate::services::dav_client::connection_policy(&s.db).await.map_err(|e| e.to_string())?;
    let resolved = smtp::resolve(&text("smtp_host").unwrap_or_default(), policy.allow_private_hosts).await?;
    let port = a.get("smtp_port").and_then(Value::as_u64).unwrap_or(587) as u16;
    let skip_verify = a.get("imap_skip_tls_verify").and_then(Value::as_bool).unwrap_or(false);
    let transport = smtp::Transport {
        resolved,
        port,
        secure: port == 465,
        require_tls: false,
        ignore_tls: false,
        reject_unauthorized: if policy.allow_insecure_tls { !skip_verify } else { true },
        auth,
    };
    let email = text("email_address").unwrap_or_default();
    Ok(Some((transport, format!("{} <{email}>", text("name").unwrap_or_default()), email)))
}

async fn forgot_password(State(s): State<AppState>, ClientIp(ip): ClientIp, body: Body) -> Result<Response> {
    if let Err(r) = auth_limit(&s, &ip).await {
        return Ok(r);
    }
    if password_login_disabled(&s).await? {
        return Ok(err(403, "Password login is disabled on this server."));
    }
    let Some(email) = body.str("email").filter(|e| !e.is_empty()) else { return Ok(err(400, "Email required")) };
    let trimmed = js_trim(email).to_lowercase();
    if !is_email(&trimmed) {
        return Ok(err(400, "Invalid email address"));
    }
    let run = async {
        let user =
            s.db.json_opt("SELECT id, password_hash FROM users WHERE recovery_email = $1", params![&trimmed])
                .await
                .map_err(|e| e.to_string())?;
        let Some(user) = user.filter(|u| truthy(u.get("password_hash"))) else { return Ok(()) };
        let uid = str_of(&user, "id").unwrap_or_default();
        let token = crypto::random_hex(32);
        let expires = chrono::Utc::now() + chrono::Duration::hours(1);
        let url = format!("{}/?reset_token={token}", inboxora_core::config::env("APP_URL").unwrap_or_default());
        let text = format!(
            "You requested a password reset for your Inboxora account.\n\nClick the link below to set a new password. This link expires in 1 hour.\n\n{url}\n\nIf you did not request this, you can ignore this email."
        );
        let Some((transport, from_header, from_email)) = reset_transport(&s, &uid).await? else {
            return Err("No email transport available".to_owned());
        };
        let (name, _) = from_header.rsplit_once(" <").unwrap_or((&from_header, ""));
        let message = smtp::simple_message(
            name,
            &from_email,
            &trimmed,
            "Reset your Inboxora password",
            &text,
            Some(&RESET_HTML.replace("{url}", &url)),
        )
        .map_err(|e| e.message)?;
        transport.send_raw(&from_email, std::slice::from_ref(&trimmed), &message).await.map_err(|e| e.message)?;
        s.db.exec("DELETE FROM password_reset_tokens WHERE user_id = $1", params![&uid])
            .await
            .map_err(|e| e.to_string())?;
        s.db.exec(
            "INSERT INTO password_reset_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3)",
            params![&uid, crypto::sha256_hex(&token), expires],
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    };
    if let Err(e) = run.await {
        tracing::error!("forgot-password error: {e}");
    }
    Ok(ok(json!({ "ok": true })))
}

async fn reset_password(State(s): State<AppState>, ClientIp(ip): ClientIp, body: Body) -> Result<Response> {
    if let Err(r) = auth_limit(&s, &ip).await {
        return Ok(r);
    }
    let Some(token) = body.str("token").filter(|t| !t.is_empty()) else { return Ok(err(400, "Token required")) };
    let Some(password) = body.str("password").filter(|p| js_len(p) >= 8) else {
        return Ok(err(400, "Password must be at least 8 characters"));
    };
    let run = async {
        let row =
            s.db.opt(
                "DELETE FROM password_reset_tokens WHERE token_hash = $1 AND expires_at > NOW() RETURNING user_id",
                params![crypto::sha256_hex(js_trim(token))],
            )
            .await?;
        let Some(row) = row else { return Ok(err(400, "Invalid or expired reset link")) };
        let uid = row.get::<_, uuid::Uuid>("user_id").to_string();
        let hash = bcrypt_hash(password.to_owned(), 12).await?;
        s.db.exec("UPDATE users SET password_hash = $1 WHERE id = $2", params![hash, &uid]).await?;
        if let Err(e) = s.sessions.destroy_user_sessions(&uid, None).await {
            tracing::error!("destroyUserSessions failed: {e}");
        }
        s.db.exec("DELETE FROM trusted_devices WHERE user_id = $1", params![&uid]).await?;
        rate_reset(&s.redis, &format!("auth:{ip}")).await;
        Ok::<_, Error>(ok(json!({ "ok": true })))
    };
    match run.await {
        Ok(r) => Ok(r),
        Err(e) => {
            tracing::error!("reset-password error: {e}");
            Ok(err(500, "Failed to reset password"))
        }
    }
}

// ── web push ─────────────────────────────────────────────────────────────────────────

fn vapid_public_key() -> Option<String> {
    let public = inboxora_core::config::env("VAPID_PUBLIC_KEY").filter(|k| !k.is_empty())?;
    inboxora_core::config::env("VAPID_PRIVATE_KEY").filter(|k| !k.is_empty())?;
    Some(public)
}

async fn vapid_key(Sess(session): Sess) -> Response {
    if let Err(r) = session_user(&session) {
        return r;
    }
    match vapid_public_key() {
        Some(k) => ok(json!({ "publicKey": k })),
        None => err(503, "Push notifications are not configured on this server."),
    }
}

async fn push_subscribe(State(s): State<AppState>, Sess(session): Sess, body: Body) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let endpoint = body.str("endpoint").filter(|e| !e.is_empty());
    let keys = body.get("keys");
    let key = |k: &str| keys.and_then(|o| o.get(k)).and_then(Value::as_str).filter(|v| !v.is_empty());
    let (Some(endpoint), Some(p256dh), Some(auth)) = (endpoint, key("p256dh"), key("auth")) else {
        return Ok(err(400, "Invalid push subscription object."));
    };
    let Ok(url) = url::Url::parse(endpoint) else { return Ok(err(400, "Push endpoint is not a valid URL.")) };
    if url.scheme() != "https" {
        return Ok(err(400, "Push endpoint must use HTTPS."));
    }
    if inboxora_net::validate_host(&crate::services::dav_client::hostname(&url), false).await.is_some() {
        return Ok(err(400, "Push endpoint host is not allowed."));
    }
    match s
        .db
        .exec(
            "INSERT INTO push_subscriptions (user_id, endpoint, p256dh, auth) VALUES ($1, $2, $3, $4) ON CONFLICT (user_id, endpoint) DO UPDATE SET p256dh = $3, auth = $4",
            params![&uid, endpoint, p256dh, auth],
        )
        .await
    {
        Ok(_) => Ok(ok(json!({ "ok": true }))),
        Err(e) => {
            tracing::error!("push/subscribe error: {e}");
            Ok(err(500, "Failed to save push subscription."))
        }
    }
}

async fn push_unsubscribe(State(s): State<AppState>, Sess(session): Sess, body: Body) -> Result<Response> {
    let uid = match session_user(&session) {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    let Some(endpoint) = body.str("endpoint").filter(|e| !e.is_empty()) else {
        return Ok(err(400, "endpoint required."));
    };
    match s
        .db
        .exec("DELETE FROM push_subscriptions WHERE user_id = $1 AND endpoint = $2", params![&uid, endpoint])
        .await
    {
        Ok(_) => Ok(ok(json!({ "ok": true }))),
        Err(e) => {
            tracing::error!("push/unsubscribe error: {e}");
            Ok(err(500, "Failed to remove push subscription."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_hints_match_node() {
        assert_eq!(mask_email("ab@x.test"), "a*@x.test");
        assert_eq!(mask_email("alice@x.test"), "a***e@x.test");
        assert_eq!(mask_email("averyverylongname@x.test"), "a****e@x.test");
        assert_eq!(mask_email("plain"), "plain");
    }

    #[test]
    fn js_helpers() {
        assert_eq!(js_slice("😀abc", 1), "\u{fffd}");
        assert_eq!(js_to_number(Some(&Value::Null)), 0.0);
        assert_eq!(js_to_number(Some(&json!(" 15 "))), 15.0);
        assert!(js_to_number(None).is_nan());
        assert_eq!(js_parse_int(Some(&json!("300px"))), Some(300));
        assert!(is_lock_pin("1234") && !is_lock_pin("12a4") && !is_lock_pin("1234567"));
    }
}
