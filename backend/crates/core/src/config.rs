//! Environment configuration, with the same variable names and defaults as the Node backend.
//!
//! Some settings (OAuth integration client ids/secrets) are stored in the database and were
//! copied into `process.env` at runtime by the Node backend; [`env`] therefore consults a
//! runtime overlay before the process environment.

use std::collections::HashMap;
use std::sync::LazyLock;

use parking_lot::RwLock;

static OVERLAY: LazyLock<RwLock<HashMap<String, Option<String>>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Read a setting: runtime overlay first, then the process environment. Empty values count
/// as unset, matching the `process.env.X || default` idiom used throughout the Node code.
pub fn env(name: &str) -> Option<String> {
    if let Some(v) = OVERLAY.read().get(name) {
        return v.clone().filter(|s| !s.is_empty());
    }
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// Set (or with `None`, unset) a runtime overlay value.
pub fn set_env(name: &str, value: Option<String>) {
    OVERLAY.write().insert(name.to_owned(), value);
}

pub fn env_or(name: &str, default: &str) -> String {
    env(name).unwrap_or_else(|| default.to_owned())
}

pub fn env_flag(name: &str) -> bool {
    env(name).is_some_and(|v| v == "true")
}

/// True when NODE_ENV=production (kept for operator compatibility).
pub fn is_production() -> bool {
    env("NODE_ENV").as_deref() == Some("production")
}

#[derive(Debug, Clone)]
pub struct DbConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub pool_max: usize,
    pub statement_timeout_ms: u64,
}

fn port_of(v: Option<String>) -> u16 {
    v.and_then(|p| p.trim().parse::<u16>().ok()).filter(|p| *p != 0).unwrap_or(5432)
}

/// True when DB_HOST names a Unix socket (its directory or the socket file).
pub fn is_database_socket(host: Option<&str>) -> bool {
    host.is_some_and(|h| h.starts_with('/'))
}

impl DbConfig {
    /// DB_HOST may be a TCP host, a socket directory, or the socket file itself
    /// (`/run/postgresql/.s.PGSQL.5432`), which is split back into directory and port.
    pub fn from_env() -> Self {
        let mut host = env_or("DB_HOST", "postgres");
        let mut port = port_of(env("DB_PORT"));
        if host.starts_with('/')
            && let Some(idx) = host.rfind("/.s.PGSQL.")
        {
            {
                let p = host[idx + "/.s.PGSQL.".len()..].to_owned();
                if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) {
                    port = port_of(Some(p));
                    host = host[..idx].to_owned();
                    if host.is_empty() {
                        host = "/".into();
                    }
                }
            }
        }
        Self {
            host,
            port,
            database: env_or("DB_NAME", "mailflow"),
            user: env_or("DB_USER", "mailflow"),
            password: env("DB_PASSWORD"),
            pool_max: env("DB_POOL_MAX").and_then(|v| v.parse().ok()).unwrap_or(20),
            statement_timeout_ms: 30_000,
        }
    }
}

/// Process-wide settings validated at startup.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub port: u16,
    pub session_secret: String,
    pub encryption_key_hex: String,
    pub app_url: Option<String>,
    pub frontend_url: String,
    pub redis_url: String,
    pub app_version: String,
    pub build_sha: String,
    pub db: DbConfig,
}

impl AppConfig {
    /// Load and validate. Returns the same fatal messages the Node backend printed.
    pub fn from_env(package_version: &str) -> Result<Self, String> {
        let session_secret = env("SESSION_SECRET").unwrap_or_default();
        if session_secret.len() < 32 {
            return Err("FATAL: SESSION_SECRET must be set and at least 32 characters. Exiting.".into());
        }
        let db = DbConfig::from_env();
        if db.password.is_none() && !is_database_socket(env("DB_HOST").as_deref()) {
            return Err("FATAL: DB_PASSWORD must be set (unless DB_HOST is a Unix socket path). Exiting.".into());
        }
        let encryption_key_hex = env("ENCRYPTION_KEY").unwrap_or_default();
        if encryption_key_hex.len() != 64 || hex::decode(&encryption_key_hex).is_err() {
            return Err("FATAL: ENCRYPTION_KEY must be set and exactly 64 hex characters (32 bytes). Generate one with: openssl rand -hex 32".into());
        }
        let app_url = env("APP_URL");
        if is_production() && app_url.is_none() {
            return Err("FATAL: APP_URL must be set in production (e.g. https://mail.example.com). WebSocket connections and OIDC depend on it.".into());
        }
        let raw_version = env("APP_VERSION").unwrap_or_else(|| package_version.to_owned());
        let app_version =
            raw_version.strip_prefix("v.").or_else(|| raw_version.strip_prefix('v')).unwrap_or(&raw_version).to_owned();
        Ok(Self {
            port: env("PORT").and_then(|p| p.parse().ok()).unwrap_or(3000),
            session_secret,
            encryption_key_hex,
            frontend_url: env("FRONTEND_URL").unwrap_or_else(|| "http://localhost:5173".into()),
            app_url,
            redis_url: env("REDIS_URL").unwrap_or_else(|| "redis://redis:6379".into()),
            app_version,
            build_sha: env("BUILD_SHA").unwrap_or_else(|| "dev".into()),
            db,
        })
    }

    /// Origin of APP_URL (scheme://host[:port]), if it parses.
    pub fn app_origin(&self) -> Option<String> {
        let u = url::Url::parse(self.app_url.as_deref()?).ok()?;
        Some(u.origin().ascii_serialization())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_file_is_split_into_dir_and_port() {
        set_env("DB_HOST", Some("/run/postgresql/.s.PGSQL.6543".into()));
        set_env("DB_PORT", None);
        let c = DbConfig::from_env();
        assert_eq!((c.host.as_str(), c.port), ("/run/postgresql", 6543));
        set_env("DB_HOST", None);
    }
}
