//! New-mail notifications: the canonical event (`mailNotificationEvent.ts`), its fan-out to
//! Web Push and native devices (`pushDispatcher.ts`), Web Push delivery with RFC 8291
//! encryption and VAPID (`pushNotifications.ts`) and the UnifiedPush/FCM transports
//! (`pushTransports.ts`).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hkdf::Hkdf;
use inboxora_core::config::env;
use inboxora_core::pg::{Db, Q};
use inboxora_core::{crypto, params};
use inboxora_net::reqwest;
use p256::ecdsa::signature::Signer;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2_10::Sha256;

pub const MAIL_CHANGED_TYPE: &str = "mail.changed";

#[derive(Debug, Clone)]
pub struct MailEvent {
    pub user_id: String,
    pub event_id: Option<String>,
    pub alert_count: usize,
    pub account_id: Option<String>,
    pub folder: String,
    pub web_push: Value,
    pub native: Option<Value>,
}

/// `buildMailNotificationEvent`.
pub fn mail_notification_event(user_id: &str, message: &Value, alert_count: usize, unread: Option<i64>) -> MailEvent {
    let s = |k: &str| message.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    let event_id = s("id").map(str::to_owned);
    let count = alert_count.max(1);
    let title = s("fromName").or(s("fromEmail")).unwrap_or("New mail");
    let body =
        if count == 1 { s("subject").unwrap_or("(no subject)").to_owned() } else { format!("{count} new messages") };
    let mut web = json!({
        "title": title,
        "body": body,
        "icon": "/inboxora-envelope-512.png",
        "url": event_id.as_ref().map_or_else(|| "/".to_owned(), |id| format!("/?m={id}")),
    });
    if let Some(u) = unread {
        web["unreadCount"] = json!(u);
    }
    MailEvent {
        user_id: user_id.to_owned(),
        alert_count: count,
        account_id: s("account_id").or(s("accountId")).map(str::to_owned),
        folder: s("folder").unwrap_or("INBOX").to_owned(),
        web_push: web,
        native: event_id.as_ref().map(|id| json!({ "type": MAIL_CHANGED_TYPE, "eventId": id })),
        event_id,
    }
}

static RECENT: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const DEDUP_TTL: Duration = Duration::from_secs(60);

fn already_dispatched(user_id: &str, event_id: &str) -> bool {
    let key = format!("{user_id}:{event_id}");
    let now = Instant::now();
    let mut m = RECENT.lock();
    let previous = m.insert(key, now);
    if m.len() > 2000 {
        m.retain(|_, t| now.duration_since(*t) <= DEDUP_TTL);
    }
    previous.is_some_and(|p| now.duration_since(p) < DEDUP_TTL)
}

/// `dispatchMailNotification`: one event to Web Push and every native device, deduplicated.
pub async fn dispatch_mail_notification(db: &Db, event: MailEvent) -> Result<Value, String> {
    let Some(event_id) = event.event_id.clone().filter(|_| !event.user_id.is_empty()) else {
        return Ok(json!({ "dispatched": false }));
    };
    if already_dispatched(&event.user_id, &event_id) {
        return Ok(json!({ "dispatched": false, "skipped": "duplicate" }));
    }
    let web = async {
        if !crate::services::push::web_push_configured() {
            return "disabled";
        }
        match send_push_to_user(db, &event.user_id, &event.web_push).await {
            Ok(()) => "delivered",
            Err(e) => {
                tracing::warn!("Web Push dispatch failed: {e}");
                "error"
            }
        }
    };
    let native = dispatch_native(db, &event);
    let (web, native) = tokio::join!(web, native);
    Ok(json!({ "dispatched": true, "webPush": web, "native": native }))
}

