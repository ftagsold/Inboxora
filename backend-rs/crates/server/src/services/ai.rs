//! AI provider access (port of `services/aiProvider.ts` and `services/openaiCodexResponses.ts`):
//! the admin configuration in `system_settings.ai_config`, an OpenAI-compatible chat
//! completions adapter (API key) and the ChatGPT Codex Responses adapter (device OAuth),
//! both streaming over SSE with bounded events and output.

use std::ops::ControlFlow;
use std::time::Duration;

use inboxora_core::pg::{Db, Q};
use inboxora_core::{crypto, params};
use inboxora_net::reqwest;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::ai_http::{PUBLIC, SseError, parse_json, read_limited, read_sse_data, sanitize_text};
use super::codex_auth;
use crate::state::AppState;

pub const API_KEY: &str = "api-key";
pub const CHATGPT: &str = "chatgpt";
pub const MASKED_API_KEY: &str = "••••••••";
pub const DEFAULT_CODEX_MODEL: &str = "gpt-5.6-luna";
const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const INSTRUCTIONS: &str = "You are Inboxora, a helpful email assistant.";
const ERROR_BODY_LIMIT: usize = 8 * 1024;
const JSON_BODY_LIMIT: usize = 2 * 1024 * 1024;
const SSE_EVENT_LIMIT: usize = 256 * 1024;
const OUTPUT_LIMIT_CHARS: usize = 2 * 1024 * 1024;
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(60);
const STREAM_TIMEOUT: Duration = Duration::from_secs(120);

/// `AiProviderError` / `CodexResponseError` / `CodexAuthError` as one value.
#[derive(Debug, Clone)]
pub struct AiError {
    pub message: String,
    pub status: u16,
    /// Safe to show the caller even on a 5xx.
    pub expose: bool,
}

impl AiError {
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        AiError { message: message.into(), status, expose: false }
    }
    fn exposed(message: impl Into<String>, status: u16) -> Self {
        AiError { message: message.into(), status, expose: true }
    }
}

impl From<codex_auth::CodexError> for AiError {
    fn from(e: codex_auth::CodexError) -> Self {
        AiError::new(e.message, e.status)
    }
}

impl From<inboxora_core::Error> for AiError {
    fn from(e: inboxora_core::Error) -> Self {
        AiError::new(e.to_string(), 500)
    }
}

pub type AiResult<T> = Result<T, AiError>;

fn clean(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).map(|s| s.trim().to_owned()).unwrap_or_default()
}

/// `normalizeAiConfig`.
pub fn normalize(raw: &Value) -> Value {
    let api = raw.get("apiKeyConfig").filter(|v| v.is_object()).unwrap_or(raw);
    let chatgpt = raw.get("chatgptConfig").filter(|v| v.is_object());
    let provider =
        raw.get("provider").and_then(Value::as_str).filter(|p| *p == API_KEY || *p == CHATGPT).unwrap_or(API_KEY);
    let model = clean(chatgpt.and_then(|c| c.get("model")));
    let features = raw.get("features");
    json!({
        "enabled": raw.get("enabled") != Some(&Value::Bool(false)),
        "provider": provider,
        "apiKeyConfig": {
            "baseUrl": clean(api.get("baseUrl")).trim_end_matches('/'),
            "apiKey": api.get("apiKey").filter(|v| inboxora_core::json::truthy(Some(v))).cloned().unwrap_or(Value::Null),
            "model": clean(api.get("model")),
        },
        "chatgptConfig": { "model": if model.is_empty() { DEFAULT_CODEX_MODEL.to_owned() } else { model } },
        "features": {
            "compose": features.and_then(|f| f.get("compose")) != Some(&Value::Bool(false)),
            "summarize": features.and_then(|f| f.get("summarize")) != Some(&Value::Bool(false)),
        },
    })
}

fn public(config: Option<Value>) -> Value {
    let Some(mut c) = config else { return Value::Null };
    let has_key = c["apiKeyConfig"]["apiKey"].as_str().is_some_and(|k| !k.is_empty());
    c["apiKeyConfig"]["apiKey"] = json!(if has_key { MASKED_API_KEY } else { "" });
    c
}

