//! Request extractors: session, authentication, client info.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use inboxora_core::session::Session;
use inboxora_core::{Error, params, pg::Q};

use crate::state::AppState;

/// The request's session (`req.session`).
#[derive(Clone)]
pub struct Sess(pub Session);

impl<S: Send + Sync> FromRequestParts<S> for Sess {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Session>()
            .cloned()
            .map(Sess)
            .ok_or_else(|| Error::internal("session middleware missing"))
    }
}

/// `requireAuth`: a session user that still exists. The user row is checked on every request,
/// so a deleted user cannot keep using an old session.
#[derive(Clone)]
pub struct Auth {
    pub user_id: String,
    pub session: Session,
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let Sess(session) = Sess::from_request_parts(parts, state).await?;
        let Some(user_id) = session.user_id() else {
            return Err(Error::unauthorized());
        };
        let exists = state.db.opt("SELECT id FROM users WHERE id = $1", params![&user_id]).await?;
        if exists.is_none() {
            if let Err(e) = state.sessions.destroy(&session).await {
                tracing::warn!("session destroy failed: {e}");
            }
            return Err(Error::unauthorized());
        }
        Ok(Auth { user_id, session })
    }
}

/// `requireAdmin`: re-checks `is_admin` on every request so a revoked admin loses access.
#[derive(Clone)]
pub struct Admin {
    pub user_id: String,
    pub session: Session,
}

impl FromRequestParts<AppState> for Admin {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        // Already checked by the prefix gate (`require_admin`).
        if let Some(admin) = parts.extensions.get::<Admin>() {
            return Ok(admin.clone());
        }
        let Sess(session) = Sess::from_request_parts(parts, state).await?;
        let Some(user_id) = session.user_id() else {
            return Err(Error::unauthorized());
        };
        let row = state.db.opt("SELECT is_admin FROM users WHERE id = $1", params![&user_id]).await?;
        let is_admin = row.is_some_and(|r| r.get::<_, Option<bool>>(0).unwrap_or(false));
        if !is_admin {
            return Err(Error::status(403, "Admin access required"));
        }
        Ok(Admin { user_id, session })
    }
}

/// `req.ip` with `trust proxy = 1`: the last X-Forwarded-For entry, else the socket peer.
#[derive(Clone, Debug)]
pub struct ClientIp(pub String);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let forwarded = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let ip = forwarded.or_else(|| {
            parts.extensions.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| match c.0.ip() {
                std::net::IpAddr::V4(v4) => format!("::ffff:{v4}"),
                std::net::IpAddr::V6(v6) => v6.to_string(),
            })
        });
        Ok(ClientIp(ip.unwrap_or_default()))
    }
}

/// Request facts used to build absolute URLs (`req.protocol`, `req.get('host')`, `req.secure`).
#[derive(Clone, Debug)]
pub struct ReqInfo {
    pub secure: bool,
    pub protocol: &'static str,
    pub host: String,
    pub hostname: String,
    pub user_agent: Option<String>,
    pub original_url: String,
}

impl<S: Send + Sync> FromRequestParts<S> for ReqInfo {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let secure = crate::http::middleware::request_is_secure(&parts.headers);
        let header = |n: &str| parts.headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let host = header("host").unwrap_or_default();
        // Express 4 `req.hostname` trusts X-Forwarded-Host behind a trusted proxy.
        let hostname = header("x-forwarded-host")
            .and_then(|h| h.split(',').next().map(|s| s.trim().to_owned()))
            .unwrap_or_else(|| host.clone());
        let hostname = if hostname.starts_with('[') {
            hostname.split(']').next().map(|s| format!("{s}]")).unwrap_or(hostname)
        } else {
            hostname.split(':').next().unwrap_or("").to_owned()
        };
        Ok(ReqInfo {
            secure,
            protocol: if secure { "https" } else { "http" },
            host,
            hostname,
            user_agent: header("user-agent"),
            original_url: parts.uri.path_and_query().map(|p| p.as_str().to_owned()).unwrap_or_default(),
        })
    }
}