async fn dispatch_native(db: &Db, event: &MailEvent) -> Value {
    let mut summary = json!({ "delivered": 0, "invalid": 0, "retry": 0, "disabled": 0, "skipped": null });
    let Some(native) = &event.native else {
        summary["skipped"] = json!("no-event-id");
        return summary;
    };
    let devices = match db
        .rows("SELECT id, device_id, platform, transport, endpoint, failure_count FROM push_devices WHERE user_id = $1 AND disabled_at IS NULL", params![
            &event.user_id
        ])
        .await
    {
        Ok(r) => r,
        Err(e) => {
            summary["skipped"] = json!("lookup-failed");
            tracing::warn!("Native push device lookup failed: {e}");
            return summary;
        }
    };
    let results = futures_util::future::join_all(devices.iter().filter_map(|d| {
        let endpoint = crypto::decrypt(&d.get::<_, String>("endpoint"))?;
        let id: uuid::Uuid = d.get("id");
        let transport: String = d.get("transport");
        Some(async move { (id, send_native(&transport, &endpoint, native).await) })
    }))
    .await;
    for (id, verdict) in results {
        let key = match verdict {
            Verdict::Invalid => {
                let _ = db
                    .exec(
                        "UPDATE push_devices SET disabled_at = NOW(), endpoint = '', token_hash = NULL, token_prefix = NULL, updated_at = NOW() WHERE id = $1",
                        params![id],
                    )
                    .await;
                "invalid"
            }
            Verdict::Retry => {
                let _ = db
                    .exec(
                        "UPDATE push_devices SET failure_count = failure_count + 1, updated_at = NOW() WHERE id = $1",
                        params![id],
                    )
                    .await;
                "retry"
            }
            Verdict::Delivered => "delivered",
            Verdict::Disabled => "disabled",
        };
        summary[key] = json!(summary[key].as_i64().unwrap_or(0) + 1);
    }
    summary
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Invalid,
    Retry,
    Delivered,
    Disabled,
}

pub async fn send_native(transport: &str, endpoint: &str, event: &Value) -> Verdict {
    match transport {
        "unifiedpush" => send_unified_push(endpoint, event).await,
        "fcm" => send_fcm(endpoint, event).await,
        _ => Verdict::Disabled,
    }
}

async fn send_unified_push(endpoint: &str, event: &Value) -> Verdict {
    let allow = crate::services::push::allow_private_endpoints();
    let guard = inboxora_net::Guard { allow_private: allow, require_https: Some(!allow) };
    let req = match inboxora_net::safe_request(reqwest::Method::POST, endpoint, guard) {
        Ok(r) => r,
        Err(_) => return Verdict::Invalid,
    };
    match req
        .header("Content-Type", "application/json")
        .body(event.to_string())
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("private") || msg.contains("insecure") || msg.contains("scheme") {
                Verdict::Invalid
            } else {
                Verdict::Retry
            }
        }
        Ok(r) => match r.status().as_u16() {
            404 | 410 => Verdict::Invalid,
            429 => Verdict::Retry,
            s if s >= 500 => Verdict::Retry,
            s if (200..300).contains(&s) => Verdict::Delivered,
            _ => Verdict::Retry,
        },
    }
}

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("TLS backend available")
});
static FCM_TOKEN: LazyLock<Mutex<Option<(String, Instant)>>> = LazyLock::new(|| Mutex::new(None));
const FCM_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

async fn fcm_access_token(sa: &crate::services::push::ServiceAccount) -> Result<String, String> {
    if let Some((t, until)) = FCM_TOKEN.lock().clone()
        && Instant::now() < until
    {
        return Ok(t);
    }
    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "scope": "https://www.googleapis.com/auth/firebase.messaging",
        "iss": sa.client_email, "sub": sa.client_email, "aud": FCM_TOKEN_URL, "iat": now, "exp": now + 3600,
    });
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(sa.private_key.as_bytes()).map_err(|e| e.to_string())?;
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.typ = Some("JWT".into());
    let assertion = jsonwebtoken::encode(&header, &claims, &key).map_err(|e| e.to_string())?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
        .append_pair("assertion", &assertion)
        .finish();
    let res = HTTP
        .post(FCM_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("FCM OAuth token request failed (HTTP {})", res.status().as_u16()));
    }
    let data: Value = res.json().await.map_err(|e| e.to_string())?;
    let token = data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("FCM OAuth response did not contain an access token")?
        .to_owned();
    let secs = data.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).max(60);
    *FCM_TOKEN.lock() = Some((token.clone(), Instant::now() + Duration::from_secs(secs) - Duration::from_secs(60)));
    Ok(token)
}