/// `loadAiConfig`.
pub async fn load_config(db: &impl Q) -> inboxora_core::Result<Option<Value>> {
    let row = db.opt("SELECT value FROM system_settings WHERE key = 'ai_config'", params![]).await?;
    Ok(row.and_then(|r| r.get::<_, Option<String>>("value")).and_then(|v| parse_json(&v)).map(|v| normalize(&v)))
}

pub async fn admin_config(db: &Db) -> AiResult<Value> {
    Ok(public(load_config(db).await?))
}

/// `saveAiConfig`.
pub async fn save_config(db: &Db, input: &Value) -> AiResult<Value> {
    let provider = input.get("provider").and_then(Value::as_str);
    if !matches!(provider, Some(API_KEY | CHATGPT)) {
        return Err(AiError::new("Unknown AI provider", 400));
    }
    if input.get("enabled") != Some(&Value::Bool(false))
        && provider == Some(CHATGPT)
        && clean(input.get("chatgptConfig").and_then(|c| c.get("model"))).is_empty()
    {
        return Err(AiError::new("ChatGPT model name is required", 400));
    }
    let existing = load_config(db).await?;
    let mut incoming = input
        .get("apiKeyConfig")
        .filter(|v| inboxora_core::json::truthy(Some(v)))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let key = incoming.get("apiKey").and_then(Value::as_str).filter(|k| !k.is_empty() && *k != MASKED_API_KEY);
    let stored = match key {
        Some(k) => json!(crypto::encrypt(k).map_err(|e| AiError::new(e, 500))?),
        None => existing.as_ref().map_or(Value::Null, |e| e["apiKeyConfig"]["apiKey"].clone()),
    };
    if let Some(m) = incoming.as_object_mut() {
        m.insert("apiKey".into(), stored);
    } else {
        incoming = json!({ "apiKey": stored });
    }
    let mut merged = input.as_object().cloned().unwrap_or_default();
    merged.insert("apiKeyConfig".into(), incoming);
    let config = normalize(&Value::Object(merged));
    let base = config["apiKeyConfig"]["baseUrl"].as_str().unwrap_or("").to_owned();
    let model = config["apiKeyConfig"]["model"].as_str().unwrap_or("");
    if config["enabled"] == json!(true) && config["provider"] == json!(API_KEY) && (base.is_empty() || model.is_empty())
    {
        return Err(AiError::new("API base URL and model name are required", 400));
    }
    if config["provider"] == json!(API_KEY) && !base.is_empty() {
        let host = url::Url::parse(&base)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .ok_or_else(|| AiError::new("Invalid API base URL", 400))?;
        let policy = super::dav_client::connection_policy(db).await?;
        if let Some(e) = inboxora_net::validate_host(&host, policy.allow_private_hosts).await {
            return Err(AiError::new(format!("API base URL: {e}"), 400));
        }
    }
    db.exec(
        "INSERT INTO system_settings (key, value, updated_at) VALUES ('ai_config', $1, NOW()) ON CONFLICT (key) DO UPDATE SET value = $1, updated_at = NOW()",
        params![config.to_string()],
    )
    .await?;
    Ok(public(Some(config)))
}

pub async fn delete_config(db: &Db) -> AiResult<()> {
    db.exec("DELETE FROM system_settings WHERE key = 'ai_config'", params![]).await?;
    Ok(())
}

async fn selected(db: &Db) -> AiResult<Value> {
    let config = load_config(db).await?.ok_or_else(|| AiError::new("AI provider not configured", 503))?;
    if config["enabled"] != json!(true) {
        return Err(AiError::new("AI features are disabled", 503));
    }
    if config["provider"] == json!(API_KEY)
        && (config["apiKeyConfig"]["baseUrl"].as_str().unwrap_or("").is_empty()
            || config["apiKeyConfig"]["model"].as_str().unwrap_or("").is_empty())
    {
        return Err(AiError::new("AI provider not fully configured", 503));
    }
    Ok(config)
}

