//! Helpers shared by route handlers.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use inboxora_core::Error;
use serde_json::Value;

use crate::state::AppState;

/// Map an unexpected failure to the handler's own 500 message (`console.error` + JSON error),
/// keeping explicit HTTP errors as they are.
pub fn fail(context: &'static str, message: &'static str) -> impl Fn(Error) -> Error {
    move |e: Error| {
        if e.is_http() {
            return e;
        }
        tracing::error!("{context}: {e}");
        Error::status(500, message)
    }
}

pub use inboxora_core::error::Js;

/// `res.status(code).json(body)`.
pub fn json(status: u16, body: Value) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::OK), Js(body)).into_response()
}

/// `res.json(body)`.
pub fn ok(body: Value) -> Response {
    Js(body).into_response()
}

/// An error that renders `{ error: message }` with `status`.
pub fn err(status: u16, message: &str) -> Response {
    json(status, serde_json::json!({ "error": message }))
}

/// `sendImage(res, type, body)`: only safe raster types, sandboxed.
pub fn send_image(mime: Option<&str>, body: Vec<u8>, cache_control: Option<&'static str>) -> Response {
    let Some(t) = inboxora_mail::util::safe_image_type(mime) else { return empty(404) };
    let mut res = Response::new(axum::body::Body::from(body));
    let h = res.headers_mut();
    h.insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static(t));
    h.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    if let Some(cc) = cache_control {
        h.insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static(cc));
    }
    res
}

/// `res.status(code).end()` (no body, so Express sends no ETag).
pub fn empty(status: u16) -> Response {
    let mut r = StatusCode::from_u16(status).unwrap_or(StatusCode::OK).into_response();
    r.extensions_mut().insert(crate::http::middleware::NoEtag);
    r
}

/// `router.use(requireAuth)` for a mount prefix: every request under it (including paths no
/// handler matches) is authenticated first.
pub async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    match <crate::http::Auth as axum::extract::FromRequestParts<AppState>>::from_request_parts(&mut parts, &state).await
    {
        Ok(auth) => {
            // Handlers reuse this instead of re-checking the user row.
            parts.extensions.insert(auth);
            next.run(Request::from_parts(parts, body)).await
        }
        Err(e) => e.into_response(),
    }
}

/// `router.use(requireAdmin)` for a mount prefix.
pub async fn require_admin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    match <crate::http::Admin as axum::extract::FromRequestParts<AppState>>::from_request_parts(&mut parts, &state)
        .await
    {
        Ok(admin) => {
            parts.extensions.insert(admin);
            next.run(Request::from_parts(parts, body)).await
        }
        Err(e) => e.into_response(),
    }
}
