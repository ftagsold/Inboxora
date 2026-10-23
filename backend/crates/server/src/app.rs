//! Assemble the router with the middleware stack.

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::any;

use crate::frontend::{self, Frontend};
use crate::http::middleware;
use crate::state::AppState;

pub fn build(state: AppState, frontend: Frontend) -> Router {
    // Layers wrap outside-in: the last `.layer` runs first.
    crate::routes::router(&state)
        .route("/ws", any(crate::ws::upgrade))
        .fallback(middleware::not_found)
        // Express answers an unsupported method on a known path with its 404 page.
        .method_not_allowed_fallback(middleware::not_found)
        .layer(from_fn(middleware::expose_matched_path))
        .layer(from_fn(middleware::lock))
        .layer(from_fn(middleware::csrf))
        .layer(from_fn(middleware::etag))
        .layer(from_fn_with_state(state.clone(), middleware::session))
        .layer(from_fn(middleware::security_headers))
        .layer(from_fn(middleware::timing))
        .layer(from_fn_with_state(state.clone(), middleware::cors))
        // Bodies are read by extractors with their own per-path limits.
        .layer(DefaultBodyLimit::disable())
        // The web app and ntfy never reach the API stack.
        .layer(from_fn_with_state(frontend, frontend::dispatch))
        .with_state(state)
}
