//! Accounts, connection setup (token refresh, pinned DNS, TLS policy, per-host admission
//! control, IPv4 fallback) and the keyed semaphore.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use inboxora_core::pg::{Db, Q};
use inboxora_core::{crypto, params};
use inboxora_imap::{Auth, Client, Config};
use parking_lot::Mutex;
use serde_json::{Map, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::policy::{self, ProviderProfile};
use crate::services::dav_client::connection_policy;
use crate::services::oauth::RefreshError;

/// An `email_accounts` row (node-pg JSON), as the engine passes it around.
#[derive(Debug, Clone)]
pub struct Account(pub Arc<Map<String, Value>>);

impl Account {
    pub fn from_map(m: Map<String, Value>) -> Self {
        Self(Arc::new(m))
    }
    pub fn str(&self, k: &str) -> Option<&str> {
        self.0.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
    }
    pub fn bool(&self, k: &str) -> Option<bool> {
        self.0.get(k).and_then(Value::as_bool)
    }
    pub fn id(&self) -> &str {
        self.str("id").unwrap_or_default()
    }
    pub fn user_id(&self) -> &str {
        self.str("user_id").unwrap_or_default()
    }
    pub fn host(&self) -> String {
        self.str("imap_host").unwrap_or("").to_lowercase()
    }
    pub fn profile(&self) -> &'static ProviderProfile {
        policy::provider_profile(self.str("imap_host"), self.str("oauth_provider"))
    }
    pub fn enabled(&self) -> bool {
        self.bool("enabled").unwrap_or(true)
    }
    /// `folder_mappings.<role>` (a configured path).
    pub fn mapping(&self, role: &str) -> Option<&str> {
        self.0.get("folder_mappings").and_then(|m| m.get(role)).and_then(Value::as_str).filter(|s| !s.is_empty())
    }
    /// Redacted address for logs (`redactEmail`: keep the domain, mask the local part).
    pub fn log(&self) -> String {
        redact_email(self.str("email_address").unwrap_or(""))
    }
    pub fn map(&self) -> &Map<String, Value> {
        &self.0
    }
}

pub fn redact_email(email: &str) -> String {
    match email.find('@') {
        Some(at) => {
            let first: String = email.chars().take(1).collect();
            format!("{first}***{}", &email[at..])
        }
        None => "[email]".into(),
    }
}

pub async fn load_account(db: &Db, id: &str) -> inboxora_core::Result<Option<Account>> {
    Ok(db.json_opt("SELECT * FROM email_accounts WHERE id = $1", params![id]).await?.map(Account::from_map))
}

/// Whether the account signs in with a refreshable Microsoft/OIDC access token.
pub fn uses_token_refresh(account: &Account) -> bool {
    matches!(account.str("oauth_provider"), Some("microsoft" | "oidc"))
}

/// `ensureFreshToken`: refresh a Microsoft/OIDC token expiring within 5 minutes, or right away
/// when `force` (the mail server just rejected the current one). A temporary refresh failure
/// keeps the current token; only a failure that needs a new sign-in is returned.
pub async fn ensure_fresh_token(db: &Db, account: Account, force: bool) -> Result<Account, RefreshError> {
    if !uses_token_refresh(&account) {
        return Ok(account);
    }
    let provider = account.str("oauth_provider").map(str::to_owned);
    if !force {
        let Some(expiry) = account.str("oauth_token_expiry").and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        else {
            return Ok(account);
        };
        if expiry.with_timezone(&chrono::Utc) - chrono::Utc::now() >= chrono::Duration::minutes(5) {
            return Ok(account);
        }
    }
    tracing::info!("Refreshing {} token for {}", provider.as_deref().unwrap_or(""), account.log());
    let map = (*account.0).clone();
    let refreshed = if provider.as_deref() == Some("microsoft") {
        crate::services::oauth::refresh_microsoft_token(db, map).await
    } else {
        crate::services::oidc::refresh_mail_account_token(db, map).await
    };
    match refreshed {
        Ok(m) => Ok(Account::from_map(m)),
        Err(e) if e.reauth => {
            tracing::error!("Token refresh rejected for {}: {e}", account.log());
            Err(e)
        }
        Err(e) => {
            tracing::error!("Token refresh failed for {}: {e}", account.log());
            Ok(account)
        }
    }
}

/// Resolved connection target (`resolveForConnection` + policy).
#[derive(Debug, Clone)]
pub struct Target {
    pub addresses: Vec<IpAddr>,
    pub servername: String,
    pub allow_insecure_tls: bool,
}

pub async fn resolve_target(db: &Db, account: &Account) -> Result<Target, String> {
    let policy = connection_policy(db).await.map_err(|e| e.to_string())?;
    let host = account.str("imap_host").unwrap_or("").trim().to_owned();
    let addrs = inboxora_net::resolve_for_connection(&host, policy.allow_private_hosts).await?;
    let addresses = match addrs {
        Some(a) => a,
        None => {
            let bare = host.trim_start_matches('[').trim_end_matches(']');
            match bare.parse::<IpAddr>() {
                Ok(ip) => vec![ip],
                Err(_) => return Err(format!("getaddrinfo ENOTFOUND {host}")),
            }
        }
    };
    Ok(Target { addresses, servername: host, allow_insecure_tls: policy.allow_insecure_tls })
}

