//! `/api/categories/*` (port of routes/categories.ts). Mounted on `/api` with per-route auth.

use std::sync::LazyLock;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{patch, post};
use inboxora_core::pg::{Db, Q, Val};
use inboxora_core::{Error, Result, params};
use regex::Regex;
use serde_json::{Value, json};

use crate::http::{Auth, Body};
use crate::routes::util::{err, json, ok};
use crate::services::categorizer::{self, BUILTIN_SETS};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/categories/sources", axum::routing::get(list_sources).post(add_source))
        .route("/api/categories/sources/{id}", patch(toggle_source).delete(delete_source))
        .route("/api/categories/sources/{id}/refresh", post(refresh_source))
        .route("/api/categories/recategorize/{account_id}", post(recategorize))
        .route("/api/categories/ai-classify/{message_id}", post(ai_classify))
}

static DOMAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9.-]*\.[a-z]{2,}$").expect("regex"));
static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._%+-]*@[a-z0-9][a-z0-9.-]*\.[a-z]{2,}$").expect("regex"));

async fn validate_subscription_url(raw: &str) -> Option<&'static str> {
    let Ok(url) = url::Url::parse(raw) else { return Some("Invalid URL") };
    if url.scheme() != "https" {
        return Some("URL must use HTTPS");
    }
    let host = url.host_str().unwrap_or("");
    inboxora_net::validate_host(host, false).await.map(|_| "URL host not allowed")
}

/// `fetchDomainList`: domains listed one per line (comments stripped), at most 512 KB.
pub async fn fetch_domain_list(url: &str) -> (Vec<String>, Option<String>) {
    use futures_util::StreamExt;
    let req = match inboxora_net::safe_get(url, inboxora_net::Guard::default()) {
        Ok(r) => r,
        Err(e) => return (Vec::new(), Some(e.to_string())),
    };
    let fetch = async {
        let resp = req.header("User-Agent", "Inboxora/4.0.0").send().await.map_err(|_| "fetch failed".to_owned())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "terminated".to_owned())?;
            body.extend_from_slice(&chunk);
            if body.len() > 512 * 1024 {
                return Err("Response too large (max 512 KB)".to_owned());
            }
        }
        Ok(body)
    };
    let body = match tokio::time::timeout(Duration::from_secs(8), fetch).await {
        Err(_) => return (Vec::new(), Some("Request timed out".into())),
        Ok(Err(e)) => return (Vec::new(), Some(e)),
        Ok(Ok(b)) => b,
    };
    let text = String::from_utf8_lossy(&body);
    let domains = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .map(|l| l.split('#').next().unwrap_or("").trim().to_lowercase())
        .filter(|l| !l.is_empty() && l.len() <= 253 && DOMAIN.is_match(l))
        .collect();
    (domains, None)
}

async fn store_fetch(db: &Db, id: &str, domains: &[String], error: Option<&str>) -> Result<()> {
    db.exec(
        "UPDATE category_list_sources SET resolved_domains = $1, last_fetched_at = NOW(), fetch_ok = $2, fetch_error = $3 WHERE id = $4",
        &[Val::from(domains.to_vec()), Val::Bool(error.is_none()), Val::from(error), Val::from(id)],
    )
    .await?;
    Ok(())
}

async fn list_sources(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let rows = s
        .db
        .json_rows(
            "SELECT id, source_type, value, label, enabled, array_length(resolved_domains, 1) AS domain_count, last_fetched_at, fetch_ok, \
             fetch_error, created_at FROM category_list_sources WHERE user_id = $1 ORDER BY source_type, created_at",
            params![&auth.user_id],
        )
        .await?;
    let sets: Vec<&str> = BUILTIN_SETS.iter().map(|(n, _)| *n).collect();
    Ok(ok(json!({ "sources": rows, "builtinSets": sets })))
}

async fn add_source(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let source_type = body.str("sourceType").unwrap_or("").to_owned();
    if !matches!(source_type.as_str(), "manual" | "builtin" | "url") {
        return Ok(err(400, "Invalid sourceType"));
    }
    let value = match body.str("value") {
        Some(v) if !v.trim().is_empty() => v.trim().to_lowercase(),
        _ => return Ok(err(400, "value is required")),
    };
    if source_type == "manual" && !(EMAIL.is_match(&value) || DOMAIN.is_match(&value)) {
        return Ok(err(400, "Invalid domain or email address"));
    }
    if source_type == "builtin" && categorizer::builtin_set(&value).is_none() {
        return Ok(err(400, &format!("Unknown built-in set: {value}")));
    }
    if source_type == "url"
        && let Some(e) = validate_subscription_url(&value).await
    {
        return Ok(err(400, e));
    }
    let label = match body.get("label") {
        None | Some(Value::Null) => None,
        Some(Value::String(l)) => Some(l.trim().to_owned()).filter(|l| !l.is_empty()),
        Some(_) => return Err(Error::internal("label?.trim is not a function")),
    };
    let row = match s
        .db
        .json_opt(
            "INSERT INTO category_list_sources (user_id, source_type, value, label) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (user_id, source_type, value) DO UPDATE SET enabled = true \
             RETURNING id, source_type, value, label, enabled, last_fetched_at, fetch_ok, fetch_error, created_at",
            params![&auth.user_id, &source_type, &value, label],
        )
        .await
    {
        Ok(r) => r,
        Err(e) if e.is_unique_violation() => return Ok(err(409, "This entry already exists")),
        Err(e) => return Err(e),
    };
    categorizer::invalidate_social_domain_cache(&auth.user_id);
    let source = row.map_or(Value::Null, Value::Object);
    if source_type == "url" {
        let id = source.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
        let (db, user) = (s.db.clone(), auth.user_id.clone());
        tokio::spawn(async move {
            let (domains, error) = fetch_domain_list(&value).await;
            if store_fetch(&db, &id, &domains, error.as_deref()).await.is_ok() {
                categorizer::invalidate_social_domain_cache(&user);
            }
        });
    }
    Ok(json(201, json!({ "source": source })))
}

