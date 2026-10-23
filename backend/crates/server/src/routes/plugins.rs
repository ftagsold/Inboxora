//! `/api/plugins`. Plugins are not part of the Rust backend, so the registry is empty: this is
//! exactly what the Node routes answered with no plugin loaded.

use axum::Router;
use axum::extract::Path;
use axum::response::Response;
use axum::routing::{get, patch};
use serde_json::json;

use crate::http::Auth;
use crate::routes::util::{err, ok};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list)).route("/{id}", patch(update))
}

async fn list(_auth: Auth) -> Response {
    ok(json!([]))
}

async fn update(_auth: Auth, Path(_id): Path<String>) -> Response {
    err(404, "Unknown plugin")
}
