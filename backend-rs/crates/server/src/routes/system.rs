//! Unauthenticated probes.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/health", get(health)).route("/api/version", get(version))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn version(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "version": state.cfg.app_version, "sha": state.cfg.build_sha }))
}
