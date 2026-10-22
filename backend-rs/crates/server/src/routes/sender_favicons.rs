//! `/api/sender-favicons/:domain` (port of `routes/senderFavicons.ts`): a cached 64 px sender
//! icon, never cached by the browser, honouring the user's `senderFavicons` preference.

use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::Response;
use axum::routing::get;
use inboxora_core::pg::Q;
use inboxora_core::{Result, params};
use serde_json::Value;

use crate::http::Auth;
use crate::routes::util::empty;
use crate::services::sender_favicon::{self, Favicon};
use crate::state::AppState;

/// The whole prefix, including 401s and unknown paths, answers `private, no-store`.
pub fn router(state: &AppState) -> Router<AppState> {
    let group = Router::new()
        .route("/{domain}", get(favicon))
        .fallback(crate::http::middleware::not_found)
        .method_not_allowed_fallback(crate::http::middleware::not_found)
        .layer(from_fn_with_state(state.clone(), super::util::require_auth));
    // `nest` does not match the bare prefix with a trailing slash; Express still runs the
    // router's middleware (no-store, requireAuth) before its 404 there.
    let bare = axum::routing::any(crate::http::middleware::not_found)
        .layer(from_fn_with_state(state.clone(), super::util::require_auth));
    Router::new().nest("/api/sender-favicons", group).route("/api/sender-favicons/", bare).layer(from_fn(no_store))
}

async fn no_store(req: axum::extract::Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    res
}

async fn favicon(State(s): State<AppState>, a: Auth, Path(domain): Path<String>) -> Result<Response> {
    let prefs =
        s.db.opt("SELECT preferences FROM users WHERE id = $1", params![&a.user_id])
            .await?
            .and_then(|r| r.get::<_, Option<Value>>("preferences"));
    if prefs.as_ref().and_then(|p| p.get("senderFavicons")) == Some(&Value::Bool(false)) {
        return Ok(empty(404));
    }
    if sender_favicon::normalize_domain(&domain).is_none() {
        return Ok(empty(400));
    }
    let (limited, reset_ms) = crate::services::auth::rate_consume(
        &s.redis,
        &format!("sender-favicon:{}", a.user_id),
        300,
        Duration::from_secs(60),
    )
    .await;
    if limited {
        let mut res = empty(429);
        let secs = reset_ms.div_ceil(1000).max(1);
        res.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
        return Ok(res);
    }
    let Favicon::Image(bytes) = sender_favicon::get(&s.redis, &domain).await else { return Ok(empty(404)) };
    let len = bytes.len();
    let mut res = Response::new(axum::body::Body::from(bytes));
    *res.status_mut() = StatusCode::OK;
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    res.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    Ok(res)
}