/// `getAiStatus`.
pub async fn status(db: &Db) -> AiResult<Value> {
    let Some(config) = load_config(db).await?.filter(|c| c["enabled"] == json!(true)) else {
        let c = load_config(db).await?;
        return Ok(json!({
            "enabled": false,
            "provider": c.as_ref().map_or(json!(API_KEY), |c| c["provider"].clone()),
            "features": c.as_ref().map_or(json!({}), |c| c["features"].clone()),
            "reconnectRequired": false,
        }));
    };
    if config["provider"] == json!(API_KEY) {
        let enabled = !config["apiKeyConfig"]["baseUrl"].as_str().unwrap_or("").is_empty()
            && !config["apiKeyConfig"]["model"].as_str().unwrap_or("").is_empty();
        return Ok(
            json!({ "enabled": enabled, "provider": config["provider"], "features": config["features"], "reconnectRequired": false }),
        );
    }
    let connection = codex_auth::status(db, None).await?;
    Ok(json!({
        "enabled": connection["connected"] == json!(true) && !config["chatgptConfig"]["model"].as_str().unwrap_or("").is_empty(),
        "provider": config["provider"],
        "features": config["features"],
        "reconnectRequired": connection["reconnectRequired"] == json!(true),
        "connection": connection,
    }))
}

// ── adapters ────────────────────────────────────────────────────────────────────────────────

fn redact(text: &str, secrets: &[&str]) -> String {
    secrets.iter().filter(|s| !s.is_empty()).fold(text.to_owned(), |t, s| t.replace(s, "[redacted]"))
}

/// `providerError`.
fn provider_error(status: u16, text: &str, secrets: &[&str]) -> AiError {
    let parsed = parse_json(text);
    let raw = parsed.as_ref().and_then(|p| match p.get("error") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(e) => e.get("message").and_then(Value::as_str).map(str::to_owned),
        None => None,
    });
    let detail = sanitize_text(&redact(raw.as_deref().unwrap_or(text), secrets), 1000);
    AiError::exposed(
        format!(
            "AI provider error ({status}){}",
            if detail.is_empty() { String::new() } else { format!(": {detail}") }
        ),
        502,
    )
}

