//! `express.json()` equivalent: parse JSON bodies with a per-path size limit.

use axum::extract::{FromRequest, Request};
use axum::http::header;
use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use inboxora_core::Error;
use serde_json::{Map, Value};

/// 25 MB attachments ≈ 34 MB base64, plus headroom.
const LARGE_LIMIT: usize = 35 * 1024 * 1024;
const DEFAULT_LIMIT: usize = 1024 * 1024;

fn limit_for(path: &str) -> usize {
    let under = |base: &str| path == base || path.starts_with(&format!("{base}/"));
    if under("/api/mail/send") || under("/api/mail/draft") { LARGE_LIMIT } else { DEFAULT_LIMIT }
}

fn is_json(req: &Request) -> bool {
    req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).and_then(|v| v.split(';').next()).is_some_and(
        |t| {
            let t = t.trim().to_ascii_lowercase();
            t == "application/json" || (t.starts_with("application/") && t.ends_with("+json"))
        },
    )
}

fn too_large() -> Error {
    Error::status(413, "Request too large. Total attachment size must not exceed 25 MB.")
}

/// Read the raw body with a limit.
pub async fn read_limited(req: Request, limit: usize) -> Result<Bytes, Error> {
    let body = Limited::new(req.into_body(), limit);
    match body.collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(e) => {
            if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                Err(too_large())
            } else {
                Err(Error::status(400, "Request body could not be read"))
            }
        }
    }
}

/// The parsed JSON body (`req.body`): `{}` when the request is not JSON or empty.
#[derive(Debug, Clone)]
pub struct Body(pub Value);

impl Body {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    pub fn truthy(&self, key: &str) -> bool {
        inboxora_core::json::truthy(self.0.get(key))
    }

    pub fn has(&self, key: &str) -> bool {
        self.0.as_object().is_some_and(|m| m.contains_key(key))
    }
}

impl<S: Send + Sync> FromRequest<S> for Body {
    type Rejection = Error;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        if !is_json(&req) {
            return Ok(Body(Value::Object(Map::new())));
        }
        let limit = limit_for(req.uri().path());
        let declared = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<usize>().ok());
        if declared.is_some_and(|len| len > limit) {
            return Err(too_large());
        }
        let bytes = read_limited(req, limit).await?;
        let first = bytes.iter().find(|b| !b.is_ascii_whitespace());
        match first {
            None => Ok(Body(Value::Object(Map::new()))),
            // body-parser strict mode accepts only objects and arrays.
            Some(b'{' | b'[') => {
                serde_json::from_slice(&bytes).map(Body).map_err(|_| Error::status(400, "Invalid JSON body"))
            }
            Some(_) => Err(Error::status(400, "Invalid JSON body")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_and_draft_get_the_large_limit() {
        assert_eq!(limit_for("/api/mail/send"), LARGE_LIMIT);
        assert_eq!(limit_for("/api/mail/draft/abc"), LARGE_LIMIT);
        assert_eq!(limit_for("/api/mail/sender"), DEFAULT_LIMIT);
        assert_eq!(limit_for("/api/mail/messages"), DEFAULT_LIMIT);
    }
}
