//! `/api/todoist` (port of `routes/todoist.ts`): connect an API token (validated against
//! Todoist, stored encrypted), list projects and labels, and create tasks.

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::{delete, get, post};
use inboxora_core::pg::Q;
use inboxora_core::{Result, crypto, params};
use inboxora_net::reqwest;
use serde_json::{Map, Value, json};

use crate::http::{Auth, Body};
use crate::routes::util::{err, ok};
use crate::services::ai_http::PUBLIC;
use crate::state::AppState;

const API: &str = "https://api.todoist.com/api/v1";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/connect", post(connect))
        .route("/disconnect", delete(disconnect))
        .route("/projects", get(projects))
        .route("/labels", get(labels))
        .route("/tasks", post(create_task))
}

struct Failure {
    status: u16,
    message: String,
}

impl From<inboxora_core::Error> for Failure {
    fn from(e: inboxora_core::Error) -> Self {
        Failure { status: 500, message: e.to_string() }
    }
}

fn fail(f: Failure) -> Response {
    err(f.status, &f.message)
}

async fn token(s: &AppState, user: &str) -> std::result::Result<String, Failure> {
    let config =
        s.db.opt("SELECT config FROM user_integrations WHERE user_id = $1 AND provider = 'todoist'", params![user])
            .await?
            .and_then(|r| r.get::<_, Option<Value>>("config"));
    let Some(stored) = config.as_ref().and_then(|c| c.get("token")).and_then(Value::as_str).filter(|t| !t.is_empty())
    else {
        return Err(Failure { status: 409, message: "Todoist not connected".into() });
    };
    crypto::decrypt(stored)
        .filter(|t| !t.is_empty())
        .ok_or(Failure { status: 409, message: "Todoist credential could not be decrypted".into() })
}

/// `todoistFetch`.
async fn call(
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> std::result::Result<Value, Failure> {
    let mut req =
        PUBLIC.request(method, format!("{API}{path}")).bearer_auth(token).header("Content-Type", "application/json");
    if let Some(b) = body {
        req = req.body(b.to_string());
    }
    let res = req.send().await.map_err(|e| Failure { status: 500, message: e.to_string() })?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()).map(str::to_owned));
        return Err(Failure {
            status: status.as_u16(),
            message: detail.unwrap_or_else(|| format!("Todoist API error {}", status.as_u16())),
        });
    }
    serde_json::from_str(&text).map_err(|e| Failure { status: 500, message: e.to_string() })
}

async fn status(State(s): State<AppState>, a: Auth) -> Result<Response> {
    Ok(
        match s
            .db
            .opt("SELECT id FROM user_integrations WHERE user_id = $1 AND provider = 'todoist'", params![&a.user_id])
            .await
        {
            Ok(r) => ok(json!({ "connected": r.is_some() })),
            Err(e) => err(500, &e.to_string()),
        },
    )
}

async fn connect(State(s): State<AppState>, a: Auth, b: Body) -> Result<Response> {
    let Some(token) = b.str("token").map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(err(400, "API token is required"));
    };
    let valid = match PUBLIC.get(format!("{API}/projects")).bearer_auth(token).send().await {
        Ok(r) => r.status().is_success(),
        Err(e) => return Ok(err(500, &e.to_string())),
    };
    if !valid {
        return Ok(err(400, "Invalid Todoist API token"));
    }
    let encrypted = match crypto::encrypt(token) {
        Ok(e) => e,
        Err(e) => return Ok(err(500, &e)),
    };
    s.db.exec(
        "INSERT INTO user_integrations (user_id, provider, config) VALUES ($1, 'todoist', $2::jsonb) \
         ON CONFLICT (user_id, provider) DO UPDATE SET config = EXCLUDED.config, updated_at = NOW()",
        params![&a.user_id, json!({ "token": encrypted }).to_string()],
    )
    .await?;
    Ok(ok(json!({ "ok": true })))
}

async fn disconnect(State(s): State<AppState>, a: Auth) -> Result<Response> {
    Ok(
        match s
            .db
            .exec("DELETE FROM user_integrations WHERE user_id = $1 AND provider = 'todoist'", params![&a.user_id])
            .await
        {
            Ok(_) => ok(json!({ "ok": true })),
            Err(e) => err(500, &e.to_string()),
        },
    )
}

async fn list(s: &AppState, user: &str, path: &str) -> Response {
    let run = async {
        let t = token(s, user).await?;
        let data = call(&t, reqwest::Method::GET, path, None).await?;
        Ok::<_, Failure>(data.get("results").cloned().unwrap_or(data))
    };
    match run.await {
        Ok(v) => ok(v),
        Err(f) => fail(f),
    }
}

async fn projects(State(s): State<AppState>, a: Auth) -> Response {
    list(&s, &a.user_id, "/projects").await
}

async fn labels(State(s): State<AppState>, a: Auth) -> Response {
    list(&s, &a.user_id, "/labels").await
}

async fn create_task(State(s): State<AppState>, a: Auth, b: Body) -> Response {
    let run = async {
        let t = token(&s, &a.user_id).await?;
        let content = b.str("content").map(str::trim).unwrap_or("");
        if content.is_empty() {
            return Ok(err(400, "Task title is required"));
        }
        let mut task = Map::new();
        task.insert("content".into(), json!(content));
        let truthy = |k: &str| b.get(k).filter(|v| inboxora_core::json::truthy(Some(v))).cloned();
        for k in ["description", "project_id", "due_string", "due_date"] {
            if let Some(v) = truthy(k) {
                task.insert(k.into(), v);
            }
        }
        if let Some(l) = b
            .get("labels")
            .filter(|l| l.as_array().is_some_and(|a| !a.is_empty()) || l.as_str().is_some_and(|s| !s.is_empty()))
        {
            task.insert("labels".into(), l.clone());
        }
        if let Some(p) = b.get("priority").filter(|p| p.as_f64().is_some_and(|n| n > 1.0)) {
            task.insert("priority".into(), p.clone());
        }
        let created = call(&t, reqwest::Method::POST, "/tasks", Some(Value::Object(task))).await?;
        let mut out = created.as_object().cloned().unwrap_or_default();
        let id = created.get("id").map(inboxora_mail::text::js_string).unwrap_or_else(|| "undefined".into());
        out.insert("url".into(), json!(format!("https://app.todoist.com/app/task/{id}")));
        Ok::<_, Failure>(ok(Value::Object(out)))
    };
    run.await.unwrap_or_else(fail)
}