fn request_error(e: &reqwest::Error) -> AiError {
    if e.is_timeout() {
        return AiError::exposed("AI provider request timed out", 504);
    }
    let msg = sanitize_text(&e.to_string(), 1000);
    AiError::exposed(
        format!("AI provider request failed: {}", if msg.is_empty() { "network error" } else { &msg }),
        502,
    )
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

fn messages_json(m: &[Message]) -> Value {
    Value::Array(m.iter().map(|m| json!({ "role": m.role, "content": m.content })).collect())
}

async fn api_key_request(
    db: &Db,
    config: &Value,
    body: Value,
    timeout: Duration,
) -> AiResult<(reqwest::Response, Option<String>)> {
    let key = config["apiKeyConfig"]["apiKey"].as_str().and_then(crypto::decrypt);
    let url = format!("{}/chat/completions", config["apiKeyConfig"]["baseUrl"].as_str().unwrap_or(""));
    let policy = super::dav_client::connection_policy(db).await?;
    let mut req = inboxora_net::safe_request(
        reqwest::Method::POST,
        &url,
        inboxora_net::Guard { allow_private: policy.allow_private_hosts, require_https: None },
    )
    .map_err(|e| AiError::exposed(format!("AI provider request failed: {e}"), 502))?
    .timeout(timeout)
    .header("Content-Type", "application/json")
    .body(body.to_string());
    if let Some(k) = &key {
        req = req.header("Authorization", format!("Bearer {k}"));
    }
    let response = req.send().await.map_err(|e| request_error(&e))?;
    Ok((response, key))
}

async fn complete_api_key(
    db: &Db,
    config: &Value,
    messages: &[Message],
    max_tokens: Option<u32>,
    allow_empty: bool,
) -> AiResult<String> {
    let mut body = json!({ "model": config["apiKeyConfig"]["model"], "messages": messages_json(messages) });
    if let Some(n) = max_tokens {
        body["max_tokens"] = json!(n);
    }
    body["stream"] = json!(false);
    body["think"] = json!(false);
    let (response, key) = api_key_request(db, config, body, COMPLETE_TIMEOUT).await?;
    let secrets = [key.as_deref().unwrap_or("")];
    let status = response.status();
    let text = read_limited(response, if status.is_success() { JSON_BODY_LIMIT } else { ERROR_BODY_LIMIT })
        .await
        .map_err(|e| request_error(&e))?;
    if !status.is_success() {
        return Err(provider_error(status.as_u16(), &text, &secrets));
    }
    let parsed = parse_json(&text);
    let choice = parsed.as_ref().and_then(|p| p.get("choices")).and_then(|c| c.get(0));
    match choice.and_then(|c| c.get("message")).and_then(|m| m.get("content")) {
        Some(Value::String(s)) => Ok(s.clone()),
        None | Some(Value::Null) if choice.is_some() => {
            if allow_empty {
                return Ok(String::new());
            }
            let reason = choice.and_then(|c| c.get("finish_reason")).and_then(Value::as_str).unwrap_or("unknown");
            Err(AiError::exposed(format!("AI provider returned an empty completion (finish_reason: {reason})"), 502))
        }
        _ => Err(AiError::exposed("AI provider returned an invalid completion", 502)),
    }
}

/// Deltas from an OpenAI-compatible `chat/completions` stream.
async fn stream_api_key(
    db: &Db,
    config: &Value,
    messages: &[Message],
    tx: &mpsc::UnboundedSender<AiResult<String>>,
) -> AiResult<()> {
    let body = json!({ "model": config["apiKeyConfig"]["model"], "messages": messages_json(messages), "stream": true });
    let (response, key) = api_key_request(db, config, body, STREAM_TIMEOUT).await?;
    let secrets = [key.as_deref().unwrap_or("")];
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = read_limited(response, ERROR_BODY_LIMIT).await.unwrap_or_default();
        return Err(provider_error(status, &text, &secrets));
    }
    let (mut completed, mut chars, mut failure): (bool, usize, Option<AiError>) = (false, 0, None);
    let mut done = false;
    let result = read_sse_data(response, SSE_EVENT_LIMIT, |data| {
        if data.trim() == "[DONE]" {
            done = true;
            return ControlFlow::Break(());
        }
        let Some(event) = parse_json(&data) else {
            failure = Some(AiError::new("AI provider returned a malformed stream event", 502));
            return ControlFlow::Break(());
        };
        if let Some(e) = event.get("error") {
            failure = Some(provider_error(502, &json!({ "error": e }).to_string(), &secrets));
            return ControlFlow::Break(());
        }
        let choice = event.get("choices").and_then(|c| c.get(0));
        if choice.and_then(|c| c.get("finish_reason")).and_then(Value::as_str).is_some_and(|r| !r.is_empty()) {
            completed = true;
        }
        if let Some(delta) = choice
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
        {
            chars += delta.chars().count();
            if chars > OUTPUT_LIMIT_CHARS {
                failure = Some(AiError::new("AI provider output was too large", 502));
                return ControlFlow::Break(());
            }
            if tx.send(Ok(delta.to_owned())).is_err() {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })
    .await;
    if let Some(f) = failure {
        return Err(f);
    }
    match result {
        Err(SseError::EmptyBody) => return Err(AiError::new("AI provider returned an empty stream", 502)),
        Err(SseError::EventTooLarge) => return Err(AiError::new("AI provider stream event was too large", 502)),
        Err(SseError::Transport) => return Err(AiError::exposed("AI provider request failed: network error", 502)),
        Ok(()) => {}
    }
    if !done && !completed && !tx.is_closed() {
        return Err(AiError::exposed("AI provider stream ended without a completion marker", 502));
    }
    Ok(())
}

/// `buildCodexRequest`.
fn codex_request(model: &str, messages: &[Message]) -> AiResult<Value> {
    if model.trim().is_empty() {
        return Err(AiError::new("ChatGPT model is required", 500));
    }
    let input: Vec<Value> = messages
        .iter()
        .map(|m| match m.role.as_str() {
            "system" => Ok(json!({ "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": m.content }] })),
            "assistant" => Ok(json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": m.content }] })),
            "user" => Ok(json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": m.content }] })),
            r => Err(AiError::new(format!("Unsupported message role: {r}"), 500)),
        })
        .collect::<AiResult<_>>()?;
    Ok(json!({ "model": model.trim(), "instructions": INSTRUCTIONS, "input": input, "store": false, "stream": true }))
}

fn codex_event_error(event: &Value, fallback: &str) -> AiError {
    let pick = |k: &str| {
        event
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| event.get("error").and_then(|e| e.get(k)).and_then(Value::as_str).map(str::to_owned))
            .or_else(|| {
                event
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get(k))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
    };
    let message = sanitize_text(&pick("message").unwrap_or_default(), 1000);
    AiError::new(if message.is_empty() { fallback.to_owned() } else { format!("{fallback}: {message}") }, 500)
}

/// Deltas from the ChatGPT Codex Responses stream. `Err` with status 401 asks for a refresh.
async fn stream_codex(
    token: &str,
    account: &str,
    model: &str,
    messages: &[Message],
    tx: &mpsc::UnboundedSender<AiResult<String>>,
    emitted: &mut bool,
) -> AiResult<()> {
    let request = codex_request(model, messages)?;
    let response = PUBLIC
        .post(CODEX_RESPONSES_URL)
        .timeout(STREAM_TIMEOUT)
        .header("Authorization", format!("Bearer {token}"))
        .header("chatgpt-account-id", account)
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", "mailflow")
        .header("Accept", "text/event-stream")
        .header("Content-Type", "application/json")
        .body(request.to_string())
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                AiError::new(format!("ChatGPT request timed out after {}ms", STREAM_TIMEOUT.as_millis()), 500)
            } else {
                let m = sanitize_text(&e.to_string(), 1000);
                AiError::new(
                    format!("ChatGPT request failed: {}", if m.is_empty() { "network error" } else { &m }),
                    500,
                )
            }
        })?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = read_limited(response, ERROR_BODY_LIMIT).await.unwrap_or_default();
        let detail = match serde_json::from_str::<Value>(&body) {
            Ok(v) => match v.get("error") {
                Some(Value::String(s)) => s.clone(),
                Some(e) => e.get("message").and_then(Value::as_str).unwrap_or("").to_owned(),
                None => String::new(),
            },
            Err(_) => body,
        };
        let safe = sanitize_text(&detail, 1000);
        return Err(AiError::new(
            format!(
                "ChatGPT provider error ({status}){}",
                if safe.is_empty() { String::new() } else { format!(": {safe}") }
            ),
            status,
        ));
    }
    let (mut terminal, mut chars, mut failure) = (false, 0usize, None::<AiError>);
    let result = read_sse_data(response, SSE_EVENT_LIMIT, |data| {
        if data.trim() == "[DONE]" {
            terminal = true;
            return ControlFlow::Break(());
        }
        let Ok(event) = serde_json::from_str::<Value>(&data) else {
            failure = Some(AiError::new("Malformed ChatGPT response event", 500));
            return ControlFlow::Break(());
        };
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.completed" => {
                terminal = true;
                ControlFlow::Break(())
            }
            "response.output_text.delta" => {
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                chars += delta.chars().count();
                if chars > OUTPUT_LIMIT_CHARS {
                    failure = Some(AiError::new("ChatGPT response output was too large", 500));
                    return ControlFlow::Break(());
                }
                if !delta.is_empty() {
                    *emitted = true;
                    if tx.send(Ok(delta.to_owned())).is_err() {
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            }
            "response.failed" => {
                failure = Some(codex_event_error(&event, "ChatGPT response failed"));
                ControlFlow::Break(())
            }
            "response.incomplete" => {
                failure = Some(codex_event_error(&event, "ChatGPT response was incomplete"));
                ControlFlow::Break(())
            }
            "error" => {
                failure = Some(codex_event_error(&event, "ChatGPT stream error"));
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        }
    })
    .await;
    if let Some(f) = failure {
        return Err(f);
    }
    match result {
        Err(SseError::EmptyBody) => return Err(AiError::new("ChatGPT response body was empty", 500)),
        Err(SseError::EventTooLarge) => return Err(AiError::new("ChatGPT response event was too large", 500)),
        Err(SseError::Transport) => return Err(AiError::new("ChatGPT request failed: network error", 500)),
        Ok(()) => {}
    }
    if !terminal && !tx.is_closed() {
        return Err(AiError::new("ChatGPT response ended before completion", 500));
    }
    Ok(())
}

