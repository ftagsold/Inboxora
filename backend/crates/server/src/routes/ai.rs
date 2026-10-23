//! AI endpoints (port of `routes/ai.ts`): admin provider configuration and ChatGPT device
//! authorization under `/api/admin/ai`, and the authenticated status and streaming chat proxy
//! under `/api/ai`.

use axum::Router;
use axum::body::Body as HttpBody;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use axum::routing::{get, post};
use futures_util::stream;
use inboxora_core::pg::Q;
use inboxora_core::{Result, params};
use serde_json::{Value, json};

use crate::http::{Auth, Body, Sess};
use crate::routes::util::{err, json as respond, ok};
use crate::services::ai::{self, AiError, Message};
use crate::services::codex_auth;
use crate::state::AppState;

/// Mounted inside the admin-only `/api/admin` group.
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/ai", get(get_config).patch(save_config).delete(delete_config))
        .route("/ai/test", post(test))
        .route("/ai/codex/device", post(start_device).delete(cancel_device))
        .route("/ai/codex/device/poll", post(poll_device))
        .route("/ai/codex/status", get(codex_status))
        .route("/ai/codex", axum::routing::delete(disconnect))
}

/// Mounted behind `requireAuth` at `/api/ai`.
pub fn router() -> Router<AppState> {
    Router::new().route("/status", get(status)).route("/chat", post(chat))
}

/// `serviceError`: 4xx and exposed errors carry their message; other 5xx a generic one.
fn service_error(e: AiError, fallback: &str) -> Response {
    let status = if (400..600).contains(&e.status) { e.status } else { 500 };
    let expose = status < 500 || e.expose;
    if status >= 500 {
        tracing::error!("{fallback}: {}", e.message);
    }
    respond(status, json!({ "error": if expose { Value::String(e.message) } else { json!(fallback) } }))
}

fn codex_error(e: codex_auth::CodexError, fallback: &str) -> Response {
    service_error(AiError::from(e), fallback)
}

fn respond_with(r: std::result::Result<Value, AiError>, fallback: &str) -> Response {
    match r {
        Ok(v) => ok(v),
        Err(e) => service_error(e, fallback),
    }
}

async fn get_config(State(s): State<AppState>) -> Result<Response> {
    Ok(match ai::admin_config(&s.db).await {
        Ok(c) => ok(json!({ "config": c })),
        Err(e) => service_error(e, "Failed to load AI configuration"),
    })
}

async fn save_config(State(s): State<AppState>, Sess(session): Sess, b: Body) -> Result<Response> {
    Ok(match ai::save_config(&s.db, &b.0).await {
        Ok(config) => {
            tracing::info!("[admin] {} updated AI config", session.get_str("username").unwrap_or_default());
            ok(json!({ "ok": true, "config": config }))
        }
        Err(e) => service_error(e, "Failed to save AI configuration"),
    })
}

async fn delete_config(State(s): State<AppState>) -> Result<Response> {
    Ok(match ai::delete_config(&s.db).await {
        Ok(()) => ok(json!({ "ok": true })),
        Err(e) => service_error(e, "Failed to delete AI configuration"),
    })
}

async fn test(State(s): State<AppState>) -> Result<Response> {
    Ok(respond_with(ai::test_provider(&s.db).await, "AI provider test failed"))
}

async fn start_device(State(s): State<AppState>, a: Auth, Sess(session): Sess) -> Result<Response> {
    Ok(match codex_auth::start_device_flow(&s.db, &a.user_id, &session.id()).await {
        Ok(v) => ok(v),
        Err(e) => codex_error(e, "Failed to start ChatGPT authorization"),
    })
}

/// `flowInput`: the request's flow id, or the 400 response.
fn flow_id(b: &Body) -> std::result::Result<String, Response> {
    let id = b.str("flowId").map(str::trim).unwrap_or("");
    if id.is_empty() {
        return Err(err(400, "flowId is required"));
    }
    if !inboxora_mail::util::is_uuid(id) {
        return Err(err(400, "Invalid flowId"));
    }
    Ok(id.to_owned())
}

async fn poll_device(State(s): State<AppState>, a: Auth, Sess(session): Sess, b: Body) -> Result<Response> {
    let id = match flow_id(&b) {
        Ok(id) => id,
        Err(r) => return Ok(r),
    };
    Ok(match codex_auth::poll_device_flow(&s.db, &id, &a.user_id, &session.id()).await {
        Ok(v) => ok(v),
        Err(e) => codex_error(e, "Failed to poll ChatGPT authorization"),
    })
}

async fn cancel_device(State(s): State<AppState>, a: Auth, Sess(session): Sess, b: Body) -> Result<Response> {
    let id = match flow_id(&b) {
        Ok(id) => id,
        Err(r) => return Ok(r),
    };
    Ok(match codex_auth::cancel_device_flow(&s.db, &id, &a.user_id, &session.id()).await {
        Ok(v) => ok(v),
        Err(e) => codex_error(e, "Failed to cancel ChatGPT authorization"),
    })
}

