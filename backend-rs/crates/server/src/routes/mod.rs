//! Route table. Groups are added here as they are ported.

pub mod block_list;
pub mod contacts;
pub mod system;
pub mod util;

use axum::Router;
use axum::middleware::from_fn_with_state;

use crate::state::AppState;

/// A route group mounted under `prefix` behind `requireAuth` (applied to the whole prefix,
/// like Express's `router.use(requireAuth)`).
fn authed(state: &AppState, prefix: &str, group: Router<AppState>) -> Router<AppState> {
    let group = group
        .fallback(crate::http::middleware::not_found)
        .method_not_allowed_fallback(crate::http::middleware::not_found)
        .layer(from_fn_with_state(state.clone(), util::require_auth));
    Router::new().nest(prefix, group)
}

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new().merge(system::router()).merge(authed(state, "/api/block-list", block_list::router())).merge(authed(
        state,
        "/api/contacts",
        contacts::router(),
    ))
}