/// Run one chat against the selected provider, sending deltas to `tx`.
async fn run_chat(db: &Db, messages: &[Message], tx: &mpsc::UnboundedSender<AiResult<String>>) -> AiResult<()> {
    let config = selected(db).await?;
    if config["provider"] == json!(API_KEY) {
        return stream_api_key(db, &config, messages, tx).await;
    }
    let model = config["chatgptConfig"]["model"].as_str().unwrap_or("").to_owned();
    let mut force = false;
    loop {
        let (token, account) = codex_auth::access(db, force).await?;
        let mut emitted = false;
        match stream_codex(&token, &account, &model, messages, tx, &mut emitted).await {
            Err(e) if e.status == 401 && !force && !emitted => force = true,
            other => return other,
        }
    }
}

/// `streamChat`: deltas on a channel; the stream ends after an `Err` or completion. Dropping
/// the receiver stops the provider request.
pub fn stream_chat(db: Db, messages: Vec<Message>) -> mpsc::UnboundedReceiver<AiResult<String>> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        if let Err(e) = run_chat(&db, &messages, &tx).await {
            let _ = tx.send(Err(e));
        }
    });
    rx
}

/// `completeText`.
pub async fn complete_text(
    db: &Db,
    messages: &[Message],
    max_tokens: Option<u32>,
    allow_empty: bool,
) -> AiResult<String> {
    let config = selected(db).await?;
    if config["provider"] == json!(API_KEY) {
        return complete_api_key(db, &config, messages, max_tokens, allow_empty).await;
    }
    let mut rx = stream_chat(db.clone(), messages.to_vec());
    let mut text = String::new();
    while let Some(delta) = rx.recv().await {
        text.push_str(&delta?);
    }
    Ok(text)
}

