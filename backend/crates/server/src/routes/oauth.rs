//! `/oauth`: connect a Microsoft 365 mailbox by authorization code or device code (port of
//! `routes/oauth.ts`).

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use inboxora_core::{Result, crypto};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::dav::express_redirect;
use super::util::{err, json as json_status, ok};
use crate::http::{Qs, Sess};
use crate::services::oauth::{self as svc, SCOPES};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/microsoft", get(start))
        .route("/microsoft/callback", get(callback))
        .route("/microsoft/device", post(device_start))
        .route("/microsoft/device/poll", get(device_poll))
}

struct DeviceFlow {
    device_code: String,
    tenant_id: String,
    client_id: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// Pending device-code flows by user (in memory, like Node).
static DEVICE_FLOWS: LazyLock<Mutex<HashMap<String, DeviceFlow>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn connect(s: &AppState, user_id: &str) {
    if let Some(hook) = &s.on_user_connect {
        hook(user_id.to_owned());
    }
}

async fn start(State(s): State<AppState>, Sess(session): Sess, headers: HeaderMap, method: Method) -> Result<Response> {
    let Some(user_id) = session.user_id().filter(|u| !u.is_empty()) else { return Ok(err(401, "Not authenticated")) };
    let cfg = svc::ms_config();
    let (Some(client_id), Some(redirect)) = (cfg.client_id.clone(), cfg.redirect_uri.clone()) else {
        return Ok(err(
            500,
            "Microsoft OAuth not configured. Set MS_CLIENT_ID, MS_CLIENT_SECRET, MS_TENANT_ID, MS_REDIRECT_URI in .env",
        ));
    };
    let nonce = crypto::random_hex(16);
    session.set("oauthNonce", nonce.clone());
    session.set("oauthUserId", user_id);
    let params = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_mode", "query")
        .append_pair("scope", SCOPES)
        .append_pair("state", &nonce)
        .append_pair("prompt", "select_account")
        .finish();
    s.sessions.save(&session).await?;
    let location = format!("{}/{}/oauth2/v2.0/authorize?{params}", svc::authority(), cfg.tenant_id);
    Ok(express_redirect(&headers, &method, StatusCode::FOUND, &location))
}

async fn callback(
    State(s): State<AppState>,
    Sess(session): Sess,
    q: Qs,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let redirect = |loc: &str| express_redirect(&headers, &method, StatusCode::FOUND, loc);
    let enc = inboxora_mail::sanitize::encode_uri_component;
    let (code, state, error, description) =
        (q.first("code"), q.first("state"), q.first("error"), q.first("error_description"));
    if let Some(e) = error.filter(|e| !e.is_empty()) {
        tracing::error!("Microsoft OAuth error: {e} {}", description.clone().unwrap_or_default());
        return redirect(&format!(
            "/?oauth_error={}",
            enc(description.as_deref().filter(|d| !d.is_empty()).unwrap_or(&e))
        ));
    }
    let expected = session.get_str("oauthNonce");
    if state.as_deref().is_none_or(|st| st.is_empty() || Some(st) != expected.as_deref()) {
        return redirect(&format!("/?oauth_error={}", enc("Invalid OAuth state — please try again")));
    }
    let Some(user_id) = session.get_str("oauthUserId").filter(|u| !u.is_empty()) else {
        return redirect(&format!("/?oauth_error={}", enc("OAuth session expired — please try again")));
    };
    session.remove("oauthNonce");
    session.remove("oauthUserId");
    let cfg = svc::ms_config();
    let run = async {
        let url = format!("{}/{}/oauth2/v2.0/token", svc::authority(), cfg.tenant_id);
        let form = [
            ("client_id", cfg.client_id.as_deref()),
            ("client_secret", cfg.client_secret.as_deref()),
            ("code", code.as_deref()),
            ("redirect_uri", cfg.redirect_uri.as_deref()),
            ("grant_type", Some("authorization_code")),
        ];
        let (status, tokens) = svc::post_form(&url, &form).await?;
        if !(200..300).contains(&status) {
            let msg = tokens
                .get("error_description")
                .or_else(|| tokens.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("Token exchange failed");
            return Err(msg.to_owned());
        }
        svc::process_microsoft_tokens(&s.db, &user_id, &tokens, &cfg.tenant_id, cfg.client_id.as_deref(), false).await
    };
    match run.await {
        Ok(_) => {
            connect(&s, &user_id);
            redirect("/?oauth_success=microsoft")
        }
        Err(e) => {
            tracing::error!("Microsoft OAuth callback error: {e}");
            redirect("/?oauth_error=Authentication+failed")
        }
    }
}

async fn device_start(State(_s): State<AppState>, Sess(session): Sess) -> Response {
    let Some(user_id) = session.user_id().filter(|u| !u.is_empty()) else { return err(401, "Not authenticated") };
    let cfg = svc::ms_config();
    let Some(client_id) = cfg.client_id.clone() else {
        return err(400, "Microsoft integration not configured. Set Client ID and Tenant ID in the Integrations tab.");
    };
    let run = async {
        let url = format!("{}/{}/oauth2/v2.0/devicecode", svc::authority(), cfg.tenant_id);
        let (status, dc) = svc::post_form(&url, &[("client_id", Some(&client_id)), ("scope", Some(SCOPES))]).await?;
        if !(200..300).contains(&status) {
            let msg = dc
                .get("error_description")
                .or_else(|| dc.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("Failed to start device code flow");
            return Err(msg.to_owned());
        }
        let expires_in = dc.get("expires_in").and_then(Value::as_f64).unwrap_or(0.0);
        DEVICE_FLOWS.lock().insert(
            user_id.clone(),
            DeviceFlow {
                device_code: dc.get("device_code").map(crate::services::dav_xml::js_string).unwrap_or_default(),
                tenant_id: cfg.tenant_id.clone(),
                client_id: client_id.clone(),
                expires_at: chrono::Utc::now() + chrono::Duration::milliseconds((expires_in * 1000.0) as i64),
            },
        );
        let interval = dc.get("interval").filter(|v| inboxora_core::json::truthy(Some(v))).cloned().unwrap_or(json!(5));
        Ok::<_, String>(json!({
            "userCode": dc.get("user_code"),
            "verificationUri": dc.get("verification_uri"),
            "expiresIn": dc.get("expires_in"),
            "interval": interval,
        }))
    };
    match run.await {
        Ok(body) => ok(body),
        Err(e) => {
            tracing::error!("Device code init error: {e}");
            err(500, &e)
        }
    }
}

async fn device_poll(State(s): State<AppState>, Sess(session): Sess) -> Response {
    let Some(user_id) = session.user_id().filter(|u| !u.is_empty()) else { return err(401, "Not authenticated") };
    let flow = DEVICE_FLOWS
        .lock()
        .get(&user_id)
        .map(|f| (f.device_code.clone(), f.tenant_id.clone(), f.client_id.clone(), f.expires_at));
    let Some((device_code, tenant_id, client_id, expires_at)) = flow else {
        return json_status(400, json!({ "status": "error", "error": "No pending device code flow" }));
    };
    let drop_flow = || {
        DEVICE_FLOWS.lock().remove(&user_id);
    };
    if chrono::Utc::now() > expires_at {
        drop_flow();
        return ok(json!({ "status": "expired" }));
    }
    let run = async {
        let url = format!("{}/{tenant_id}/oauth2/v2.0/token", svc::authority());
        let form = [
            ("client_id", Some(client_id.as_str())),
            ("grant_type", Some("urn:ietf:params:oauth:grant-type:device_code")),
            ("device_code", Some(device_code.as_str())),
        ];
        let (status, tokens) = svc::post_form(&url, &form).await?;
        match tokens.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => return Ok(json!({ "status": "pending" })),
            Some("authorization_declined") => {
                drop_flow();
                return Ok(json!({ "status": "declined" }));
            }
            Some("expired_token") => {
                drop_flow();
                return Ok(json!({ "status": "expired" }));
            }
            _ => {}
        }
        if !(200..300).contains(&status) {
            drop_flow();
            let msg = tokens
                .get("error_description")
                .or_else(|| tokens.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("Token exchange failed");
            return Ok(json!({ "status": "error", "error": msg }));
        }
        drop_flow();
        svc::process_microsoft_tokens(&s.db, &user_id, &tokens, &tenant_id, Some(&client_id), true).await?;
        connect(&s, &user_id);
        Ok::<_, String>(json!({ "status": "success" }))
    };
    match run.await {
        Ok(body) => ok(body),
        Err(e) => {
            tracing::error!("Device code poll error: {e}");
            drop_flow();
            ok(json!({ "status": "error", "error": e }))
        }
    }
}