async fn codex_status(State(s): State<AppState>, a: Auth, Sess(session): Sess) -> Result<Response> {
    let sid = session.id();
    Ok(match codex_auth::status(&s.db, Some((&a.user_id, &sid))).await {
        Ok(v) => ok(v),
        Err(e) => codex_error(e, "Failed to load ChatGPT status"),
    })
}

async fn disconnect(State(s): State<AppState>) -> Result<Response> {
    Ok(match codex_auth::disconnect(&s.db).await {
        Ok(v) => ok(v),
        Err(e) => codex_error(e, "Failed to disconnect ChatGPT"),
    })
}

async fn status(State(s): State<AppState>) -> Result<Response> {
    Ok(respond_with(ai::status(&s.db).await, "Failed to load AI status"))
}

const LANGUAGES: [(&str, &str); 8] = [
    ("en", "English"),
    ("ru", "Russian"),
    ("de", "German"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("it", "Italian"),
    ("zhCN", "Simplified Chinese"),
    ("pl", "Polish"),
];

/// `aiLanguageInstruction`.
pub fn language_instruction(language: Option<&str>) -> String {
    let name = language
        .and_then(|l| LANGUAGES.iter().find(|(k, _)| *k == l))
        .map_or("the user interface language", |(_, n)| n);
    format!(
        "Always respond in {name}, unless the user explicitly asks for another language. For email drafting and rewriting, preserve the original email language when it differs from {name}."
    )
}

/// `validateMessages`.
fn validate_messages(v: Option<&Value>) -> std::result::Result<Vec<Message>, &'static str> {
    let Some(list) = v.and_then(Value::as_array).filter(|l| !l.is_empty()) else {
        return Err("messages array is required");
    };
    list.iter()
        .map(|m| {
            let role = m.get("role").filter(|r| inboxora_core::json::truthy(Some(r)));
            let content = m.get("content").and_then(Value::as_str);
            let (Some(role), Some(content)) = (role, content) else {
                return Err("Each message must have role and content");
            };
            let Some(role) = role.as_str().filter(|r| ["system", "user", "assistant"].contains(r)) else {
                return Err("Invalid message role");
            };
            if inboxora_mail::text::js_len(content) > 32_000 {
                return Err("Message content exceeds maximum length");
            }
            Ok(Message { role: role.to_owned(), content: content.to_owned() })
        })
        .collect()
}

async fn chat(State(s): State<AppState>, a: Auth, b: Body) -> Result<Response> {
    let messages = match validate_messages(b.get("messages")) {
        Ok(m) => m,
        Err(e) => return Ok(err(400, e)),
    };
    let status = match ai::status(&s.db).await {
        Ok(v) => v,
        Err(e) => return Ok(service_error(e, "Failed to load AI status")),
    };
    if status["enabled"] != json!(true) {
        let e = if status["reconnectRequired"] == json!(true) {
            "AI provider requires reconnection"
        } else {
            "AI provider is disabled or unavailable"
        };
        return Ok(err(503, e));
    }
    let language =
        s.db.opt("SELECT preferences FROM users WHERE id = $1", params![&a.user_id])
            .await
            .ok()
            .flatten()
            .and_then(|r| r.get::<_, Option<Value>>("preferences"))
            .and_then(|p| p.get("language").and_then(Value::as_str).filter(|l| !l.is_empty()).map(str::to_owned))
            .unwrap_or_else(|| "en".into());
    let mut all = vec![Message { role: "system".into(), content: language_instruction(Some(&language)) }];
    all.extend(messages);
    let rx = ai::stream_chat(s.db.clone(), all);
    // Each delta becomes one OpenAI-style SSE event; the stream ends with [DONE] or one error
    // event. Dropping the body (client gone) drops the receiver, which stops the provider.
    let events = stream::unfold((rx, false), |(mut rx, finished)| async move {
        if finished {
            return None;
        }
        Some(match rx.recv().await {
            Some(Ok(delta)) => (
                Ok::<_, std::convert::Infallible>(format!(
                    "data: {}\n\n",
                    json!({ "choices": [{ "delta": { "content": delta } }] })
                )),
                (rx, false),
            ),
            Some(Err(e)) => {
                tracing::warn!("AI chat stream failed: {}", e.message);
                (Ok(format!("data: {}\n\n", json!({ "error": "AI request failed" }))), (rx, true))
            }
            None => (Ok("data: [DONE]\n\n".to_owned()), (rx, true)),
        })
    });
    let mut res = Response::new(HttpBody::from_stream(events));
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_messages_like_node() {
        assert_eq!(validate_messages(Some(&json!([]))).unwrap_err(), "messages array is required");
        assert_eq!(
            validate_messages(Some(&json!([{ "role": "user" }]))).unwrap_err(),
            "Each message must have role and content"
        );
        assert_eq!(
            validate_messages(Some(&json!([{ "role": "tool", "content": "x" }]))).unwrap_err(),
            "Invalid message role"
        );
        assert_eq!(validate_messages(Some(&json!([{ "role": "user", "content": "x" }]))).unwrap().len(), 1);
        assert!(language_instruction(Some("de")).starts_with("Always respond in German,"));
        assert!(language_instruction(Some("xx")).contains("the user interface language"));
    }
}
