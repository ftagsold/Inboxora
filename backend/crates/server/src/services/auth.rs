//! Shared authentication infrastructure: rate limiting, auth settings, auth events, DAV
//! application passwords (ports of rateLimiter.ts, authLimiter.ts, authEvents.ts,
//! davAppPasswords.ts, davCredentials.ts).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use fred::prelude::*;
use inboxora_core::pg::{Db, Q};
use inboxora_core::{Error, Result, crypto, params};
use parking_lot::{Mutex, RwLock};
use regex::Regex;
use serde_json::{Map, Value};

// ── rate limiting (Redis `rl:<key>`, in-memory fallback) ─────────────────────────────

static MEMORY: LazyLock<Mutex<HashMap<String, (u64, Instant)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `consume(key, max, windowMs)` → (limited, reset ms).
pub async fn rate_consume(redis: &Pool, key: &str, max: u64, window: Duration) -> (bool, u64) {
    let rk = format!("rl:{key}");
    let window_ms = window.as_millis() as u64;
    let remote = async {
        let count: u64 = redis.incr(&rk).await?;
        if count == 1 {
            let _: bool = redis.pexpire(&rk, window_ms as i64, None).await?;
            return Ok::<_, fred::error::Error>((max < 1, window_ms));
        }
        let mut ttl: i64 = redis.pttl(&rk).await?;
        if ttl < 0 {
            let _: bool = redis.pexpire(&rk, window_ms as i64, None).await?;
            ttl = window_ms as i64;
        }
        Ok((count > max, ttl as u64))
    };
    match remote.await {
        Ok(r) => r,
        Err(_) => {
            let now = Instant::now();
            let mut m = MEMORY.lock();
            if m.len() > 50_000 {
                m.retain(|_, (_, reset)| *reset > now);
            }
            match m.get_mut(key) {
                Some((count, reset)) if now <= *reset => {
                    *count += 1;
                    (*count > max, (*reset - now).as_millis() as u64)
                }
                _ => {
                    m.insert(key.to_owned(), (1, now + window));
                    (max < 1, window_ms)
                }
            }
        }
    }
}

pub async fn rate_reset(redis: &Pool, key: &str) {
    let _: std::result::Result<i64, _> = redis.del(format!("rl:{key}")).await;
    MEMORY.lock().remove(key);
}

// ── auth settings ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct AuthLimits {
    pub max_requests: u64,
    pub window: Duration,
}

static LIMITS: LazyLock<RwLock<AuthLimits>> =
    LazyLock::new(|| RwLock::new(AuthLimits { max_requests: 10, window: Duration::from_secs(15 * 60) }));

pub fn auth_limits() -> AuthLimits {
    *LIMITS.read()
}

/// `reloadAuthSettings`.
pub async fn reload_auth_settings(db: &Db) {
    let rows = match db
        .rows(
            "SELECT key, value FROM system_settings WHERE key IN ('auth_max_attempts', 'auth_window_minutes')",
            params![],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("[auth] Failed to load rate limit settings: {e}");
            return;
        }
    };
    let mut l = LIMITS.write();
    for r in rows {
        let (k, v): (String, Option<String>) = (r.get(0), r.get(1));
        let n = v.as_deref().and_then(|v| v.trim().parse::<f64>().ok()).filter(|n| n.fract() == 0.0);
        match (k.as_str(), n) {
            ("auth_max_attempts", Some(n)) if (1.0..=100.0).contains(&n) => l.max_requests = n as u64,
            ("auth_window_minutes", Some(n)) if (1.0..=1440.0).contains(&n) => {
                l.window = Duration::from_secs(n as u64 * 60)
            }
            _ => {}
        }
    }
}

// ── auth events ──────────────────────────────────────────────────────────────────────

/// `logAuthEvent` (fire and forget).
pub fn log_auth_event(
    db: &Db,
    event_type: &str,
    username: Option<&str>,
    user_id: Option<&str>,
    ip: Option<&str>,
    success: bool,
) {
    let (db, t) = (db.clone(), event_type.to_owned());
    let (u, uid, ip) = (
        username.filter(|s| !s.is_empty()).map(str::to_owned),
        user_id.filter(|s| !s.is_empty()).map(str::to_owned),
        ip.filter(|s| !s.is_empty()).map(str::to_owned),
    );
    tokio::spawn(async move {
        if let Err(e) = db
            .exec(
                "INSERT INTO auth_events (event_type, username, user_id, ip, success) VALUES ($1, $2, $3, $4, $5)",
                params![t, u, uid, ip, success],
            )
            .await
        {
            tracing::error!("[auth] Failed to log event: {e}");
        }
    });
}

