//! `/api/dav-credentials`: DAV application passwords.

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{delete, get};
use inboxora_core::pg::Q;
use inboxora_core::{Result, params};
use serde_json::json;

use crate::http::{Auth, Body};
use crate::routes::util::{err, json, ok};
use crate::services::auth;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list).post(create)).route("/{id}", delete(revoke))
}

async fn list(State(s): State<AppState>, a: Auth) -> Result<Response> {
    let rows = s
        .db
        .json_rows("SELECT id, label, created_at, last_used_at FROM dav_app_passwords WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC", params![&a.user_id])
        .await?;
    Ok(ok(json!({ "credentials": rows })))
}

async fn create(State(s): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let (credential, secret) = auth::create_app_password(&s.db, &a.user_id, body.get("label")).await?;
    Ok(json(201, json!({ "credential": credential, "secret": secret })))
}

async fn revoke(State(s): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    let row = s
        .db
        .json_opt("UPDATE dav_app_passwords SET revoked_at = NOW() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL RETURNING id, revoked_at", params![&id, &a.user_id])
        .await?;
    Ok(match row {
        Some(c) => ok(json!({ "credential": c })),
        None => err(404, "DAV application password not found"),
    })
}