async fn send_fcm(endpoint: &str, event: &Value) -> Verdict {
    let Some(sa) = env("FCM_SERVICE_ACCOUNT_JSON").and_then(|raw| crate::services::push::parse_service_account(&raw))
    else {
        return Verdict::Disabled;
    };
    let token = match fcm_access_token(&sa).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("FCM credential error: {e}");
            return Verdict::Retry;
        }
    };
    let body = json!({ "message": {
        "token": endpoint,
        "data": {
            "type": event.get("type").map(inboxora_mail::text::js_string).filter(|s| !s.is_empty()).unwrap_or_else(|| MAIL_CHANGED_TYPE.into()),
            "eventId": event.get("eventId").map(inboxora_mail::text::js_string).unwrap_or_default(),
        },
        "android": { "priority": "high", "ttl": "3600s" },
    }});
    let url = format!(
        "https://fcm.googleapis.com/v1/projects/{}/messages:send",
        inboxora_mail::sanitize::encode_uri_component(&sa.project_id)
    );
    let Ok(r) = HTTP.post(url).bearer_auth(token).json(&body).send().await else { return Verdict::Retry };
    let status = r.status().as_u16();
    if (200..300).contains(&status) {
        return Verdict::Delivered;
    }
    match status {
        404 => Verdict::Invalid,
        401 | 403 => {
            tracing::warn!("FCM rejected the server credentials (HTTP {status}); check FCM_SERVICE_ACCOUNT_JSON.");
            Verdict::Retry
        }
        429 => Verdict::Retry,
        s if s >= 500 => Verdict::Retry,
        _ => {
            let body: Value = r.json().await.unwrap_or(Value::Null);
            let code =
                body["error"]["details"][0]["errorCode"].as_str().or(body["error"]["status"].as_str()).unwrap_or("");
            if status == 400 || code == "UNREGISTERED" || code == "INVALID_ARGUMENT" {
                Verdict::Invalid
            } else {
                Verdict::Retry
            }
        }
    }
}

// ── Web Push ────────────────────────────────────────────────────────────────────────────────

fn b64(s: &str) -> Option<Vec<u8>> {
    let t = s.trim().trim_end_matches('=');
    URL_SAFE_NO_PAD.decode(t).ok().or_else(|| STANDARD.decode(s.trim()).ok())
}

/// RFC 8291 `aes128gcm` payload encryption for one subscription.
pub fn encrypt_payload(p256dh: &str, auth: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
    let ua_public_bytes = b64(p256dh).ok_or("invalid p256dh")?;
    let auth_secret = b64(auth).ok_or("invalid auth")?;
    let ua_public = p256::PublicKey::from_sec1_bytes(&ua_public_bytes).map_err(|e| e.to_string())?;
    let as_secret = p256::ecdh::EphemeralSecret::random(&mut p256::elliptic_curve::rand_core::OsRng);
    let as_public = as_secret.public_key().to_encoded_point(false);
    let shared = as_secret.diffie_hellman(&ua_public);
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(&ua_public_bytes);
    key_info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&auth_secret), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|e| e.to_string())?;
    let salt = crypto::random_bytes::<16>();
    let prk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|e| e.to_string())?;
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|e| e.to_string())?;
    let mut padded = payload.to_vec();
    padded.push(2);
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|e| e.to_string())?;
    let ct = cipher.encrypt(&Nonce::from(nonce), padded.as_slice()).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(16 + 4 + 1 + 65 + ct.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&4096u32.to_be_bytes());
    out.push(65);
    out.extend_from_slice(as_public.as_bytes());
    out.extend_from_slice(&ct);
    Ok(out)
}

/// `Authorization: vapid t=<JWT>, k=<public key>` for an endpoint's origin.
pub fn vapid_header(endpoint: &str) -> Result<String, String> {
    let public = env("VAPID_PUBLIC_KEY").ok_or("VAPID not configured")?;
    let private = env("VAPID_PRIVATE_KEY").ok_or("VAPID not configured")?;
    let subject =
        env("VAPID_SUBJECT").or_else(|| env("APP_URL")).unwrap_or_else(|| "mailto:admin@mailflow.local".into());
    let url = url::Url::parse(endpoint).map_err(|e| e.to_string())?;
    let aud = url.origin().ascii_serialization();
    let key = p256::ecdsa::SigningKey::from_slice(&b64(&private).ok_or("invalid VAPID private key")?)
        .map_err(|e| e.to_string())?;
    let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = URL_SAFE_NO_PAD
        .encode(json!({ "aud": aud, "exp": chrono::Utc::now().timestamp() + 12 * 3600, "sub": subject }).to_string());
    let input = format!("{header}.{claims}");
    let sig: p256::ecdsa::Signature = key.sign(input.as_bytes());
    Ok(format!("vapid t={input}.{}, k={}", URL_SAFE_NO_PAD.encode(sig.to_bytes()), public.trim_end_matches('=')))
}

