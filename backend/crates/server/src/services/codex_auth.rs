//! ChatGPT (OpenAI Codex) device authorization and the shared credential (port of
//! `services/openaiCodexAuth.ts`). Flows are stored encrypted in `ai_codex_device_flows`; the
//! single credential lives in `ai_codex_credentials` and is refreshed under a row lock.

use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use inboxora_core::pg::{Db, Q};
use inboxora_core::{crypto, params};
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use super::ai_http::{PUBLIC, parse_json, read_limited, sanitize_text};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const DEVICE_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_TIMEOUT_MS: i64 = 15 * 60_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_INTERVAL_MS: i64 = 5000;
const MIN_INTERVAL_MS: i64 = 1000;
const MAX_INTERVAL_MS: i64 = 60_000;
const SLOW_DOWN_MS: i64 = 5000;
const POLL_CLAIM_STALE_MS: i64 = 30_000;
const REFRESH_SKEW_MS: i64 = 60_000;
const JSON_BODY_LIMIT: usize = 256 * 1024;
const ERROR_BODY_LIMIT: usize = 8 * 1024;
const TERMINAL_REFRESH_ERRORS: [&str; 3] = ["invalid_grant", "invalid_client", "unauthorized_client"];
const CLEAR_SECRETS: &str =
    "device_auth_id_enc = NULL, user_code_enc = NULL, authorization_code_enc = NULL, code_verifier_enc = NULL";

/// `CodexAuthError`.
#[derive(Debug, Clone)]
pub struct CodexError {
    pub message: String,
    pub status: u16,
    pub code: Option<String>,
    pub transient: bool,
}

impl CodexError {
    fn new(message: impl Into<String>, status: u16) -> Self {
        CodexError { message: message.into(), status, code: None, transient: false }
    }
    fn code(mut self, code: Option<String>) -> Self {
        self.code = code.filter(|c| !c.is_empty());
        self
    }
    fn transient(mut self, t: bool) -> Self {
        self.transient = t;
        self
    }
}

impl From<inboxora_core::Error> for CodexError {
    fn from(e: inboxora_core::Error) -> Self {
        CodexError::new(e.to_string(), 500)
    }
}