/// `testAiProvider`.
pub async fn test_provider(db: &Db) -> AiResult<Value> {
    complete_text(
        db,
        &[Message { role: "user".into(), content: "Reply with only the word \"ok\".".into() }],
        Some(16),
        true,
    )
    .await?;
    Ok(json!({ "ok": true }))
}

/// `aiClassifyMessage`: one of the five category names, or `None` when unavailable.
pub async fn classify_message(state: &AppState, message: &Map<String, Value>) -> Option<String> {
    let s = |k: &str| message.get(k).and_then(Value::as_str).unwrap_or("");
    let snippet = s("snippet");
    let prompt = format!(
        "Classify this email into exactly one category. Reply with only the category name, nothing else.\n\nCategories:\n\
         - primary: personal email, work correspondence, direct replies\n\
         - newsletter: mailing lists, blog digests, subscribed newsletters\n\
         - promotion: marketing, sales, discount offers, advertisements\n\
         - automated: transactional email, receipts, notifications, alerts, password resets\n\
         - social: social media notifications (Facebook, Twitter, LinkedIn, etc.)\n\n\
         From: {}\nSubject: {}\n{}\n\nCategory:",
        if s("from_email").is_empty() { "(unknown)" } else { s("from_email") },
        inboxora_mail::text::js_substring(s("subject"), 200),
        if snippet.is_empty() {
            String::new()
        } else {
            format!("Preview: {}", inboxora_mail::text::js_substring(snippet, 300))
        },
    );
    let response =
        complete_text(&state.db, &[Message { role: "user".into(), content: prompt }], Some(1024), false).await.ok()?;
    let category = response.trim().to_lowercase();
    ["primary", "newsletter", "promotion", "automated", "social"].contains(&category.as_str()).then_some(category)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_masks() {
        let c = normalize(
            &json!({ "baseUrl": " https://x.test/v1// ", "apiKey": "enc", "model": " m ", "features": { "compose": false } }),
        );
        assert_eq!(c["provider"], "api-key");
        assert_eq!(c["apiKeyConfig"]["baseUrl"], "https://x.test/v1");
        assert_eq!(c["apiKeyConfig"]["model"], "m");
        assert_eq!(c["chatgptConfig"]["model"], DEFAULT_CODEX_MODEL);
        assert_eq!(c["features"], json!({ "compose": false, "summarize": true }));
        assert_eq!(public(Some(c))["apiKeyConfig"]["apiKey"], MASKED_API_KEY);
        let e = provider_error(401, r#"{"error":{"message":"bad key sk-123"}}"#, &["sk-123"]);
        assert_eq!(e.message, "AI provider error (401): bad key [redacted]");
        assert!(e.expose && e.status == 502);
        let r = codex_request("m", &[Message { role: "system".into(), content: "s".into() }]).unwrap();
        assert_eq!(r["input"][0]["role"], "developer");
        assert!(codex_request("m", &[Message { role: "tool".into(), content: "x".into() }]).is_err());
    }
}
