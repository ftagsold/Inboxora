//! Route table. Groups are added here as they are ported.

pub mod block_list;
pub mod categories;
pub mod contacts;
pub mod rules;
pub mod search;
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

/// Authenticated route groups by mount prefix (Express `app.use(prefix, router)` order).
fn groups() -> Vec<(&'static str, Router<AppState>)> {
    vec![
        ("/api/block-list", block_list::router()),
        ("/api/contacts", contacts::router()),
        ("/api/search", search::router()),
        ("/api/rules", rules::router()),
    ]
}

pub fn router(state: &AppState) -> Router<AppState> {
    groups()
        .into_iter()
        .fold(Router::new().merge(system::router()).merge(categories::router()), |r, (prefix, group)| {
            r.merge(authed(state, prefix, group))
        })
}