type CResult<T> = Result<T, CodexError>;

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn at(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

/// `hashSessionId`.
pub fn hash_session(session_id: &str) -> CResult<String> {
    if session_id.is_empty() {
        return Err(CodexError::new("Session is required", 401));
    }
    Ok(crypto::sha256_hex(session_id))
}

/// `decodeJwtClaims`.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    let [_, payload, _] = parts.as_slice() else { return None };
    if payload.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `extractChatGptAccount`: `(accountId, email)`.
pub fn chatgpt_account(claims: Option<&Value>) -> CResult<(String, String)> {
    let claims = claims
        .filter(|c| c.is_object())
        .ok_or_else(|| CodexError::new("ChatGPT token has no account information", 400))?;
    let auth = claims.get("https://api.openai.com/auth");
    let profile = claims.get("https://api.openai.com/profile");
    let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
    let account = s(auth.and_then(|a| a.get("chatgpt_account_id")))
        .or_else(|| s(claims.get("chatgpt_account_id")))
        .unwrap_or_default();
    if account.is_empty() {
        return Err(CodexError::new("ChatGPT token has no account ID", 400));
    }
    let email = s(profile.and_then(|p| p.get("email")))
        .or_else(|| s(claims.get("email")))
        .or_else(|| s(auth.and_then(|a| a.get("email"))))
        .unwrap_or_default();
    Ok((account, email))
}

/// `maskAccountLabel`.
pub fn mask_label(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    if let Some(at) = value.find('@').filter(|a| *a > 0) {
        let first: String = value.chars().take(1).collect();
        return format!("{first}***{}", &value[at..]);
    }
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 4 {
        "••••".into()
    } else {
        format!("{}••••{}", chars[..2].iter().collect::<String>(), chars[chars.len() - 2..].iter().collect::<String>())
    }
}

fn error_code(body: Option<&Value>) -> String {
    match body.and_then(|b| b.get("error")) {
        Some(Value::String(s)) => s.clone(),
        Some(e) => e.get("code").and_then(Value::as_str).unwrap_or("").to_owned(),
        None => String::new(),
    }
}

/// `fetchAuthResponse`: status and capped body text.
async fn auth_request(
    url: &str,
    json_body: Option<Value>,
    form: Option<Vec<(&str, String)>>,
) -> CResult<(u16, String)> {
    let mut req =
        PUBLIC.post(url).timeout(REQUEST_TIMEOUT).header("originator", "mailflow").header("User-Agent", "Inboxora");
    if let Some(b) = json_body {
        req = req.header("Content-Type", "application/json").body(b.to_string());
    }
    if let Some(f) = form {
        let encoded = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(f).finish();
        req = req.header("Content-Type", "application/x-www-form-urlencoded").body(encoded);
    }
    let response = req.send().await.map_err(|e| {
        if e.is_timeout() {
            CodexError::new("ChatGPT authorization request timed out", 400).transient(true)
        } else {
            CodexError::new(format!("ChatGPT authorization network error: {}", sanitize_text(&e.to_string(), 300)), 400)
                .transient(true)
        }
    })?;
    let status = response.status().as_u16();
    let limit = if response.status().is_success() { JSON_BODY_LIMIT } else { ERROR_BODY_LIMIT };
    let text = read_limited(response, limit).await.map_err(|e| {
        CodexError::new(format!("ChatGPT authorization network error: {}", sanitize_text(&e.to_string(), 300)), 400)
            .transient(true)
    })?;
    Ok((status, text))
}

/// `intervalMilliseconds`.
fn interval_ms(raw: Option<&Value>) -> Option<i64> {
    let seconds = match raw {
        None | Some(Value::Null) => return Some(DEFAULT_INTERVAL_MS),
        Some(Value::String(s)) if s.is_empty() => return Some(DEFAULT_INTERVAL_MS),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok()?,
        Some(Value::Number(n)) => n.as_f64()?,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        _ => return None,
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(((seconds * 1000.0).round() as i64).clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS))
}

fn encrypt(v: &str) -> CResult<String> {
    crypto::encrypt(v).map_err(|e| CodexError::new(e, 500))
}

fn decrypt_json(v: &str) -> CResult<Value> {
    let plain = crypto::decrypt(v)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| CodexError::new("Stored ChatGPT authorization is unavailable", 503))?;
    serde_json::from_str(&plain).map_err(|_| CodexError::new("Stored ChatGPT authorization is corrupted", 503))
}

/// `credentialExpiry`.
fn expiry(body: &Value, claims: Option<&Value>, now: i64) -> CResult<i64> {
    let seconds = match body.get("expires_in") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    };
    if seconds.is_finite() && seconds > 0.0 {
        return Ok(now + (seconds * 1000.0) as i64);
    }
    let exp = claims.and_then(|c| c.get("exp")).and_then(Value::as_f64).unwrap_or(0.0);
    if exp.is_finite() && exp > 0.0 {
        return Ok((exp * 1000.0) as i64);
    }
    Err(CodexError::new("ChatGPT token response has no valid expiry", 400))
}

#[derive(Debug, Clone)]
struct Flow {
    id: String,
    device_auth_id_enc: Option<String>,
    user_code_enc: Option<String>,
    authorization_code_enc: Option<String>,
    code_verifier_enc: Option<String>,
    interval_ms: i64,
    expires_at: i64,
    next_poll_at: i64,
    state: String,
    failure_code: Option<String>,
    updated_at: i64,
}

fn flow_of(r: &inboxora_core::pg::Row) -> Flow {
    let t = |k: &str| r.get::<_, DateTime<Utc>>(k).timestamp_millis();
    Flow {
        id: r.get::<_, uuid::Uuid>("id").to_string(),
        device_auth_id_enc: r.get("device_auth_id_enc"),
        user_code_enc: r.get("user_code_enc"),
        authorization_code_enc: r.get("authorization_code_enc"),
        code_verifier_enc: r.get("code_verifier_enc"),
        interval_ms: i64::from(r.get::<_, i32>("interval_ms")),
        expires_at: t("expires_at"),
        next_poll_at: t("next_poll_at"),
        state: r.get("state"),
        failure_code: r.get("failure_code"),
        updated_at: t("updated_at"),
    }
}

