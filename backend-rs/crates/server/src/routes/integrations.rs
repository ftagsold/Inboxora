//! `/api/integrations`: admin-managed OAuth app credentials (port of `routes/integrations.ts`).
//! Stored configs are applied to the runtime environment overlay (`MS_*`), which the OAuth
//! flows read.

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use inboxora_core::config::{env, set_env};
use inboxora_core::pg::{Db, Q, Val};
use inboxora_core::{Error, Result, crypto, params};
use serde_json::{Map, Value, json};

use super::util::{err, ok};
use crate::http::{Admin, Auth, Body};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list)).route("/status", get(status)).route("/{provider}", post(save).delete(remove))
}

const MASK: &str = "••••••••";

/// `applyMicrosoftConfig(config)`.
fn apply_microsoft(config: &Map<String, Value>) {
    let text = |k: &str| config.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
    set_env("MS_CLIENT_ID", text("clientId"));
    set_env("MS_TENANT_ID", text("tenantId"));
    set_env("MS_REDIRECT_URI", text("redirectUri"));
    set_env("MS_CLIENT_SECRET", text("clientSecret").and_then(|s| crypto::decrypt(&s)).filter(|s| !s.is_empty()));
}

/// `loadIntegrationConfigs()`: apply stored configs at startup.
pub async fn load_integration_configs(db: &Db) {
    match db.json_rows("SELECT provider, config FROM integration_config", params![]).await {
        Ok(rows) => {
            for row in rows {
                if row.get("provider").and_then(Value::as_str) == Some("microsoft")
                    && let Some(Value::Object(cfg)) = row.get("config")
                {
                    apply_microsoft(cfg);
                }
            }
            tracing::info!("Integration configs loaded");
        }
        Err(e) => tracing::error!("Failed to load integration configs: {e}"),
    }
}

async fn list(State(s): State<AppState>, _a: Admin) -> Result<Response> {
    let rows = s.db.json_rows("SELECT provider, config, updated_at FROM integration_config", params![]).await?;
    let mut configs = Map::new();
    for row in rows {
        let mut cfg = match row.get("config") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        if inboxora_core::json::truthy(cfg.get("clientSecret")) {
            cfg.insert("clientSecret".into(), Value::String(MASK.into()));
        }
        cfg.insert("updated_at".into(), row.get("updated_at").cloned().unwrap_or(Value::Null));
        configs.insert(row.get("provider").and_then(Value::as_str).unwrap_or_default().to_owned(), Value::Object(cfg));
    }
    Ok(ok(Value::Object(configs)))
}

async fn status(_auth: Auth) -> Response {
    ok(json!({ "microsoft": { "configured": env("MS_CLIENT_ID").is_some() } }))
}

async fn save(State(s): State<AppState>, _a: Admin, Path(provider): Path<String>, body: Body) -> Result<Response> {
    if provider != "microsoft" {
        return Ok(err(400, "Unknown provider"));
    }
    let mut config = match body.0 {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    if config.get("clientSecret").and_then(Value::as_str) == Some(MASK) {
        let existing =
            s.db.json_opt("SELECT config FROM integration_config WHERE provider = $1", params![&provider]).await?;
        match existing.and_then(|r| r.get("config").and_then(|c| c.get("clientSecret")).cloned()) {
            Some(secret) => {
                config.insert("clientSecret".into(), secret);
            }
            None => {
                config.shift_remove("clientSecret");
            }
        }
    }
    if let Some(secret) = config.get("clientSecret").filter(|v| inboxora_core::json::truthy(Some(v))).cloned() {
        let plain = crate::services::dav_xml::js_string(&secret);
        if !plain.starts_with("enc:v1:") {
            config.insert("clientSecret".into(), Value::String(crypto::encrypt(&plain).map_err(Error::internal)?));
        }
    }
    s.db.exec(
        "INSERT INTO integration_config (provider, config) VALUES ($1, $2) ON CONFLICT (provider) DO UPDATE SET config = EXCLUDED.config, updated_at = NOW()",
        &[Val::from(&provider), Val::Json(Value::Object(config.clone()))],
    )
    .await?;
    apply_microsoft(&config);
    Ok(ok(json!({ "ok": true })))
}

async fn remove(State(s): State<AppState>, _a: Admin, Path(provider): Path<String>) -> Result<Response> {
    s.db.exec("DELETE FROM integration_config WHERE provider = $1", params![&provider]).await?;
    if provider == "microsoft" {
        for k in ["MS_CLIENT_ID", "MS_CLIENT_SECRET", "MS_TENANT_ID", "MS_REDIRECT_URI"] {
            set_env(k, None);
        }
    }
    Ok(ok(json!({ "ok": true })))
}
