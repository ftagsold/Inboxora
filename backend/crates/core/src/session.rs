//! Sessions compatible with `express-session` 1.19 + `connect-redis` 7.
//!
//! Wire and storage format (both backends must be able to read each other's sessions):
//! - cookie `connect.sid=` + encodeURIComponent(`s:<id>.<sig>`), where `sig` is base64
//!   HMAC-SHA256(SESSION_SECRET, id) with trailing `=` removed;
//! - ids are 24 random bytes in unpadded base64url;
//! - Redis key `sess:<id>` holds `JSON.stringify(session)` with `cookie` first, TTL derived from
//!   `cookie.expires`.
//!
//! Save/touch/Set-Cookie follow express-session with `resave: false`,
//! `saveUninitialized: false`, `rolling: false` and `cookie.maxAge` = 7 days:
//! the session is written only when its data (everything except `cookie`) changed, otherwise
//! the Redis TTL is refreshed; `Set-Cookie` is sent only when the data changed.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, Utc};
use fred::prelude::*;
use hmac::{KeyInit, Mac};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::json::js_iso;

pub const COOKIE_NAME: &str = "connect.sid";
const KEY_PREFIX: &str = "sess:";
pub const MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Session storage backed by Redis.
#[derive(Clone)]
pub struct SessionStore {
    redis: Pool,
    secret: Arc<str>,
}

/// The mutable session state for one request.
#[derive(Debug)]
pub struct SessionState {
    pub id: String,
    /// The id the request's cookie carried (None for a request without a valid session).
    cookie_id: Option<String>,
    original_id: String,
    original_hash: String,
    saved_hash: Option<String>,
    data: Map<String, Value>,
    /// Cookie fields other than expires/secure, kept as stored (`path`, `httpOnly`, …).
    original_max_age: i64,
    expires: DateTime<Utc>,
    secure: bool,
    destroyed: bool,
}

/// Request-scoped handle shared between the middleware and handlers.
#[derive(Clone, Debug)]
pub struct Session(Arc<Mutex<SessionState>>);

fn hash_data(data: &Map<String, Value>) -> String {
    use sha1::Digest;
    hex::encode(sha1::Sha1::digest(serde_json::to_string(data).unwrap_or_default().as_bytes()))
}

fn sign(val: &str, secret: &str) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key");
    mac.update(val.as_bytes());
    let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    format!("{val}.{}", sig.trim_end_matches('='))
}

/// `cookie-signature.unsign`.
fn unsign(signed: &str, secret: &str) -> Option<String> {
    let idx = signed.rfind('.')?;
    let val = &signed[..idx];
    let expected = sign(val, secret);
    crate::crypto::ct_eq(expected.as_bytes(), signed.as_bytes()).then(|| val.to_owned())
}

/// Read `connect.sid` from a Cookie header (first occurrence wins, value percent-decoded).
fn cookie_value(header: &str) -> Option<String> {
    for part in header.split(';') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k.trim() != COOKIE_NAME {
            continue;
        }
        let mut v = v.trim();
        if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
            v = &v[1..v.len() - 1];
        }
        return Some(
            percent_encoding::percent_decode_str(v).decode_utf8().map_or_else(|_| v.to_owned(), |c| c.into_owned()),
        );
    }
    None
}

fn new_id() -> String {
    crate::crypto::random_base64url(24)
}

/// `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    percent_encoding::utf8_percent_encode(s, SET).to_string()
}

impl SessionStore {
    pub fn new(redis: Pool, secret: &str) -> Self {
        Self { redis, secret: secret.into() }
    }

    pub fn redis(&self) -> &Pool {
        &self.redis
    }

    /// The session id carried by a valid signed cookie.
    pub fn id_from_cookie_header(&self, header: Option<&str>) -> Option<String> {
        let raw = cookie_value(header?)?;
        unsign(raw.strip_prefix("s:")?, &self.secret)
    }