async fn toggle_source(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
        return Ok(err(400, "enabled must be boolean"));
    };
    let row =
        s.db.json_opt(
            "UPDATE category_list_sources SET enabled = $1 WHERE id = $2 AND user_id = $3 \
             RETURNING id, source_type, value, label, enabled, last_fetched_at, fetch_ok, fetch_error",
            params![enabled, &id, &auth.user_id],
        )
        .await?;
    let Some(row) = row else { return Ok(err(404, "Source not found")) };
    categorizer::invalidate_social_domain_cache(&auth.user_id);
    Ok(ok(json!({ "source": row })))
}

async fn delete_source(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Response> {
    let row =
        s.db.opt(
            "DELETE FROM category_list_sources WHERE id = $1 AND user_id = $2 RETURNING id",
            params![&id, &auth.user_id],
        )
        .await?;
    if row.is_none() {
        return Ok(err(404, "Source not found"));
    }
    categorizer::invalidate_social_domain_cache(&auth.user_id);
    Ok(ok(json!({ "ok": true })))
}

async fn refresh_source(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Response> {
    let row =
        s.db.json_opt(
            "SELECT id, source_type, value FROM category_list_sources WHERE id = $1 AND user_id = $2",
            params![&id, &auth.user_id],
        )
        .await?;
    let Some(src) = row else { return Ok(err(404, "Source not found")) };
    if src.get("source_type").and_then(Value::as_str) != Some("url") {
        return Ok(err(400, "Only URL sources can be refreshed"));
    }
    let (domains, error) = fetch_domain_list(src.get("value").and_then(Value::as_str).unwrap_or("")).await;
    store_fetch(&s.db, src.get("id").and_then(Value::as_str).unwrap_or(""), &domains, error.as_deref()).await?;
    categorizer::invalidate_social_domain_cache(&auth.user_id);
    Ok(ok(json!({ "ok": true, "domainCount": domains.len(), "error": error })))
}

async fn recategorize(State(s): State<AppState>, auth: Auth, Path(account_id): Path<String>) -> Result<Response> {
    let row = s
        .db
        .opt(
            "SELECT ea.id FROM email_accounts ea JOIN users u ON u.id = ea.user_id WHERE ea.id = $1 AND ea.user_id = $2 \
             AND (ea.categorization_enabled = true OR (u.preferences->>'categorizationEnabled')::boolean = true)",
            params![&account_id, &auth.user_id],
        )
        .await?;
    if row.is_none() {
        return Ok(err(404, "Account not found or categorization not enabled"));
    }
    let (engine, user) = (s.mail.clone(), auth.user_id.clone());
    tokio::spawn(async move {
        match categorizer::backfill_categories(&engine, &account_id, &user).await {
            Ok(n) => tracing::info!("Re-categorization complete: {n} messages for account {account_id}"),
            Err(e) => tracing::error!("Re-categorization error for account {account_id}: {e}"),
        }
    });
    Ok(json(202, json!({ "ok": true })))
}

async fn ai_classify(State(s): State<AppState>, auth: Auth, Path(message_id): Path<String>) -> Result<Response> {
    if !inboxora_mail::util::is_uuid(&message_id) {
        return Ok(err(400, "Invalid message id"));
    }
    let row =
        s.db.json_opt(
            "SELECT m.id, m.account_id, m.folder, m.uid FROM messages m JOIN email_accounts a ON m.account_id = a.id \
             WHERE m.id = $1 AND a.user_id = $2 AND m.is_deleted = false",
            params![&message_id, &auth.user_id],
        )
        .await?;
    let Some(mut msg) = row else { return Ok(err(404, "Message not found")) };
    s.mail.hydrate_rows(std::slice::from_mut(&mut msg)).await;
    let category = crate::services::ai::classify_message(&s, &msg).await;
    let Some(category) = category else { return Ok(err(503, "AI classification unavailable or failed")) };
    s.db.exec(
        "UPDATE messages SET category = $1 FROM email_accounts a WHERE messages.id = $2 AND messages.account_id = a.id AND a.user_id = $3",
        &[if category == "primary" { Val::Null } else { Val::from(category.as_str()) }, Val::from(&message_id), Val::from(&auth.user_id)],
    )
    .await?;
    Ok(ok(json!({ "ok": true, "category": category })))
}