const ACTIVE: [&str; 3] = ["pending", "polling", "authorized"];

async fn release(
    db: &Db,
    id: &str,
    state: &str,
    interval: Option<i64>,
    next_poll: Option<i64>,
    failure: Option<&str>,
    clear: bool,
) {
    let _ = db
        .exec(
            "UPDATE ai_codex_device_flows SET state = $2, interval_ms = COALESCE($3::int, interval_ms), next_poll_at = COALESCE($4, next_poll_at), failure_code = $5, \
             device_auth_id_enc = CASE WHEN $6 THEN NULL ELSE device_auth_id_enc END, user_code_enc = CASE WHEN $6 THEN NULL ELSE user_code_enc END, \
             authorization_code_enc = CASE WHEN $6 THEN NULL ELSE authorization_code_enc END, code_verifier_enc = CASE WHEN $6 THEN NULL ELSE code_verifier_enc END, \
             updated_at = NOW() WHERE id = $1 AND state IN ('polling', 'authorized')",
            params![id, state, interval.map(|i| i as i32), next_poll.map(at), failure, clear],
        )
        .await;
}

async fn release_failed(db: &Db, id: &str, e: &CodexError, state: &str) {
    let code = e
        .code
        .clone()
        .unwrap_or_else(|| if e.transient { "transient_error".into() } else { "authorization_failed".into() });
    release(db, id, state, None, None, Some(&code), state == "failed").await;
}

/// `startDeviceFlow`.
pub async fn start_device_flow(db: &Db, user: &str, session_id: &str) -> CResult<Value> {
    let session = hash_session(session_id)?;
    let (status, text) = auth_request(DEVICE_CODE_URL, Some(json!({ "client_id": CLIENT_ID })), None).await?;
    if !(200..300).contains(&status) {
        return Err(
            CodexError::new(format!("ChatGPT device code request failed ({status})"), 502).transient(status >= 500)
        );
    }
    let body = parse_json(&text).unwrap_or(Value::Null);
    let interval = interval_ms(body.get("interval"));
    let device = body.get("device_auth_id").and_then(Value::as_str).filter(|s| !s.is_empty());
    let code_value =
        body.get("user_code").filter(|v| inboxora_core::json::truthy(Some(v))).or_else(|| body.get("usercode"));
    let user_code = code_value.and_then(Value::as_str).filter(|s| !s.is_empty());
    let (Some(device), Some(user_code), Some(interval)) = (device, user_code, interval) else {
        return Err(CodexError::new("Invalid device code response", 502));
    };
    let started = now_ms();
    let (device_enc, code_enc) = (encrypt(device)?, encrypt(user_code)?);
    let (u, s) = (user.to_owned(), session.clone());
    let row = db
        .transaction(false, 2, move |tx| {
            let (user, session, device_enc, code_enc) = (u.clone(), s.clone(), device_enc.clone(), code_enc.clone());
            Box::pin(async move {
                tx.exec("SELECT id FROM users WHERE id = $1 FOR UPDATE", params![&user]).await?;
                tx.exec(
                    &format!("UPDATE ai_codex_device_flows SET state = 'cancelled', {CLEAR_SECRETS}, updated_at = NOW() WHERE admin_user_id = $1 AND session_hash = $2 AND state IN ('pending', 'polling', 'authorized')"),
                    params![&user, &session],
                )
                .await?;
                tx.exec("DELETE FROM ai_codex_device_flows WHERE expires_at < NOW() - INTERVAL '1 day'", params![]).await?;
                let row = tx
                    .opt(
                        "INSERT INTO ai_codex_device_flows (admin_user_id, session_hash, device_auth_id_enc, user_code_enc, interval_ms, expires_at, next_poll_at, state, created_at, updated_at) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending', $8, $8) RETURNING *",
                        params![&user, &session, &device_enc, &code_enc, interval as i32, at(started + DEVICE_TIMEOUT_MS), at(started + interval), at(started)],
                    )
                    .await?;
                Ok(row.map(|r| flow_of(&r)))
            })
        })
        .await?
        .ok_or_else(|| CodexError::new("Could not start the device flow", 500))?;
    Ok(
        json!({ "flowId": row.id, "userCode": user_code, "verificationUrl": DEVICE_URL, "intervalMs": interval, "expiresAt": row.expires_at, "status": "pending" }),
    )
}