    /// Load (or start) the session for a request. A store error is returned so the caller
    /// can fail the request (or close a WebSocket with 1011) rather than treat it as logged out.
    pub async fn load(&self, cookie_header: Option<&str>, secure: bool) -> Result<Session, Error> {
        let now = Utc::now();
        if let Some(id) = self.id_from_cookie_header(cookie_header) {
            let raw: Option<String> = self.redis.get(format!("{KEY_PREFIX}{id}")).await?;
            if let Some(mut stored) = raw.and_then(|r| serde_json::from_str::<Map<String, Value>>(&r).ok()) {
                let cookie = stored.shift_remove("cookie").unwrap_or(Value::Null);
                let original_max_age = cookie.get("originalMaxAge").and_then(Value::as_i64).unwrap_or(MAX_AGE_MS);
                let hash = hash_data(&stored);
                return Ok(Session(Arc::new(Mutex::new(SessionState {
                    cookie_id: Some(id.clone()),
                    original_id: id.clone(),
                    id,
                    original_hash: hash.clone(),
                    saved_hash: Some(hash),
                    data: stored,
                    original_max_age,
                    // express-session touches every request: expires = now + originalMaxAge.
                    expires: now + chrono::Duration::milliseconds(original_max_age),
                    secure,
                    destroyed: false,
                }))));
            }
            // A signed cookie whose session expired: start fresh, keeping the cookie id
            // reference so a new session is only issued if data is assigned.
            return Ok(Session::fresh(Some(id), secure));
        }
        Ok(Session::fresh(None, secure))
    }

    fn serialize(state: &SessionState) -> String {
        let mut m = Map::new();
        let mut cookie = Map::new();
        cookie.insert("originalMaxAge".into(), Value::from(state.original_max_age));
        cookie.insert("expires".into(), Value::String(js_iso(&state.expires)));
        cookie.insert("secure".into(), Value::Bool(state.secure));
        cookie.insert("httpOnly".into(), Value::Bool(true));
        cookie.insert("path".into(), Value::String("/".into()));
        cookie.insert("sameSite".into(), Value::String("lax".into()));
        m.insert("cookie".into(), Value::Object(cookie));
        for (k, v) in &state.data {
            m.insert(k.clone(), v.clone());
        }
        serde_json::to_string(&m).unwrap_or_default()
    }

    fn ttl_secs(state: &SessionState) -> i64 {
        let ms = (state.expires - Utc::now()).num_milliseconds();
        (ms as f64 / 1000.0).ceil() as i64
    }

    async fn write(&self, state: &SessionState) -> Result<(), Error> {
        let key = format!("{KEY_PREFIX}{}", state.id);
        let ttl = Self::ttl_secs(state);
        if ttl > 0 {
            let _: () = self.redis.set(key, Self::serialize(state), Some(Expiration::EX(ttl)), None, false).await?;
        } else {
            let _: () = self.redis.del(key).await?;
        }
        Ok(())
    }

    async fn destroy_id(&self, id: &str) -> Result<(), Error> {
        let _: i64 = self.redis.del(format!("{KEY_PREFIX}{id}")).await?;
        Ok(())
    }

    /// Delete every session belonging to a user (used after password changes and deletions).
    /// Mirrors the Node helpers that scanned `sess:*`.
    pub async fn destroy_user_sessions(&self, user_id: &str, except: Option<&str>) -> Result<usize, Error> {
        let mut removed = 0;
        let client = self.redis.next();
        let mut cursor = "0".to_owned();
        loop {
            let (next, keys): (String, Vec<String>) = client
                .custom(
                    fred::cmd!("SCAN"),
                    vec![cursor.clone(), "MATCH".into(), format!("{KEY_PREFIX}*"), "COUNT".into(), "100".into()],
                )
                .await?;
            for key in keys {
                if except.is_some_and(|id| key == format!("{KEY_PREFIX}{id}")) {
                    continue;
                }
                let raw: Option<String> = client.get(&key).await?;
                let owner = raw
                    .as_deref()
                    .and_then(|r| serde_json::from_str::<Value>(r).ok())
                    .and_then(|v| v.get("userId").and_then(Value::as_str).map(str::to_owned));
                if owner.as_deref() == Some(user_id) {
                    let _: i64 = client.del(&key).await?;
                    removed += 1;
                }
            }
            if next == "0" {
                break;
            }
            cursor = next;
        }
        Ok(removed)
    }

