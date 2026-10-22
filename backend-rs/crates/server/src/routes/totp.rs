//! `/api/totp`: authenticator-app enrolment for a signed-in user (port of `routes/totp.ts`).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use axum::routing::{get, post};
use inboxora_core::pg::Q;
use inboxora_core::{Error, Result, crypto, params};
use parking_lot::Mutex;
use serde_json::json;

use super::util::{err, ok};
use crate::http::{Auth, Body};
use crate::services::auth::bcrypt_verify;
use crate::services::dav_xml::js_string;
use crate::services::totp;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/setup", get(setup))
        .route("/enable", post(enable))
        .route("/cancel", post(cancel))
        .route("/disable", post(disable))
}

/// The in-memory per-user attempt budget (`totpLimiter`: 5 per 15 minutes).
static BUCKETS: LazyLock<Mutex<HashMap<String, (u32, Instant)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const WINDOW: Duration = Duration::from_secs(15 * 60);

fn limit(user_id: &str) -> Option<Response> {
    let now = Instant::now();
    let mut m = BUCKETS.lock();
    if m.len() > 10_000 {
        m.retain(|_, (_, reset)| now <= *reset);
    }
    match m.get_mut(user_id) {
        Some((count, reset)) if now <= *reset => {
            if *count >= 5 {
                let mut res = err(429, "Too many attempts. Try again later.");
                let secs = (*reset - now).as_millis().div_ceil(1000) as u64;
                res.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
                return Some(res);
            }
            *count += 1;
            None
        }
        _ => {
            m.insert(user_id.to_owned(), (1, now + WINDOW));
            None
        }
    }
}

fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

async fn setup(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let row = s.db.opt("SELECT username, totp_enabled FROM users WHERE id = $1", params![&auth.user_id]).await?;
    if row.as_ref().and_then(|r| r.get::<_, Option<bool>>("totp_enabled")).unwrap_or(false) {
        return Ok(err(409, "Two-factor authentication is already enabled. Disable it before setting it up again."));
    }
    let username = row
        .and_then(|r| r.get::<_, Option<String>>("username"))
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "user".into());
    let secret = totp::generate_secret();
    let qr = totp::qr_data_url(&totp::keyuri(&username, "Inboxora", &secret)).map_err(Error::internal)?;
    auth.session.set("pendingTOTPSecret", secret.clone());
    auth.session.set("pendingTOTPExpiry", now_ms() + 10.0 * 60_000.0);
    Ok(ok(json!({ "secret": secret, "qrCode": qr })))
}

fn clear_pending(auth: &Auth) {
    auth.session.remove("pendingTOTPSecret");
    auth.session.remove("pendingTOTPExpiry");
}

async fn enable(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    if let Some(r) = limit(&auth.user_id) {
        return Ok(r);
    }
    let Some(code) = body.get("code").filter(|c| inboxora_core::json::truthy(Some(c))) else {
        return Ok(err(400, "Code required"));
    };
    let Some(secret) = auth.session.get_str("pendingTOTPSecret").filter(|s| !s.is_empty()) else {
        return Ok(err(400, "No pending setup found. Start over."));
    };
    let expiry = auth.session.get("pendingTOTPExpiry").and_then(|v| v.as_f64()).filter(|e| *e != 0.0);
    if expiry.is_none_or(|e| now_ms() > e) {
        clear_pending(&auth);
        return Ok(err(400, "Setup session expired. Start over."));
    }
    let normalized: String = js_string(code).chars().filter(|c| !c.is_whitespace()).collect();
    if !totp::verify(&normalized, &secret) {
        return Ok(err(400, "Invalid code — check your device clock and try again."));
    }
    let updated = s
        .db
        .opt(
            "UPDATE users SET totp_secret = $1, totp_enabled = true WHERE id = $2 AND COALESCE(totp_enabled, false) = false RETURNING id",
            params![crypto::encrypt(&secret).map_err(Error::internal)?, &auth.user_id],
        )
        .await?;
    clear_pending(&auth);
    if updated.is_none() {
        return Ok(err(409, "Two-factor authentication was enabled in another session. Refresh and try again."));
    }
    Ok(ok(json!({ "ok": true })))
}

async fn cancel(auth: Auth) -> Response {
    clear_pending(&auth);
    ok(json!({ "ok": true }))
}

async fn disable(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    if let Some(r) = limit(&auth.user_id) {
        return Ok(r);
    }
    let Some(password) = body.get("password").filter(|p| inboxora_core::json::truthy(Some(p))) else {
        return Ok(err(400, "Password required"));
    };
    let Some(row) = s.db.opt("SELECT password_hash FROM users WHERE id = $1", params![&auth.user_id]).await? else {
        return Ok(err(401, "User not found"));
    };
    let Some(hash) = row.get::<_, Option<String>>("password_hash").filter(|h| !h.is_empty()) else {
        return Ok(err(
            400,
            "Your account uses SSO login and has no password. Contact an administrator to disable 2FA.",
        ));
    };
    if !bcrypt_verify(js_string(password), hash).await {
        return Ok(err(401, "Incorrect password"));
    }
    s.db.exec("UPDATE users SET totp_secret = NULL, totp_enabled = false WHERE id = $1", params![&auth.user_id])
        .await?;
    Ok(ok(json!({ "ok": true })))
}