enum Claim {
    NotFound,
    Terminal(String, Option<String>),
    Waiting(i64),
    Claimed(Flow),
}

fn terminal(state: &str, failure: Option<&str>) -> Value {
    match state {
        "completed" => json!({ "status": "connected" }),
        "failed" => {
            json!({ "status": "failed", "reconnectRequired": true, "reason": failure.unwrap_or("authorization_failed") })
        }
        s => json!({ "status": s }),
    }
}

async fn exchange(db: &Db, flow: &Flow) -> CResult<Value> {
    let unavailable = || CodexError::new("Stored ChatGPT exchange code is unavailable", 400);
    let code = flow.authorization_code_enc.as_deref().and_then(crypto::decrypt).ok_or_else(unavailable)?;
    let verifier = flow.code_verifier_enc.as_deref().and_then(crypto::decrypt).ok_or_else(unavailable)?;
    let (status, text) = auth_request(
        TOKEN_URL,
        None,
        Some(vec![
            ("grant_type", "authorization_code".into()),
            ("client_id", CLIENT_ID.into()),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", DEVICE_REDIRECT_URI.into()),
        ]),
    )
    .await?;
    if !(200..300).contains(&status) {
        let code = error_code(parse_json(&text).as_ref());
        return Err(CodexError::new(format!("ChatGPT token exchange failed ({status})"), 502)
            .code(Some(sanitize_text(&code, 100)))
            .transient(status >= 500));
    }
    let body = parse_json(&text).unwrap_or(Value::Null);
    let (Some(access), Some(refresh)) = (
        body.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty()),
        body.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()),
    ) else {
        return Err(CodexError::new("Invalid ChatGPT token exchange response", 502));
    };
    let claims = jwt_claims(access);
    let (account, email) = chatgpt_account(claims.as_ref())?;
    let payload = json!({
        "state": "connected", "accessToken": access, "refreshToken": refresh, "expiresAt": expiry(&body, claims.as_ref(), now_ms())?,
        "accountId": account, "accountLabel": mask_label(if email.is_empty() { &account } else { &email }), "failureCode": null,
    });
    let encrypted = encrypt(&payload.to_string())?;
    let id = flow.id.clone();
    let completed = db
        .transaction(false, 2, move |tx| {
            let (id, encrypted) = (id.clone(), encrypted.clone());
            Box::pin(async move {
                let state: Option<String> =
                    tx.opt("SELECT state FROM ai_codex_device_flows WHERE id = $1 FOR UPDATE", params![&id]).await?.map(|r| r.get("state"));
                if !state.as_deref().is_some_and(|s| s == "polling" || s == "authorized") {
                    return Ok(false);
                }
                tx.exec(
                    "INSERT INTO ai_codex_credentials (singleton, encrypted_payload, updated_at) VALUES (TRUE, $1, NOW()) \
                     ON CONFLICT (singleton) DO UPDATE SET encrypted_payload = $1, updated_at = NOW()",
                    params![&encrypted],
                )
                .await?;
                tx.exec(
                    &format!("UPDATE ai_codex_device_flows SET state = 'completed', {CLEAR_SECRETS}, completed_at = NOW(), updated_at = NOW() WHERE id = $1"),
                    params![&id],
                )
                .await?;
                Ok(true)
            })
        })
        .await?;
    Ok(json!({ "status": if completed { "connected" } else { "cancelled" } }))
}

