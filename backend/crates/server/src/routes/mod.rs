//! Route table. Groups are added here as they are ported.

pub mod accounts;
pub mod admin;
pub mod ai;
pub mod auth;
pub mod block_list;
pub mod caldav;
pub mod calendar;
pub mod calendar_events;
pub mod calendar_feed;
pub mod calendar_sources;
pub mod carddav;
pub mod categories;
pub mod contacts;
pub mod dav;
pub mod dav_credentials;
pub mod dav_servers;
pub mod diagnostics;
pub mod integrations;
pub mod mail;
pub mod oauth;
pub mod oidc;
pub mod plugins;
pub mod push;
pub mod rules;
pub mod search;
pub mod sender_favicons;
pub mod system;
pub mod todoist;
pub mod totp;
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

/// The `/api/admin` group: every request (including unknown subpaths) is admin-checked first.
fn admin_only(state: &AppState) -> Router<AppState> {
    let group = admin::router()
        .fallback(crate::http::middleware::not_found)
        .method_not_allowed_fallback(crate::http::middleware::not_found)
        .layer(from_fn_with_state(state.clone(), util::require_admin));
    Router::new().nest("/api/admin", group)
}

/// Authenticated route groups by mount prefix (Express `app.use(prefix, router)` order).
fn groups() -> Vec<(&'static str, Router<AppState>)> {
    vec![
        ("/api/accounts", accounts::router()),
        ("/api/block-list", block_list::router()),
        ("/api/contacts", contacts::router()),
        ("/api/todoist", todoist::router()),
        ("/api/search", search::router()),
        ("/api/mail", mail::router()),
        ("/api/integrations", integrations::router()),
        ("/api/totp", totp::router()),
        ("/api/rules", rules::router()),
        ("/api/plugins", plugins::router()),
        ("/api/calendar", calendar::router()),
        ("/api/dav/servers", dav_servers::router()),
        ("/api/dav-credentials", dav_credentials::router()),
        ("/api/ai", ai::router()),
        ("/api/diagnostics", diagnostics::router()),
    ]
}

pub fn router(state: &AppState) -> Router<AppState> {
    groups().into_iter().fold(
        Router::new()
            .nest("/api/auth", auth::router())
            .merge(admin_only(state))
            .nest("/api/auth/oidc", oidc::api_router())
            .nest("/auth/oidc", oidc::browser_router())
            .nest("/oauth", oauth::router())
            .merge(system::router())
            .merge(categories::router())
            .merge(push::router())
            .merge(calendar_feed::public_router())
            .merge(carddav::router())
            .merge(caldav::router())
            .merge(sender_favicons::router(state)),
        |r, (prefix, group)| r.merge(authed(state, prefix, group)),
    )
}
