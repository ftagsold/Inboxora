//! Native push device registry and configuration (ports of pushDevices.ts, pushConfig.ts and
//! the status parts of pushTransports.ts / pushNotifications.ts). Delivery lives with the
//! mail engine's notification dispatcher.

use std::sync::LazyLock;

use inboxora_core::config::env;
use inboxora_core::pg::{Db, Q};
use inboxora_core::{Error, Result, crypto, params};
use regex::Regex;
use serde_json::{Map, Value};

const BCRYPT_ROUNDS: u32 = 12;
const MAX_DEVICE_ID: usize = 128;
const MAX_ENDPOINT: usize = 4096;
const MAX_APP_VERSION: usize = 64;

fn trim_slashes(v: Option<String>) -> String {
    v.map(|s| s.trim().trim_end_matches('/').to_owned()).unwrap_or_default()
}

pub fn push_base_url() -> Option<String> {
    let explicit = trim_slashes(env("PUSH_BASE_URL"));
    if !explicit.is_empty() {
        return Some(explicit);
    }
    Some(trim_slashes(env("APP_URL"))).filter(|s| !s.is_empty())
}

pub fn allow_private_endpoints() -> bool {
    env("PUSH_ALLOW_PRIVATE_ENDPOINTS").as_deref() == Some("true")
}

/// VAPID keys present (`pushConfigured`).
pub fn web_push_configured() -> bool {
    env("VAPID_PUBLIC_KEY").is_some() && env("VAPID_PRIVATE_KEY").is_some()
}

/// An FCM service account (raw JSON or base64 of it).
#[derive(Debug, Clone)]
pub struct ServiceAccount {
    pub project_id: String,
    pub client_email: String,
    pub private_key: String,
}

pub fn parse_service_account(raw: &str) -> Option<ServiceAccount> {
    let mut text = raw.trim().to_owned();
    if !text.starts_with('{') {
        text = String::from_utf8(inboxora_mail::headers::base64_lenient(&text)).ok()?;
    }
    let v: Value = serde_json::from_str(&text).ok()?;
    let s = |k: &str| v.get(k).filter(|x| inboxora_core::json::truthy(Some(x))).map(inboxora_mail::text::js_string);
    Some(ServiceAccount {
        project_id: s("project_id")?,
        client_email: s("client_email")?,
        private_key: s("private_key")?.replace("\\n", "\n"),
    })
}

pub fn fcm_configured() -> bool {
    env("FCM_SERVICE_ACCOUNT_JSON").and_then(|r| parse_service_account(&r)).is_some()
}

/// `trimmed(value, max)`: string values trimmed and cut to `max` UTF-16 units.
fn trimmed(v: Option<&Value>, max: usize) -> String {
    v.and_then(Value::as_str)
        .map(|s| inboxora_mail::text::js_substring(inboxora_mail::text::js_trim(s), max))
        .unwrap_or_default()
}

pub struct Registration {
    pub device_id: String,
    pub platform: String,
    pub transport: String,
    pub endpoint: String,
    pub app_version: Option<String>,
}

/// `validateDeviceRegistration`: `Err` carries the 400 message.
pub fn validate_registration(input: &Value) -> std::result::Result<Registration, &'static str> {
    let device_id = trimmed(input.get("deviceId"), MAX_DEVICE_ID);
    if device_id.is_empty() {
        return Err("deviceId is required");
    }
    let platform = trimmed(input.get("platform"), 16).to_lowercase();
    if !matches!(platform.as_str(), "android" | "ios" | "web" | "desktop") {
        return Err("Unsupported platform");
    }
    let transport = trimmed(input.get("transport"), 32).to_lowercase();
    if !matches!(transport.as_str(), "unifiedpush" | "fcm") {
        return Err("Unsupported push transport");
    }
    let endpoint = trimmed(input.get("endpoint"), MAX_ENDPOINT);
    if endpoint.is_empty() {
        return Err("endpoint is required");
    }
    if transport == "unifiedpush" {
        let url = url::Url::parse(&endpoint).map_err(|_| "UnifiedPush endpoint must be a valid URL")?;
        let allow_http = allow_private_endpoints() && url.scheme() == "http";
        if url.scheme() != "https" && !allow_http {
            return Err("UnifiedPush endpoint must use HTTPS");
        }
    }
    let app_version = Some(trimmed(input.get("appVersion"), MAX_APP_VERSION)).filter(|s| !s.is_empty());
    Ok(Registration { device_id, platform, transport, endpoint, app_version })
}