/// `pollDeviceFlow`.
pub async fn poll_device_flow(db: &Db, flow_id: &str, user: &str, session_id: &str) -> CResult<Value> {
    let session = hash_session(session_id)?;
    let time = now_ms();
    let (id, u, s) = (flow_id.to_owned(), user.to_owned(), session);
    let claim = db
        .transaction(false, 2, move |tx| {
            let (id, user, session) = (id.clone(), u.clone(), s.clone());
            Box::pin(async move {
                let Some(row) = tx
                    .opt("SELECT * FROM ai_codex_device_flows WHERE id = $1 AND admin_user_id = $2 AND session_hash = $3 FOR UPDATE", params![&id, &user, &session])
                    .await?
                else {
                    return Ok(Claim::NotFound);
                };
                let flow = flow_of(&row);
                if ACTIVE.contains(&flow.state.as_str()) && flow.expires_at <= time {
                    tx.exec(&format!("UPDATE ai_codex_device_flows SET state = 'expired', {CLEAR_SECRETS}, updated_at = $2 WHERE id = $1"), params![&id, at(time)]).await?;
                    return Ok(Claim::Terminal("expired".into(), None));
                }
                if ["completed", "cancelled", "expired", "failed"].contains(&flow.state.as_str()) {
                    return Ok(Claim::Terminal(flow.state.clone(), flow.failure_code.clone()));
                }
                if flow.state == "pending" && flow.next_poll_at > time {
                    return Ok(Claim::Waiting(flow.next_poll_at - time));
                }
                if flow.state == "polling" && flow.updated_at > time - POLL_CLAIM_STALE_MS {
                    return Ok(Claim::Waiting(MIN_INTERVAL_MS));
                }
                tx.exec("UPDATE ai_codex_device_flows SET state = 'polling', updated_at = $2 WHERE id = $1", params![&id, at(time)]).await?;
                Ok(Claim::Claimed(Flow { state: "polling".into(), updated_at: time, ..flow }))
            })
        })
        .await?;
    let mut flow = match claim {
        Claim::NotFound => return Err(CodexError::new("Device authorization not found", 404)),
        Claim::Terminal(state, failure) => return Ok(terminal(&state, failure.as_deref())),
        Claim::Waiting(ms) => return Ok(json!({ "status": "pending", "retryAfterMs": ms.max(MIN_INTERVAL_MS) })),
        Claim::Claimed(f) => f,
    };
    if flow.authorization_code_enc.is_none() || flow.code_verifier_enc.is_none() {
        let unavailable = CodexError::new("Stored ChatGPT device authorization is unavailable", 400);
        let device = flow.device_auth_id_enc.as_deref().and_then(crypto::decrypt);
        let code = flow.user_code_enc.as_deref().and_then(crypto::decrypt);
        let (Some(device), Some(code)) = (device, code) else {
            release_failed(db, &flow.id, &unavailable, "failed").await;
            return Err(unavailable);
        };
        let (status, text) =
            match auth_request(DEVICE_TOKEN_URL, Some(json!({ "device_auth_id": device, "user_code": code })), None)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    release_failed(db, &flow.id, &e, "pending").await;
                    return Err(e);
                }
            };
        let body = parse_json(&text);
        if !(200..300).contains(&status) {
            let code = error_code(body.as_ref());
            if status == 403 || status == 404 || code == "deviceauth_authorization_pending" {
                release(db, &flow.id, "pending", None, Some(now_ms() + flow.interval_ms), None, false).await;
                return Ok(json!({ "status": "pending", "retryAfterMs": flow.interval_ms }));
            }
            if code == "slow_down" {
                let interval = (flow.interval_ms + SLOW_DOWN_MS).min(MAX_INTERVAL_MS);
                release(db, &flow.id, "pending", Some(interval), Some(now_ms() + interval), None, false).await;
                return Ok(json!({ "status": "pending", "retryAfterMs": interval }));
            }
            let e = CodexError::new(format!("ChatGPT device authorization failed ({status})"), 502)
                .code(Some(sanitize_text(&code, 100)))
                .transient(status >= 500);
            release_failed(db, &flow.id, &e, if e.transient { "pending" } else { "failed" }).await;
            return Err(e);
        }
        let body = body.unwrap_or(Value::Null);
        let (Some(auth_code), Some(verifier)) = (
            body.get("authorization_code").and_then(Value::as_str).filter(|s| !s.is_empty()),
            body.get("code_verifier").and_then(Value::as_str).filter(|s| !s.is_empty()),
        ) else {
            let e = CodexError::new("Invalid ChatGPT device authorization response", 502);
            release_failed(db, &flow.id, &e, "failed").await;
            return Err(e);
        };
        let (code_enc, verifier_enc) = (encrypt(auth_code)?, encrypt(verifier)?);
        let authorized = db
            .exec(
                "UPDATE ai_codex_device_flows SET state = 'authorized', authorization_code_enc = $2, code_verifier_enc = $3, updated_at = NOW() WHERE id = $1 AND state = 'polling'",
                params![&flow.id, &code_enc, &verifier_enc],
            )
            .await?;
        if authorized == 0 {
            return Ok(json!({ "status": "cancelled" }));
        }
        flow.authorization_code_enc = Some(code_enc);
        flow.code_verifier_enc = Some(verifier_enc);
        flow.state = "authorized".into();
    }
    match exchange(db, &flow).await {
        Ok(v) => Ok(v),
        Err(e) => {
            release_failed(db, &flow.id, &e, if e.transient { "authorized" } else { "failed" }).await;
            Err(e)
        }
    }
}