    /// Finish the request: persist or touch, and return the Set-Cookie value if one is due.
    pub async fn commit(&self, session: &Session) -> Result<Option<String>, Error> {
        let (state_id, should_save, should_touch, set_cookie, snapshot) = {
            let st = session.0.lock();
            if st.destroyed {
                return Ok(None);
            }
            let hash = hash_data(&st.data);
            let modified = st.original_id != st.id || st.original_hash != hash;
            let saved = st.original_id == st.id && st.saved_hash.as_deref() == Some(hash.as_str());
            let cookie_matches = st.cookie_id.as_deref() == Some(st.id.as_str());
            // shouldSave: an uninitialized session is saved only once modified.
            let should_save = if st.saved_hash.is_none() && !cookie_matches { modified } else { !saved };
            let should_touch = cookie_matches && !should_save;
            // shouldSetCookie: new id → only when modified; same id → when modified (expires set).
            let set_cookie = modified;
            let snapshot = SessionState {
                id: st.id.clone(),
                cookie_id: st.cookie_id.clone(),
                original_id: st.original_id.clone(),
                original_hash: st.original_hash.clone(),
                saved_hash: st.saved_hash.clone(),
                data: st.data.clone(),
                original_max_age: st.original_max_age,
                expires: st.expires,
                secure: st.secure,
                destroyed: false,
            };
            (st.id.clone(), should_save, should_touch, set_cookie, snapshot)
        };
        if should_save {
            self.write(&snapshot).await?;
        } else if should_touch {
            let ttl = Self::ttl_secs(&snapshot);
            let _: bool = self.redis.expire(format!("{KEY_PREFIX}{state_id}"), ttl, None).await?;
        }
        // express-session refuses to set a Secure cookie on an insecure request; our cookie is
        // only Secure when the request was secure, so the cookie is always settable.
        Ok(set_cookie.then(|| self.set_cookie_header(&state_id, &snapshot.expires, snapshot.secure)))
    }

    /// The `Set-Cookie` value for a session id.
    pub fn set_cookie_header(&self, id: &str, expires: &DateTime<Utc>, secure: bool) -> String {
        let signed = format!("s:{}", sign(id, &self.secret));
        let mut s = format!(
            "{COOKIE_NAME}={}; Path=/; Expires={}; HttpOnly",
            encode_uri_component(&signed),
            expires.format("%a, %d %b %Y %H:%M:%S GMT")
        );
        if secure {
            s.push_str("; Secure");
        }
        s.push_str("; SameSite=Lax");
        s
    }

    /// `res.clearCookie('connect.sid')` as Express emits it.
    pub fn clear_cookie_header() -> String {
        format!("{COOKIE_NAME}=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT")
    }

    /// Immediately persist the session (`req.session.save()`), e.g. before a redirect.
    pub async fn save(&self, session: &Session) -> Result<(), Error> {
        let snapshot = {
            let mut st = session.0.lock();
            if st.destroyed {
                return Ok(());
            }
            st.saved_hash = Some(hash_data(&st.data));
            SessionState {
                id: st.id.clone(),
                cookie_id: st.cookie_id.clone(),
                original_id: st.original_id.clone(),
                original_hash: st.original_hash.clone(),
                saved_hash: st.saved_hash.clone(),
                data: st.data.clone(),
                original_max_age: st.original_max_age,
                expires: st.expires,
                secure: st.secure,
                destroyed: false,
            }
        };
        self.write(&snapshot).await
    }

    /// `req.session.regenerate()`: destroy the stored session and continue with a new, empty
    /// one (fixation defense on login).
    pub async fn regenerate(&self, session: &Session) -> Result<(), Error> {
        let old = {
            let mut st = session.0.lock();
            let old = st.id.clone();
            let secure = st.secure;
            *st = SessionState::empty(st.cookie_id.clone(), secure);
            old
        };
        self.destroy_id(&old).await
    }