/// Hourly pruning of auth events older than 90 days.
pub fn start_auth_event_pruning(db: Db) {
    tokio::spawn(async move {
        let mut i = tokio::time::interval(Duration::from_secs(3600));
        i.tick().await;
        loop {
            i.tick().await;
            if let Err(e) =
                db.exec("DELETE FROM auth_events WHERE created_at < NOW() - INTERVAL '90 days'", params![]).await
            {
                tracing::error!("[auth] Failed to prune auth events: {e}");
            }
        }
    });
}

// ── bcrypt helpers ───────────────────────────────────────────────────────────────────

pub async fn bcrypt_hash(secret: String, cost: u32) -> Result<String> {
    tokio::task::spawn_blocking(move || bcrypt::hash(secret, cost))
        .await
        .map_err(|e| Error::internal(e.to_string()))?
        .map_err(|e| Error::internal(e.to_string()))
}

/// bcryptjs-compatible compare (bcryptjs truncates at 72 bytes; so does `bcrypt::verify`).
pub async fn bcrypt_verify(secret: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || bcrypt::verify(secret, &hash).unwrap_or(false)).await.unwrap_or(false)
}

// ── DAV application passwords ────────────────────────────────────────────────────────

static DAV_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(mf_dav_[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.([A-Za-z0-9_-]{16,})$")
        .expect("regex")
});
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| bcrypt::hash("mailflow-dav-timing-equalizer", 12).unwrap_or_default());

pub const LABEL_ERROR: &str = "A device label between 1 and 120 characters is required";

pub async fn create_app_password(
    db: &Db,
    user_id: &str,
    label: Option<&Value>,
) -> Result<(Map<String, Value>, String)> {
    let label = label.and_then(Value::as_str).map(str::trim).unwrap_or("");
    if label.is_empty() || inboxora_mail::text::js_len(label) > 120 {
        return Err(Error::status(400, LABEL_ERROR));
    }
    let prefix = format!("mf_dav_{}", crypto::random_uuid());
    let part = crypto::random_base64url(32);
    let secret = format!("{prefix}.{part}");
    let hash = bcrypt_hash(part, 12).await?;
    let row = db
        .json_opt(
            "INSERT INTO dav_app_passwords (user_id, label, token_prefix, secret_hash) VALUES ($1, $2, $3, $4) RETURNING id, label, created_at",
            params![user_id, label, &prefix, &hash],
        )
        .await?
        .unwrap_or_default();
    Ok((row, secret))
}

/// `authenticateDavCredential`: only dedicated app passwords authenticate DAV clients.
pub async fn authenticate_dav(db: &Db, username: &str, password: &str) -> Result<Option<(String, String)>> {
    if username.is_empty() {
        return Ok(None);
    }
    let user = db.opt("SELECT id FROM users WHERE username = $1", params![username]).await?;
    let Some(user) = user else {
        // Equalise timing with the bcrypt compare of a real account.
        let _ = bcrypt_verify(password.to_owned(), DUMMY_HASH.clone()).await;
        return Ok(None);
    };
    let user_id = user.get::<_, uuid::Uuid>(0).to_string();
    let Some(c) = DAV_PREFIX.captures(password) else { return Ok(None) };
    let row = db
        .opt("SELECT id, secret_hash FROM dav_app_passwords WHERE user_id = $1 AND token_prefix = $2 AND revoked_at IS NULL", params![&user_id, &c[1]])
        .await?;
    let Some(row) = row else { return Ok(None) };
    let (id, hash): (uuid::Uuid, String) = (row.get(0), row.get(1));
    if !bcrypt_verify(c[2].to_owned(), hash).await {
        return Ok(None);
    }
    db.exec("UPDATE dav_app_passwords SET last_used_at = NOW() WHERE id = $1", params![id.to_string()]).await?;
    Ok(Some((user_id, id.to_string())))
}

/// `ensureUserDavResources`.
pub async fn ensure_user_dav_resources(q: &impl Q, user_id: &str) -> Result<()> {
    q.exec("INSERT INTO address_books (user_id, name, source) VALUES ($1, 'Personal', 'local') ON CONFLICT (user_id, name) DO NOTHING", params![user_id]).await?;
    q.exec(
        "INSERT INTO calendars (user_id, owner_user_id, name, source, read_only) VALUES ($1, $1, 'Personal', 'local', false) ON CONFLICT (user_id, name) DO NOTHING",
        params![user_id],
    )
    .await?;
    Ok(())
}