/// `cancelDeviceFlow`.
pub async fn cancel_device_flow(db: &Db, flow_id: &str, user: &str, session_id: &str) -> CResult<Value> {
    let session = hash_session(session_id)?;
    let cancelled = db
        .opt(
            &format!(
                "UPDATE ai_codex_device_flows SET state = 'cancelled', {CLEAR_SECRETS}, updated_at = NOW() \
                 WHERE id = $1 AND admin_user_id = $2 AND session_hash = $3 AND state IN ('pending', 'polling', 'authorized') RETURNING id"
            ),
            params![flow_id, user, &session],
        )
        .await?;
    if cancelled.is_none() {
        return Err(CodexError::new("Device authorization not found", 404));
    }
    Ok(json!({ "status": "cancelled" }))
}

async fn credential(db: &Db) -> CResult<Option<String>> {
    Ok(db
        .opt("SELECT encrypted_payload FROM ai_codex_credentials WHERE singleton = TRUE", params![])
        .await?
        .and_then(|r| r.get::<_, Option<String>>("encrypted_payload"))
        .filter(|p| !p.is_empty()))
}

/// `getStatus`: the credential state, or the caller's pending device flow.
pub async fn status(db: &Db, owner: Option<(&str, &str)>) -> CResult<Value> {
    let mut credential_status: Option<Value> = None;
    if let Some(enc) = credential(db).await? {
        match decrypt_json(&enc) {
            Ok(c) if c.get("state").and_then(Value::as_str) == Some("connected") => {
                return Ok(json!({
                    "connected": true, "state": "connected", "expiresAt": c.get("expiresAt"),
                    "accountLabel": mask_label(c.get("accountLabel").and_then(Value::as_str).unwrap_or("")),
                }));
            }
            Ok(c) => {
                credential_status = Some(json!({
                    "connected": false,
                    "state": c.get("state").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("reconnect_required"),
                    "reconnectRequired": true,
                    "reason": c.get("failureCode").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("authorization_expired"),
                }));
            }
            Err(_) => {
                credential_status = Some(
                    json!({ "connected": false, "state": "reconnect_required", "reconnectRequired": true, "reason": "credential_unavailable" }),
                );
            }
        }
    }
    if let Some((user, session_id)) = owner.filter(|(u, s)| !u.is_empty() && !s.is_empty()) {
        let session = hash_session(session_id)?;
        let flow = db
            .opt("SELECT * FROM ai_codex_device_flows WHERE admin_user_id = $1 AND session_hash = $2 ORDER BY created_at DESC LIMIT 1", params![user, &session])
            .await?
            .map(|r| flow_of(&r));
        if let Some(flow) = flow.filter(|f| ACTIVE.contains(&f.state.as_str()) && f.expires_at > now_ms()) {
            let Some(code) = flow.user_code_enc.as_deref().and_then(crypto::decrypt) else {
                return Ok(
                    json!({ "connected": false, "state": "failed", "reconnectRequired": true, "reason": "device_code_unavailable" }),
                );
            };
            return Ok(json!({ "connected": false, "state": "pending", "device": {
                "flowId": flow.id, "userCode": code, "verificationUrl": DEVICE_URL, "expiresAt": flow.expires_at, "intervalMs": flow.interval_ms,
            } }));
        }
    }
    Ok(credential_status
        .unwrap_or_else(|| json!({ "connected": false, "state": "disconnected", "reconnectRequired": false })))
}