    /// `req.session.destroy()`.
    pub async fn destroy(&self, session: &Session) -> Result<(), Error> {
        let id = {
            let mut st = session.0.lock();
            st.destroyed = true;
            st.id.clone()
        };
        self.destroy_id(&id).await
    }
}

impl SessionState {
    fn empty(cookie_id: Option<String>, secure: bool) -> Self {
        let id = new_id();
        let data = Map::new();
        let hash = hash_data(&data);
        Self {
            original_id: id.clone(),
            id,
            cookie_id,
            original_hash: hash,
            saved_hash: None,
            data,
            original_max_age: MAX_AGE_MS,
            expires: Utc::now() + chrono::Duration::milliseconds(MAX_AGE_MS),
            secure,
            destroyed: false,
        }
    }
}

impl Session {
    fn fresh(cookie_id: Option<String>, secure: bool) -> Self {
        Session(Arc::new(Mutex::new(SessionState::empty(cookie_id, secure))))
    }

    /// A detached session for tests and internal callers.
    pub fn detached(data: Map<String, Value>) -> Self {
        let s = Self::fresh(None, false);
        s.0.lock().data = data;
        s
    }

    pub fn id(&self) -> String {
        self.0.lock().id.clone()
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.0.lock().data.get(key).cloned()
    }

    pub fn get_str(&self, key: &str) -> Option<String> {
        self.0.lock().data.get(key).and_then(Value::as_str).map(str::to_owned)
    }

    pub fn get_bool(&self, key: &str) -> bool {
        crate::json::truthy(self.0.lock().data.get(key))
    }

    pub fn set(&self, key: &str, value: impl Into<Value>) {
        self.0.lock().data.insert(key.to_owned(), value.into());
    }

    /// `delete req.session.key`.
    pub fn remove(&self, key: &str) {
        self.0.lock().data.shift_remove(key);
    }

    /// The authenticated user id (`req.session.userId`).
    pub fn user_id(&self) -> Option<String> {
        self.get_str("userId").filter(|s| !s.is_empty())
    }

    pub fn is_locked(&self) -> bool {
        self.get_bool("locked")
    }

    pub fn is_destroyed(&self) -> bool {
        self.0.lock().destroyed
    }

    /// The whole data map (for diagnostics/tests).
    pub fn data(&self) -> Map<String, Value> {
        self.0.lock().data.clone()
    }

    /// Remaining lifetime of the cookie.
    pub fn max_age(&self) -> Duration {
        let st = self.0.lock();
        (st.expires - Utc::now()).to_std().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_cookie_signature() {
        // From cookie-signature.sign('abc', 'x'.repeat(32)).
        assert_eq!(sign("abc", &"x".repeat(32)), "abc.Qk8iIq+eUdx4hAKV3yBgWYmVuKjzpO5XMbW+KynWq8c");
        assert_eq!(unsign("abc.Qk8iIq+eUdx4hAKV3yBgWYmVuKjzpO5XMbW+KynWq8c", &"x".repeat(32)).as_deref(), Some("abc"));
        assert_eq!(unsign("abc.Qk8iIq+eUdx4hAKV3yBgWYmVuKjzpO5XMbW+KynWq8d", &"x".repeat(32)), None);
        assert_eq!(
            encode_uri_component("s:abc.Qk8iIq+eUdx4hAKV3yBgWYmVuKjzpO5XMbW+KynWq8c"),
            "s%3Aabc.Qk8iIq%2BeUdx4hAKV3yBgWYmVuKjzpO5XMbW%2BKynWq8c"
        );
    }

    #[test]
    fn reads_first_cookie_and_decodes() {
        let h = "a=1; connect.sid=s%3Aabc.def; connect.sid=other";
        assert_eq!(cookie_value(h).as_deref(), Some("s:abc.def"));
    }

    #[test]
    fn ids_are_32_char_base64url() {
        let id = new_id();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }
}
