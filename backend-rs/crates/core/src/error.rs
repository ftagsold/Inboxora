//! Application error type and its HTTP mapping.
//!
//! Expected failures carry an explicit status and JSON body (`{"error": "..."}` like the Node
//! handlers); anything else is logged and answered with a generic 500.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http {status}: {body}")]
    Http { status: StatusCode, body: Value },
    #[error("database: {}", display_chain(.0))]
    Db(#[from] tokio_postgres::Error),
    #[error("database pool: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),
    #[error("redis: {0}")]
    Redis(#[from] fred::error::Error),
    #[error("{0}")]
    Internal(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// An error with its source chain (tokio-postgres hides the cause of bind/serialize errors).
fn display_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

impl Error {
    /// `res.status(status).json({ error: message })`.
    pub fn status(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, json!({ "error": message.into() }))
    }

    /// `res.status(status).json(body)`.
    pub fn json(status: u16, body: Value) -> Self {
        Self::Http { status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), body }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::status(400, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::status(404, message)
    }

    pub fn unauthorized() -> Self {
        Self::status(401, "Not authenticated")
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::status(403, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// PostgreSQL SQLSTATE of a database error.
    pub fn pg_code(&self) -> Option<&str> {
        match self {
            Error::Db(e) => e.code().map(|c| c.code()),
            _ => None,
        }
    }

    /// The database error's constraint name, when a constraint was violated.
    pub fn pg_constraint(&self) -> Option<&str> {
        match self {
            Error::Db(e) => e.as_db_error().and_then(|d| d.constraint()),
            _ => None,
        }
    }

    /// True for a unique-violation (23505).
    pub fn is_unique_violation(&self) -> bool {
        self.pg_code() == Some("23505")
    }

    pub fn is_http(&self) -> bool {
        matches!(self, Error::Http { .. })
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Error::Http { status, body } => (status, axum::Json(body)).into_response(),
            other => {
                tracing::error!("Unhandled route error: {other}");
                (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({ "error": "Internal server error" })))
                    .into_response()
            }
        }
    }
}

/// Shorthand for early-returning an HTTP error: `bail!(400, "Missing field")`.
#[macro_export]
macro_rules! bail {
    ($status:expr, $msg:expr) => {
        return Err($crate::error::Error::status($status, $msg))
    };
}