/// `disconnectCodex`.
pub async fn disconnect(db: &Db) -> CResult<Value> {
    db.transaction(false, 2, |tx| {
        Box::pin(async move {
            tx.exec(&format!("UPDATE ai_codex_device_flows SET state = 'cancelled', {CLEAR_SECRETS}, updated_at = NOW() WHERE state IN ('pending', 'polling', 'authorized')"), params![])
                .await?;
            tx.exec("DELETE FROM ai_codex_credentials WHERE singleton = TRUE", params![]).await?;
            Ok(())
        })
    })
    .await?;
    Ok(json!({ "status": "disconnected" }))
}

fn access_of(c: &Value) -> CResult<(String, String)> {
    let token = c.get("accessToken").and_then(Value::as_str).filter(|s| !s.is_empty());
    let account = c.get("accountId").and_then(Value::as_str).filter(|s| !s.is_empty());
    match (c.get("state").and_then(Value::as_str), token, account) {
        (Some("connected"), Some(t), Some(a)) => Ok((t.to_owned(), a.to_owned())),
        _ => Err(CodexError::new("ChatGPT authorization requires reconnection", 401)),
    }
}

fn reconnect(code: Option<&str>) -> CodexError {
    CodexError::new("ChatGPT authorization requires reconnection", 401).code(code.map(str::to_owned))
}

/// One refresh at a time per process (`refreshInFlight`); the row lock covers other processes.
static REFRESH: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

