//! Unauthenticated probes.

use axum::Router;
use axum::extract::State;
use axum::routing::get;
use inboxora_core::error::Js;
use serde_json::json;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/health", get(health)).route("/api/version", get(version))
}

async fn health() -> Js {
    Js(json!({ "status": "ok" }))
}

async fn version(State(state): State<AppState>) -> Js {
    Js(json!({ "version": state.cfg.app_version, "sha": state.cfg.build_sha }))
}