/// `makeClientCfg`.
pub fn client_config(account: &Account, target: &Target) -> Result<Config, String> {
    let tls = account.bool("imap_tls").unwrap_or(true);
    if !target.allow_insecure_tls && !tls {
        return Err("Plain-text IMAP is not allowed: admin must enable \"Allow insecure TLS\"".into());
    }
    let skip_verify = target.allow_insecure_tls && account.bool("imap_skip_tls_verify").unwrap_or(false);
    let user = account.str("auth_user").or(account.str("email_address")).unwrap_or("").to_owned();
    let mut auth = Auth::Password { user: user.clone(), pass: account.str("auth_pass").and_then(crypto::decrypt) };
    if matches!(account.str("oauth_provider"), Some("google" | "microsoft" | "oidc"))
        && let Some(token) = account.str("oauth_access_token").and_then(crypto::decrypt)
    {
        auth = Auth::OAuth { user, token };
    }
    let port = account.0.get("imap_port").and_then(Value::as_u64).unwrap_or(993) as u16;
    Ok(Config {
        addresses: target.addresses.clone(),
        servername: target.servername.clone(),
        port,
        secure: tls,
        verify_tls: !skip_verify,
        auth,
        command_timeout: Duration::from_secs(30),
        compress: true,
    })
}

/// A per-key counting semaphore with FIFO hand-off (`createKeyedSemaphore`). Each key is a
/// tokio semaphore, so a waiter abandoned by a timeout never strands its slot.
pub struct KeyedSemaphore {
    limit: usize,
    slots: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl KeyedSemaphore {
    pub fn new(limit: usize) -> Self {
        Self { limit, slots: Mutex::new(HashMap::new()) }
    }

    fn sem(&self, key: &str) -> Arc<Semaphore> {
        self.slots.lock().entry(key.to_owned()).or_insert_with(|| Arc::new(Semaphore::new(self.limit))).clone()
    }

    pub async fn acquire(&self, key: &str) -> OwnedSemaphorePermit {
        self.sem(key).acquire_owned().await.expect("keyed semaphore is never closed")
    }

    pub fn active(&self, key: &str) -> usize {
        self.slots.lock().get(key).map_or(0, |s| self.limit - s.available_permits())
    }
}

/// Holds a keyed-semaphore slot until dropped.
pub struct SlotGuard {
    _permit: OwnedSemaphorePermit,
}

impl SlotGuard {
    pub async fn acquire(sem: &KeyedSemaphore, key: &str) -> SlotGuard {
        SlotGuard { _permit: sem.acquire(key).await }
    }
}

/// `connectImapClient`: admission-controlled connect with the IPv4 fallback for a stalled
/// dual-stack handshake (#382/#384). `on_refusal` reports whether the provider refused.
pub async fn connect_client(
    sem: &KeyedSemaphore,
    account: &Account,
    target: &Target,
    timeout: Duration,
    label: &str,
) -> Result<Client, String> {
    let host = account.host();
    let host = host.as_str();
    let cfg = client_config(account, target)?;
    let attempt = |cfg: Config, tag: String| async move {
        let _slot = SlotGuard::acquire(sem, host).await;
        match tokio::time::timeout(timeout, Client::connect(&cfg)).await {
            Ok(Ok(c)) => Ok(c),
            Ok(Err(e)) => Err(connect_error_text(&e)),
            Err(_) => Err(format!("{tag} timeout ({}ms)", timeout.as_millis())),
        }
    };
    match attempt(cfg.clone(), label.to_owned()).await {
        Ok(c) => Ok(c),
        Err(e) => {
            let refused = policy::is_connection_refusal(&e) && !e.contains("timeout (");
            if !policy::should_retry_ipv4(&e, &cfg.addresses, refused) {
                return Err(e);
            }
            tracing::warn!("IMAP connect stalled for {} ({label}); retrying IPv4-only", account.log());
            let mut v4 = cfg.clone();
            v4.addresses.retain(IpAddr::is_ipv4);
            attempt(v4, format!("{label} IPv4-retry")).await
        }
    }
}

/// A connect failure's text. An authentication refusal keeps the server's response code (e.g.
/// `[UNAVAILABLE]` when the server's own auth backend is down) so a rejected sign-in can be
/// told apart from a temporary one.
fn connect_error_text(e: &inboxora_imap::Error) -> String {
    match e {
        inboxora_imap::Error::Auth { code: Some(code), .. } => format!("{e} [{code}]"),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keyed_semaphore_hands_off_fifo() {
        let sem = Arc::new(KeyedSemaphore::new(1));
        let held = sem.acquire("h").await;
        assert_eq!(sem.active("h"), 1);
        let s2 = sem.clone();
        let waiter = tokio::spawn(async move {
            let _p = s2.acquire("h").await;
        });
        tokio::task::yield_now().await;
        drop(held);
        waiter.await.unwrap();
        assert_eq!(sem.active("h"), 0);
        // Other keys are independent.
        let (_a, _b) = (sem.acquire("a").await, sem.acquire("b").await);
        assert_eq!((sem.active("a"), sem.active("b")), (1, 1));
    }

    #[tokio::test]
    async fn keyed_semaphore_survives_abandoned_waiter() {
        let sem = KeyedSemaphore::new(1);
        let held = sem.acquire("h").await;
        // A waiter that times out must not keep (or strand) the slot.
        assert!(tokio::time::timeout(Duration::from_millis(10), sem.acquire("h")).await.is_err());
        drop(held);
        assert_eq!(sem.active("h"), 0);
        let _p = tokio::time::timeout(Duration::from_millis(100), sem.acquire("h")).await.expect("slot free");
    }

    #[test]
    fn redaction() {
        assert_eq!(redact_email("alice@example.com"), "a***@example.com");
    }
}