async fn refresh_under_lock(db: &Db, force: bool) -> CResult<(String, String)> {
    let mut tx_client = db.client().await?;
    let tx = tx_client.begin(false).await?;
    let enc: Option<String> = tx
        .opt("SELECT encrypted_payload FROM ai_codex_credentials WHERE singleton = TRUE FOR UPDATE", params![])
        .await?
        .and_then(|r| r.get::<_, Option<String>>("encrypted_payload"))
        .filter(|p| !p.is_empty());
    let Some(enc) = enc else { return Err(CodexError::new("ChatGPT is not connected", 503)) };
    let c = decrypt_json(&enc)?;
    let save = |v: Value| async move {
        let encrypted = encrypt(&v.to_string())?;
        tx.exec(
            "INSERT INTO ai_codex_credentials (singleton, encrypted_payload, updated_at) VALUES (TRUE, $1, NOW()) \
             ON CONFLICT (singleton) DO UPDATE SET encrypted_payload = $1, updated_at = NOW()",
            params![&encrypted],
        )
        .await?;
        tx.commit().await?;
        Ok::<_, CodexError>(())
    };
    if c.get("state").and_then(Value::as_str) != Some("connected") {
        return Err(reconnect(c.get("failureCode").and_then(Value::as_str)));
    }
    let expires = c.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    if !force && expires > now_ms() + REFRESH_SKEW_MS {
        return access_of(&c);
    }
    let with = |state: &str, code: &str| {
        let mut m = c.as_object().cloned().unwrap_or_default();
        m.insert("state".into(), json!(state));
        m.insert("accessToken".into(), Value::Null);
        m.insert("refreshToken".into(), Value::Null);
        m.insert("failureCode".into(), json!(code));
        Value::Object(m)
    };
    let Some(refresh) = c.get("refreshToken").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
    else {
        save(with("reconnect_required", "missing_refresh_token")).await?;
        return Err(reconnect(Some("missing_refresh_token")));
    };
    let (status, text) = auth_request(
        TOKEN_URL,
        None,
        Some(vec![
            ("grant_type", "refresh_token".into()),
            ("client_id", CLIENT_ID.into()),
            ("refresh_token", refresh.clone()),
        ]),
    )
    .await?;
    let body = parse_json(&text);
    if !(200..300).contains(&status) {
        let code = sanitize_text(&error_code(body.as_ref()), 100);
        if TERMINAL_REFRESH_ERRORS.contains(&code.as_str()) {
            save(with("reconnect_required", &code)).await?;
            return Err(CodexError::new("ChatGPT authorization expired; reconnect required", 401).code(Some(code)));
        }
        return Err(CodexError::new(format!("ChatGPT token refresh failed ({status})"), 502)
            .code(Some(code))
            .transient(true));
    }
    let body = body.unwrap_or(Value::Null);
    let Some(access) = body.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return Err(CodexError::new("Invalid ChatGPT token refresh response", 502).transient(true));
    };
    let next_refresh =
        body.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()).map_or(refresh, str::to_owned);
    let claims = jwt_claims(access);
    let (account, email) = chatgpt_account(claims.as_ref())
        .map_err(|_| CodexError::new("Invalid ChatGPT token refresh identity", 502).transient(true))?;
    let expires_at = expiry(&body, claims.as_ref(), now_ms())
        .map_err(|_| CodexError::new("Invalid ChatGPT token refresh expiry", 502).transient(true))?;
    let label_source = if !email.is_empty() {
        email
    } else {
        c.get("accountLabel")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or_else(|| account.clone(), str::to_owned)
    };
    let refreshed = json!({
        "state": "connected", "accessToken": access, "refreshToken": next_refresh, "expiresAt": expires_at,
        "accountId": account, "accountLabel": mask_label(&label_source), "failureCode": null,
    });
    save(refreshed.clone()).await?;
    access_of(&refreshed)
}

/// `getCodexAccess`: `(accessToken, accountId)`, refreshing near expiry or on demand.
pub async fn access(db: &Db, force: bool) -> CResult<(String, String)> {
    let enc = credential(db).await?.ok_or_else(|| CodexError::new("ChatGPT is not connected", 503))?;
    let c = decrypt_json(&enc)?;
    if c.get("state").and_then(Value::as_str) != Some("connected") {
        return Err(reconnect(c.get("failureCode").and_then(Value::as_str)));
    }
    let expires = c.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    if !force && expires > now_ms() + REFRESH_SKEW_MS {
        return access_of(&c);
    }
    let _one = REFRESH.lock().await;
    refresh_under_lock(db, force).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_labels_and_intervals() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(json!({ "https://api.openai.com/auth": { "chatgpt_account_id": "acc-1" }, "email": "jane@x.co", "exp": 10 }).to_string());
        let claims = jwt_claims(&format!("h.{payload}.s"));
        assert_eq!(chatgpt_account(claims.as_ref()).unwrap(), ("acc-1".into(), "jane@x.co".into()));
        assert!(jwt_claims("only.two").is_none());
        assert_eq!(mask_label("jane@x.co"), "j***@x.co");
        assert_eq!(mask_label("abcdef"), "ab••••ef");
        assert_eq!(mask_label("abc"), "••••");
        assert_eq!(interval_ms(None), Some(5000));
        assert_eq!(interval_ms(Some(&json!("0.2"))), Some(1000));
        assert_eq!(interval_ms(Some(&json!(500))), Some(60_000));
        assert_eq!(interval_ms(Some(&json!(-1))), None);
        assert_eq!(expiry(&json!({}), claims.as_ref(), 0).unwrap(), 10_000);
    }
}