pub async fn register(db: &Db, user_id: &str, reg: Registration) -> Result<(Map<String, Value>, String)> {
    let prefix = format!("mf_push_{}", crypto::random_uuid());
    let secret = crypto::random_base64url(32);
    let token = format!("{prefix}.{secret}");
    let hash = tokio::task::spawn_blocking(move || bcrypt::hash(secret, BCRYPT_ROUNDS))
        .await
        .map_err(|e| Error::internal(e.to_string()))?
        .map_err(|e| Error::internal(e.to_string()))?;
    let endpoint = crypto::encrypt(&reg.endpoint).map_err(Error::internal)?;
    let row = db
        .json_opt(
            "INSERT INTO push_devices (user_id, device_id, platform, transport, endpoint, token_prefix, token_hash, app_version, updated_at, last_seen) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()) ON CONFLICT (user_id, device_id) DO UPDATE SET platform = EXCLUDED.platform, \
             transport = EXCLUDED.transport, endpoint = EXCLUDED.endpoint, token_prefix = EXCLUDED.token_prefix, token_hash = EXCLUDED.token_hash, \
             app_version = EXCLUDED.app_version, failure_count = 0, disabled_at = NULL, updated_at = NOW(), last_seen = NOW() \
             RETURNING id, device_id, platform, transport, app_version, created_at, updated_at, last_seen",
            params![user_id, &reg.device_id, &reg.platform, &reg.transport, &endpoint, &prefix, &hash, reg.app_version],
        )
        .await?
        .ok_or_else(|| Error::internal("Device registration did not return a row"))?;
    Ok((row, token))
}

/// An authenticated device (`req.pushDevice`).
#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub user_id: String,
    pub device_id: String,
    pub transport: String,
}

fn parse_token(v: &str) -> Option<(String, String)> {
    static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(mf_push_[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.([A-Za-z0-9_-]{16,})$")
            .expect("regex")
    });
    let c = TOKEN.captures(v)?;
    Some((c[1].to_owned(), c[2].to_owned()))
}

/// `bearerTokenFromHeader`.
pub fn bearer(header: Option<&str>) -> Option<String> {
    let h = header?;
    let rest = h.get(..7).filter(|p| p.eq_ignore_ascii_case("bearer ")).map(|_| &h[7..]).or_else(|| {
        let (scheme, rest) = h.split_once(char::is_whitespace)?;
        scheme.eq_ignore_ascii_case("bearer").then_some(rest)
    })?;
    let t = rest.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

pub async fn authenticate(db: &Db, token: &str) -> Result<Option<Device>> {
    let Some((prefix, secret)) = parse_token(token) else { return Ok(None) };
    let Some(row) = db
        .json_opt("SELECT id, user_id, device_id, transport, token_hash FROM push_devices WHERE token_prefix = $1 AND disabled_at IS NULL", params![&prefix])
        .await?
    else {
        return Ok(None);
    };
    let hash = row.get("token_hash").and_then(Value::as_str).unwrap_or("").to_owned();
    let ok = tokio::task::spawn_blocking(move || bcrypt::verify(secret, &hash).unwrap_or(false)).await.unwrap_or(false);
    if !ok {
        return Ok(None);
    }
    let s = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let _ = db.exec("UPDATE push_devices SET last_seen = NOW() WHERE id = $1", params![s("id")]).await;
    Ok(Some(Device { id: s("id"), user_id: s("user_id"), device_id: s("device_id"), transport: s("transport") }))
}

pub async fn prune_stale(db: &Db) -> Result<u64> {
    db.exec(
        "DELETE FROM push_devices WHERE (disabled_at IS NOT NULL AND disabled_at < NOW() - INTERVAL '30 days') OR (last_seen < NOW() - INTERVAL '180 days')",
        params![],
    )
    .await
}