/// `sendPushToUser`: deliver to every subscription, retrying transient failures and pruning
/// gone subscriptions.
pub async fn send_push_to_user(db: &Db, user_id: &str, payload: &Value) -> Result<(), String> {
    if !crate::services::push::web_push_configured() {
        return Ok(());
    }
    let rows = db
        .rows("SELECT id, endpoint, p256dh, auth FROM push_subscriptions WHERE user_id = $1", params![user_id])
        .await
        .map_err(|e| e.to_string())?;
    let body = payload.to_string();
    let stale: Vec<String> = futures_util::future::join_all(rows.iter().map(|r| {
        let (id, endpoint, p256dh, auth) = (
            r.get::<_, uuid::Uuid>("id").to_string(),
            r.get::<_, String>("endpoint"),
            r.get::<_, String>("p256dh"),
            r.get::<_, String>("auth"),
        );
        let body = body.clone();
        async move {
            for attempt in 0..3u32 {
                let send = async {
                    let enc = encrypt_payload(&p256dh, &auth, body.as_bytes())?;
                    let auth_header = vapid_header(&endpoint)?;
                    HTTP.post(&endpoint)
                        .header("Content-Encoding", "aes128gcm")
                        .header("Content-Type", "application/octet-stream")
                        .header("TTL", "86400")
                        .header("Urgency", "high")
                        .header("Authorization", auth_header)
                        .body(enc)
                        .send()
                        .await
                        .map_err(|e| e.to_string())
                };
                let (status, retry_after) = match send.await {
                    Ok(r) => (
                        Some(r.status().as_u16()),
                        r.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_owned),
                    ),
                    Err(_) => (None, None),
                };
                match status {
                    Some(s) if (200..300).contains(&s) => return None,
                    Some(404 | 410) => return Some(id),
                    _ => {}
                }
                let transient = status.is_none_or(|s| s == 429 || s >= 500);
                if !transient || attempt == 2 {
                    tracing::warn!(
                        "Push delivery failed ({}, attempt {})",
                        status.map_or_else(|| "network".to_owned(), |s| s.to_string()),
                        attempt + 1
                    );
                    return None;
                }
                let requested = retry_after.and_then(|v| v.parse::<u64>().ok().map(|s| s * 1000)).unwrap_or(0);
                tokio::time::sleep(Duration::from_millis((1000u64 << attempt).max(requested).min(60_000))).await;
            }
            None
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect();
    if !stale.is_empty()
        && let Err(e) = db.exec("DELETE FROM push_subscriptions WHERE id = ANY($1::uuid[])", params![stale]).await
    {
        tracing::error!("Failed to prune stale push subscriptions: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_shape() {
        let e = mail_notification_event(
            "u",
            &json!({ "id": "m1", "fromName": "Alice", "subject": "Hi", "accountId": "a" }),
            1,
            Some(3),
        );
        assert_eq!(e.web_push["title"], "Alice");
        assert_eq!(e.web_push["url"], "/?m=m1");
        assert_eq!(e.web_push["unreadCount"], 3);
        assert_eq!(e.native.unwrap()["type"], "mail.changed");
        let e = mail_notification_event("u", &json!({}), 3, None);
        assert_eq!(e.web_push["body"], "3 new messages");
        assert!(e.native.is_none());
    }

    #[test]
    fn encrypts_for_a_subscription() {
        let ua = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let ua_pub = URL_SAFE_NO_PAD.encode(ua.public_key().to_encoded_point(false).as_bytes());
        let auth = URL_SAFE_NO_PAD.encode([7u8; 16]);
        let out = encrypt_payload(&ua_pub, &auth, b"{\"x\":1}").unwrap();
        // salt(16) rs(4) idlen(1) keyid(65) ciphertext(payload+1+tag16)
        assert_eq!(out.len(), 16 + 4 + 1 + 65 + 7 + 1 + 16);
        assert_eq!(out[20], 65);
    }
}
