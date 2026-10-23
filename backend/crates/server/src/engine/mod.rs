//! The IMAP mail engine (port of `imapManager.ts`): one persistent IDLE connection per
//! account for push and the periodic INBOX sync, a small per-account pool for user-driven
//! fetches and mutations, background backfill on a per-host budget, and
//! the recovery timers (health check, staleness probe, flag-push reconciler, snooze).

pub mod backfill;
pub mod background;
pub mod cache;
pub mod connect;
pub mod limits;
pub mod mime;
pub mod ops;
pub mod parse;
pub mod policy;
pub mod pool;
pub mod preload;
pub mod search;
pub mod sync;

#[cfg(test)]
mod live_tests;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use inboxora_core::events::Events;
use inboxora_core::params;
use inboxora_core::pg::{Db, Q};
use inboxora_imap::{Client, Event};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use self::connect::{Account, KeyedSemaphore, connect_client, ensure_fresh_token, load_account, resolve_target};
use self::limits::{Kind, Limiter, Permit, Prio};
use self::policy::*;
use self::pool::{Lease, Pool, Slot};
use crate::services::diagnostics_ring::{record_broadcast, record_sync_signal, record_warning};

/// An engine failure; its text is what Node's `extractImapError` would report.
#[derive(Debug, Clone)]
pub struct EngineError(pub String);

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EngineError {}

impl From<inboxora_core::Error> for EngineError {
    fn from(e: inboxora_core::Error) -> Self {
        EngineError(e.to_string())
    }
}
impl From<inboxora_imap::Error> for EngineError {
    fn from(e: inboxora_imap::Error) -> Self {
        EngineError(e.to_string())
    }
}
impl From<String> for EngineError {
    fn from(e: String) -> Self {
        EngineError(e)
    }
}
impl From<&str> for EngineError {
    fn from(e: &str) -> Self {
        EngineError(e.to_owned())
    }
}

pub type EResult<T> = Result<T, EngineError>;

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Race a step against a timeout (Node's `raceTimeout`).
pub async fn race<T>(ms: u64, label: &str, fut: impl Future<Output = EResult<T>>) -> EResult<T> {
    match tokio::time::timeout(Duration::from_millis(ms), fut).await {
        Ok(r) => r,
        Err(_) => Err(EngineError(format!("{label} timeout ({ms}ms)"))),
    }
}

/// The persistent sync connection: shared between the IDLE loop and sync ticks. A user of the
/// connection signals `wants`, which breaks IDLE (imapflow's auto-IDLE behaviour).
pub struct Persistent {
    pub generation: u64,
    client: tokio::sync::Mutex<Option<Client>>,
    wants: AtomicUsize,
    wake: Notify,
    closed: AtomicBool,
    closed_tx: watch::Sender<bool>,
}

pub struct PersistentGuard<'a> {
    guard: tokio::sync::MutexGuard<'a, Option<Client>>,
}

impl PersistentGuard<'_> {
    pub fn client(&mut self) -> EResult<&mut Client> {
        self.guard.as_mut().filter(|c| c.is_usable()).ok_or_else(|| EngineError("Connection not available".into()))
    }
}

impl Persistent {
    fn new(generation: u64, client: Client) -> Arc<Self> {
        Arc::new(Self {
            generation,
            client: tokio::sync::Mutex::new(Some(client)),
            wants: AtomicUsize::new(0),
            wake: Notify::new(),
            closed: AtomicBool::new(false),
            closed_tx: watch::channel(false).0,
        })
    }

    /// Take the connection (breaking IDLE).
    pub async fn lock(&self) -> PersistentGuard<'_> {
        self.wants.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_waiters();
        let guard = self.client.lock().await;
        self.wants.fetch_sub(1, Ordering::SeqCst);
        PersistentGuard { guard }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Force-close (imapflow `close()`): a command in flight on this connection is abandoned.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.closed_tx.send(true);
        self.wake.notify_waiters();
    }

    /// Resolves once the connection is force-closed.
    pub async fn closed(&self) {
        let mut rx = self.closed_tx.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }
}

#[derive(Debug, Clone)]
pub struct FlagOp {
    pub message_id: String,
    pub flag: String,
    pub value: bool,
    pub attempts: u32,
    pub resolved: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct State {
    pub connections: HashMap<String, Arc<Persistent>>,
    pub sync_timers: HashMap<String, JoinHandle<()>>,
    pub backfill_running: HashSet<String>,
    pub backfill_all_running: HashSet<String>,
    pub connect_cooldown: HashMap<String, (u64, u32)>,
    pub sync_error_state: HashMap<String, Option<String>>,
    pub pending_flag_push: HashMap<String, HashMap<String, FlagOp>>,
    pub pending_flag_sync: HashSet<String>,
    pub pending_inbox_sync: HashSet<String>,
    pub pending_move_uids: HashMap<String, u32>,
    pub poll_only: HashSet<String>,
    pub connecting: HashSet<String>,
    pub syncing: HashSet<String>,
    pub on_demand_syncing: HashSet<String>,
    pub other_folders_syncing: HashSet<String>,
    pub reconciling: HashSet<String>,
    pub last_sync_ok_at: HashMap<String, u64>,
    pub last_folder_sync_at: HashMap<String, u64>,
    pub sync_started_at: HashMap<String, u64>,
    pub sync_tick_count: HashMap<String, u64>,
    pub throttle_skips: HashMap<String, u32>,
    pub user_sync_interval_ms: HashMap<String, u64>,
    pub user_folder_sync_interval_ms: HashMap<String, u64>,
    pub flag_debounce: HashMap<String, JoinHandle<()>>,
    pub expunge_debounce: HashMap<String, JoinHandle<()>>,
    pub preload_running: HashSet<String>,
    pub last_warm_preload: HashMap<String, u64>,
}

pub struct Engine {
    pub db: Db,
    pub events: Events,
    pub cache: cache::ContentCache,
    pub st: Mutex<State>,
    pub connect_sem: KeyedSemaphore,
    pub bg_sem: KeyedSemaphore,
    pools: Mutex<HashMap<String, Arc<Pool>>>,
    limiters: Mutex<HashMap<String, Arc<Limiter>>>,
    /// `IMAP_MAX_CONNECTIONS_PER_ACCOUNT`: the start value for servers without an admin setting.
    default_conn_limit: usize,
    pub preload: Mutex<HashMap<String, preload::Status>>,
    persistent_cap: Option<u32>,
    generation: AtomicU64,
    pub started: Instant,
}

pub type EngineRef = Arc<Engine>;

/// Removes a key from a state set on drop.
pub struct SetGuard {
    engine: EngineRef,
    which: fn(&mut State) -> &mut HashSet<String>,
    key: String,
}

impl Drop for SetGuard {
    fn drop(&mut self) {
        let mut st = self.engine.st.lock();
        (self.which)(&mut st).remove(&self.key);
    }
}

impl Engine {
    pub fn new(db: Db, events: Events, redis: Option<fred::prelude::Pool>) -> EngineRef {
        Arc::new(Self {
            db,
            events,
            cache: cache::ContentCache::new(redis),
            st: Mutex::new(State::default()),
            connect_sem: KeyedSemaphore::new(CONNECT_CONCURRENCY_PER_HOST),
            bg_sem: KeyedSemaphore::new(BACKGROUND_CONN_MAX_PER_HOST),
            pools: Mutex::new(HashMap::new()),
            limiters: Mutex::new(HashMap::new()),
            default_conn_limit: inboxora_core::config::env("IMAP_MAX_CONNECTIONS_PER_ACCOUNT")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .map_or(limits::DEFAULT_START_LIMIT, |v| v.clamp(limits::LIMIT_FLOOR, limits::LIMIT_MAX)),
            preload: Mutex::new(HashMap::new()),
            persistent_cap: parse_persistent_cap(inboxora_core::config::env("IMAP_MAX_PERSISTENT_PER_HOST").as_deref()),
            generation: AtomicU64::new(1),
            started: Instant::now(),
        })
    }

    /// Claim `key` in a state set; `None` when already claimed.
    pub fn claim(self: &Arc<Self>, which: fn(&mut State) -> &mut HashSet<String>, key: &str) -> Option<SetGuard> {
        let mut st = self.st.lock();
        if !which(&mut st).insert(key.to_owned()) {
            return None;
        }
        Some(SetGuard { engine: self.clone(), which, key: key.to_owned() })
    }

    pub fn broadcast(&self, data: Value, user_id: Option<&str>) {
        if let Some(t) = data.get("type").and_then(Value::as_str) {
            record_broadcast(t);
        }
        self.events.broadcast(&data, user_id);
    }

    pub fn is_connected(&self, account_id: &str) -> bool {
        self.st.lock().connections.contains_key(account_id)
    }

    pub fn persistent(&self, account_id: &str) -> Option<Arc<Persistent>> {
        self.st.lock().connections.get(account_id).cloned()
    }

    // ── move guards ─────────────────────────────────────────────────────────────────────

    pub fn guard_move_uid(&self, account_id: &str, folder: &str, uid: i64) {
        *self.st.lock().pending_move_uids.entry(format!("{account_id}:{folder}:{uid}")).or_insert(0) += 1;
    }

    pub fn unguard_move_uid(&self, account_id: &str, folder: &str, uid: i64) {
        let key = format!("{account_id}:{folder}:{uid}");
        let mut st = self.st.lock();
        match st.pending_move_uids.get_mut(&key) {
            Some(n) if *n > 1 => *n -= 1,
            _ => {
                st.pending_move_uids.remove(&key);
            }
        }
    }

    pub fn is_move_uid_guarded(&self, account_id: &str, folder: &str, uid: i64) -> bool {
        self.st.lock().pending_move_uids.contains_key(&format!("{account_id}:{folder}:{uid}"))
    }

    // ── connections ─────────────────────────────────────────────────────────────────────

    /// The account's connection budget, created from its stored limits on first use.
    pub async fn limiter(&self, account: &Account) -> Arc<Limiter> {
        if let Some(l) = self.limiters.lock().get(account.id()) {
            return l.clone();
        }
        let cfg = self.limit_config(account).await;
        self.limiters.lock().entry(account.id().to_owned()).or_insert_with(|| Limiter::new(cfg)).clone()
    }

    /// Start value and auto-lower from the admin's server setting, learned value from the account.
    async fn limit_config(&self, account: &Account) -> limits::Config {
        let learned = account
            .map()
            .get("imap_conn_limit")
            .and_then(Value::as_i64)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v > 0);
        let port = account.map().get("imap_port").and_then(Value::as_i64).unwrap_or(993);
        let server = self
            .db
            .opt(
                "SELECT start_limit, auto_lower FROM imap_server_limits WHERE host = $1 AND port = $2",
                params![account.host(), port as i32],
            )
            .await
            .ok()
            .flatten();
        let start = server
            .as_ref()
            .and_then(|r| r.get::<_, Option<i16>>("start_limit"))
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(self.default_conn_limit);
        let auto_lower = server.as_ref().is_none_or(|r| r.get::<_, bool>("auto_lower"));
        limits::Config { start, learned, auto_lower }
    }

    /// Re-read an account's limits after an admin change or reset (live, no reconnect).
    pub async fn reload_limits(&self, account_id: &str) {
        let Some(l) = self.limiters.lock().get(account_id).cloned() else { return };
        let Ok(Some(account)) = load_account(&self.db, account_id).await else { return };
        l.configure(self.limit_config(&account).await);
        if let Some(p) = self.pools.lock().get(account_id) {
            p.resize(l.pool_size());
        }
    }

    pub fn default_conn_limit(&self) -> usize {
        self.default_conn_limit
    }

    /// Live connection numbers of an account (diagnostics, admin).
    pub fn limit_snapshot(&self, account_id: &str) -> Option<Value> {
        let l = self.limiters.lock().get(account_id).cloned()?;
        let mut v = l.snapshot();
        if let Some(p) = self.pools.lock().get(account_id) {
            let (idle, in_use) = p.counts();
            v["poolIdle"] = json!(idle);
            v["poolInUse"] = json!(in_use);
        }
        Some(v)
    }

    /// A slot of the account budget. At the limit an idle pooled connection is closed first;
    /// otherwise the request queues by priority (bounded so a stuck queue surfaces as an error).
    async fn permit(&self, account: &Account, prio: Prio, kind: Kind) -> EResult<Permit> {
        let l = self.limiter(account).await;
        if let Some(p) = l.try_acquire(kind) {
            return Ok(p);
        }
        let pool = self.pools.lock().get(account.id()).cloned();
        if pool.is_some_and(|p| p.drop_one_idle()) {
            tokio::task::yield_now().await;
        }
        let wait = if prio == Prio::Ui { Duration::from_secs(30) } else { Duration::from_secs(300) };
        tokio::time::timeout(wait, l.acquire(prio, kind))
            .await
            .map_err(|_| EngineError("IMAP connection limit reached".into()))
    }

    /// Open a counted connection: the permit lives as long as the socket.
    async fn connect_counted(
        &self,
        account: &Account,
        target: &connect::Target,
        label: &str,
        prio: Prio,
        kind: Kind,
    ) -> EResult<Client> {
        let permit = self.permit(account, prio, kind).await?;
        match connect_client(&self.connect_sem, account, target, Duration::from_secs(30), label).await {
            Ok(mut c) => {
                c.attach_guard(Box::new(permit));
                Ok(c)
            }
            Err(e) => {
                if limits::is_limit_signal(&e) {
                    let observed = self.limiter(account).await.total();
                    self.lower_limit(account, observed, &e).await;
                }
                Err(EngineError(e))
            }
        }
    }

    /// Step the account's limit down after a refusal seen with `observed` connections open.
    pub async fn lower_limit(&self, account: &Account, observed: usize, reason: &str) {
        let l = self.limiter(account).await;
        let Some(next) = l.lower(observed, reason) else { return };
        tracing::warn!("[imap-limit] {} limit {} → {next} ({reason})", account.log(), next + 1);
        if let Some(p) = self.pools.lock().get(account.id()) {
            p.resize(l.pool_size());
        }
        if let Err(e) = self
            .db
            .exec(
                "UPDATE email_accounts SET imap_conn_limit = $1::int, imap_conn_limit_at = NOW(), imap_conn_limit_reason = $2 WHERE id = $3",
                params![next as i32, reason.chars().take(200).collect::<String>(), account.id()],
            )
            .await
        {
            tracing::warn!("[imap-limit] could not store limit for {}: {e}", account.log());
        }
    }

    /// A sync error on `client`: an explicit limit refusal, or a socket the server closed right
    /// after login while other connections were open, means the account is over its limit.
    pub async fn note_conn_failure(&self, account: &Account, client: &Client, detail: &str) {
        if client.is_usable() {
            return;
        }
        let others = self.limiter(account).await.total();
        let early = client.age() < limits::EARLY_DROP && others >= 1;
        if limits::is_limit_signal(detail) || early {
            self.lower_limit(account, others + 1, detail).await;
        }
    }

    /// A fresh, unpooled connection (`connectImapClient` after token refresh and resolve).
    pub async fn open_client(
        &self,
        account: &Account,
        label: &str,
        prio: Prio,
        kind: Kind,
    ) -> EResult<(Account, Client)> {
        let fresh = ensure_fresh_token(&self.db, account.clone()).await;
        let target = resolve_target(&self.db, &fresh).await?;
        let client = self.connect_counted(&fresh, &target, label, prio, kind).await?;
        Ok((fresh, client))
    }

    /// `withFreshLogin`'s connection: token refresh and resolution bounded like Node's.
    pub async fn open_client_bounded(
        &self,
        account: &Account,
        label: &str,
        prio: Prio,
        kind: Kind,
    ) -> EResult<(Account, Client)> {
        let fresh = race(15_000, &format!("{label} token refresh"), async {
            Ok(ensure_fresh_token(&self.db, account.clone()).await)
        })
        .await?;
        let target = race(15_000, &format!("{label} host resolve"), async {
            resolve_target(&self.db, &fresh).await.map_err(EngineError)
        })
        .await?;
        let client = self.connect_counted(&fresh, &target, label, prio, kind).await?;
        Ok((fresh, client))
    }

    async fn pool_for(&self, account: &Account) -> Arc<Pool> {
        let size = self.limiter(account).await.pool_size();
        let pool =
            self.pools.lock().entry(account.id().to_owned()).or_insert_with(|| Arc::new(Pool::new(size))).clone();
        pool.resize(size);
        pool
    }

    /// `acquirePooledClient` for user-driven work.
    pub async fn lease(&self, account: &Account) -> EResult<Lease> {
        self.lease_with(account, Prio::Ui).await
    }

    /// A pooled connection; new sockets queue for the account budget at `prio`.
    pub async fn lease_with(&self, account: &Account, prio: Prio) -> EResult<Lease> {
        let pool = self.pool_for(account).await;
        match pool.try_acquire() {
            Slot::Ready(l) => Ok(l),
            Slot::Connect(r) => {
                let (_, client) = self.open_client(account, "IMAP pool connect", prio, Kind::Pool).await?;
                match pool.admit(r, client) {
                    Ok(l) => Ok(l),
                    Err(mut c) => {
                        tokio::spawn(async move { c.logout().await });
                        Err("IMAP pool was evicted while connecting".into())
                    }
                }
            }
            Slot::Wait(rx) => match tokio::time::timeout(pool::WAIT_FOR_SLOT, rx).await {
                Ok(Ok(client)) => Ok(pool.handed(client)),
                Ok(Err(_)) => Err("IMAP pool evicted".into()),
                Err(_) => {
                    let (_, client) = self.open_client(account, "IMAP temp connect", prio, Kind::Temporary).await?;
                    Ok(Lease::temporary(client))
                }
            },
        }
    }

    pub fn evict_pool(&self, account_id: &str) {
        if let Some(p) = self.pools.lock().remove(account_id) {
            p.evict();
        }
    }

    /// Persist and broadcast an account failure (de-duplicated against the last value).
    pub async fn record_account_error(&self, account: &Account, detail: &str) {
        if self.st.lock().sync_error_state.get(account.id()).is_some_and(|v| v.as_deref() == Some(detail)) {
            return;
        }
        match self
            .db
            .exec("UPDATE email_accounts SET sync_error = $1 WHERE id = $2", params![detail, account.id()])
            .await
        {
            Ok(_) => {
                self.st.lock().sync_error_state.insert(account.id().to_owned(), Some(detail.to_owned()));
                self.broadcast(
                    json!({ "type": "account_error", "accountId": account.id(), "error": detail }),
                    Some(account.user_id()),
                );
            }
            Err(e) => tracing::warn!("Could not record sync_error for {}: {e}", account.log()),
        }
    }

    pub async fn clear_account_error(&self, account: &Account) {
        let prev = self.st.lock().sync_error_state.get(account.id()).cloned();
        if prev == Some(None) {
            return;
        }
        match self.db.exec("UPDATE email_accounts SET sync_error = NULL WHERE id = $1", params![account.id()]).await {
            Ok(_) => {
                self.st.lock().sync_error_state.insert(account.id().to_owned(), None);
                if matches!(prev, Some(Some(_))) {
                    self.broadcast(
                        json!({ "type": "account_connected", "accountId": account.id() }),
                        Some(account.user_id()),
                    );
                }
            }
            Err(e) => tracing::warn!("Could not clear sync_error for {}: {e}", account.log()),
        }
    }

    pub fn note_connection_refusal(&self, account: &Account) -> u64 {
        let mut st = self.st.lock();
        let failures = st.connect_cooldown.get(account.id()).map_or(0, |c| c.1) + 1;
        let ms = connect_cooldown_ms(failures);
        st.connect_cooldown.insert(account.id().to_owned(), (now_ms() + ms, failures));
        tracing::warn!(
            "Connection refused for {} — backing off {}s (refusal #{failures})",
            account.log(),
            (ms as f64 / 1000.0).round()
        );
        ms
    }

    fn cooling_down(&self, account_id: &str) -> bool {
        self.st.lock().connect_cooldown.get(account_id).is_some_and(|(until, _)| now_ms() < *until)
    }

    async fn persistent_eligible(&self, account: &Account, cap: u32) -> bool {
        let host = account.host();
        if host.is_empty() {
            return true;
        }
        match self
            .db
            .rows(
                "SELECT id FROM email_accounts WHERE enabled = true AND protocol = 'imap' AND lower(imap_host) = $1 ORDER BY created_at ASC NULLS FIRST, id ASC",
                params![host],
            )
            .await
        {
            Ok(rows) => {
                let ids: Vec<String> = rows.iter().map(|r| r.get::<_, uuid::Uuid>("id").to_string()).collect();
                persistent_eligible(&ids, account.id(), Some(cap))
            }
            Err(_) => true,
        }
    }

    /// Install a persistent connection: events, IDLE loop and the connection map entry.
    fn install_persistent(self: &Arc<Self>, account: &Account, mut client: Client) -> Arc<Persistent> {
        let (tx, rx) = mpsc::unbounded_channel();
        client.set_events(tx);
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        let p = Persistent::new(generation, client);
        self.st.lock().connections.insert(account.id().to_owned(), p.clone());
        let keepalive = account.profile().idle_keepalive_ms.unwrap_or(25 * 60_000);
        tokio::spawn(idle_loop(p.clone(), Duration::from_millis(keepalive)));
        tokio::spawn(self.clone().event_loop(account.clone(), p.clone(), rx));
        p
    }

    /// Remove the persistent connection only when it is still `p` (identity guard).
    pub fn drop_persistent(&self, account_id: &str, p: &Arc<Persistent>) {
        let mut st = self.st.lock();
        if st.connections.get(account_id).is_some_and(|c| c.generation == p.generation) {
            st.connections.remove(account_id);
        }
        drop(st);
        p.close();
    }

    async fn event_loop(self: Arc<Self>, account: Account, p: Arc<Persistent>, mut rx: mpsc::UnboundedReceiver<Event>) {
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::Exists { count, prev_count, .. } => {
                    if count <= prev_count {
                        continue;
                    }
                    self.broadcast(
                        json!({ "type": "exists_hint", "accountId": account.id(), "delta": count - prev_count }),
                        Some(account.user_id()),
                    );
                    if self.st.lock().syncing.contains(account.id()) {
                        self.st.lock().pending_inbox_sync.insert(account.id().to_owned());
                        continue;
                    }
                    tracing::info!("IMAP IDLE: new mail for {} ({prev_count} → {count})", account.log());
                    let (e, a) = (self.clone(), account.clone());
                    tokio::spawn(async move { e.sync_tick(a).await });
                }
                Event::Flags { .. } => {
                    let (e, a) = (self.clone(), account.clone());
                    let handle = tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        e.st.lock().flag_debounce.remove(a.id());
                        tracing::info!("IMAP IDLE: flag change for {}, syncing flags", a.log());
                        e.sync_flags_for_range(&a).await;
                    });
                    if let Some(old) = self.st.lock().flag_debounce.insert(account.id().to_owned(), handle) {
                        old.abort();
                    }
                }
                Event::Expunge { .. } | Event::Vanished { .. } => {
                    let (e, a) = (self.clone(), account.clone());
                    let handle = tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        e.st.lock().expunge_debounce.remove(a.id());
                        tracing::info!("IMAP IDLE: expunge for {}, reconciling", a.log());
                        if let Err(err) = e.reconcile_deletes(&a).await {
                            tracing::warn!("Expunge-triggered reconcile error for {}: {err}", a.log());
                        }
                    });
                    if let Some(old) = self.st.lock().expunge_debounce.insert(account.id().to_owned(), handle) {
                        old.abort();
                    }
                }
                Event::Closed => {
                    let mut st = self.st.lock();
                    if st.connections.get(account.id()).is_some_and(|c| c.generation == p.generation) {
                        st.connections.remove(account.id());
                        tracing::info!("IMAP connection closed for {}", account.log());
                    }
                    break;
                }
            }
        }
    }

    /// `connectAccount`.
    pub async fn connect_account(self: &Arc<Self>, account: Account) -> bool {
        if let Some((until, failures)) = self.st.lock().connect_cooldown.get(account.id()).copied()
            && now_ms() < until
        {
            tracing::debug!(
                "connectAccount: {} cooling down {}s after {failures} refusal(s)",
                account.log(),
                (until - now_ms()) / 1000
            );
            self.preload_failed(&account, "Connection refused by the mail server, retrying later");
            return false;
        }
        let Some(_connecting) = self.claim(|s| &mut s.connecting, account.id()) else {
            tracing::info!("Already connecting {}, skipping duplicate", account.log());
            return false;
        };
        let port = account.0.get("imap_port").and_then(serde_json::Value::as_i64).unwrap_or(993);
        tracing::info!("Connecting {} ({}:{port})…", account.log(), account.str("imap_host").unwrap_or(""));
        self.disconnect_account(account.id()).await;
        let profile = account.profile();
        if let Some(cap) = resolve_persistent_cap(self.persistent_cap, profile.max_persistent_per_host)
            && !self.persistent_eligible(&account, cap).await
        {
            self.start_poll_only(&account);
            self.cold_preload(&account);
            return true;
        }
        let (account, client) = match self.open_client(&account, "IMAP connect", Prio::Inbox, Kind::Persistent).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to connect {}: {e}", account.log());
                if is_connection_refusal(&e.0) {
                    self.note_connection_refusal(&account);
                }
                self.record_account_error(&account, &e.0).await;
                self.preload_failed(&account, &e.0);
                return false;
            }
        };
        let p = self.install_persistent(&account, client);
        self.clear_account_error(&account).await;
        let should_backfill = self.should_auto_backfill(&account).await;
        let initial = async {
            let mut g = p.lock().await;
            let c = g.client()?;
            race(20_000, "Initial folder sync", async { self.sync_folders(&account, c).await }).await?;
            self.st.lock().last_folder_sync_at.insert(account.id().to_owned(), now_ms());
            if profile.fresh_inbox_sync {
                drop(g);
                self.sync_inbox_with_fresh_login(&account).await.map(|_| ())
            } else {
                race(40_000, "Initial message sync", self.sync_messages(&account, c, "INBOX", 20, false, true))
                    .await
                    .map(|_| ())
            }
        };
        if let Err(e) = initial.await {
            tracing::warn!("Initial sync skipped for {}: {e}", account.log());
            if !p.lock().await.client().is_ok_and(|c| c.is_usable()) {
                self.drop_persistent(account.id(), &p);
            }
        }
        if !profile.prefer_fresh_body_fetch {
            let (e, a) = (self.clone(), account.clone());
            tokio::spawn(async move {
                if let Err(err) = e.lease(&a).await {
                    tracing::warn!("Pool pre-warm failed for {}: {err}", a.log());
                }
            });
        }
        if should_backfill {
            let (e, a) = (self.clone(), account.clone());
            tokio::spawn(async move { e.backfill_all_folders(&a).await });
        } else {
            tracing::debug!("Backfill deferred on connect for {} — account already has cached mail", account.log());
        }
        let interval =
            self.st.lock().user_sync_interval_ms.get(account.user_id()).copied().unwrap_or(DEFAULT_SYNC_INTERVAL_MS);
        self.start_sync_interval(&account, interval);
        self.st.lock().connect_cooldown.remove(account.id());
        self.cold_preload(&account);
        tracing::info!("Connected account: {}", account.log());
        self.broadcast(json!({ "type": "account_connected", "accountId": account.id() }), Some(account.user_id()));
        true
    }

    async fn should_auto_backfill(&self, account: &Account) -> bool {
        if account.profile().auto_backfill_existing_on_connect {
            return true;
        }
        matches!(
            self.db.opt("SELECT 1 FROM messages WHERE account_id = $1 LIMIT 1", params![account.id()]).await,
            Ok(None)
        )
    }

    /// `disconnectAccount`.
    pub async fn disconnect_account(&self, account_id: &str) {
        let (p, timers) = {
            let mut st = self.st.lock();
            st.pending_inbox_sync.remove(account_id);
            st.poll_only.remove(account_id);
            st.throttle_skips.remove(account_id);
            st.sync_tick_count.remove(account_id);
            st.last_sync_ok_at.remove(account_id);
            st.sync_error_state.remove(account_id);
            st.pending_flag_sync.remove(account_id);
            st.connect_cooldown.remove(account_id);
            st.last_folder_sync_at.remove(account_id);
            st.sync_started_at.remove(account_id);
            let timers = [
                st.sync_timers.remove(account_id),
                st.flag_debounce.remove(account_id),
                st.expunge_debounce.remove(account_id),
            ];
            (st.connections.remove(account_id), timers)
        };
        for t in timers.into_iter().flatten() {
            t.abort();
        }
        if let Some(p) = p {
            p.close();
            if let Ok(mut g) = tokio::time::timeout(Duration::from_secs(5), p.client.lock()).await
                && let Some(mut c) = g.take()
            {
                c.logout().await;
            }
        }
        self.evict_pool(account_id);
    }

    pub async fn disconnect_user(&self, user_id: &str) {
        match self
            .db
            .rows("SELECT id FROM email_accounts WHERE user_id = $1 AND protocol = 'imap'", params![user_id])
            .await
        {
            Ok(rows) => {
                let ids: Vec<String> = rows.iter().map(|r| r.get::<_, uuid::Uuid>("id").to_string()).collect();
                futures_util::future::join_all(ids.iter().map(|id| self.disconnect_account(id))).await;
            }
            Err(e) => tracing::error!("disconnectUser error for user {user_id}: {e}"),
        }
    }

    fn start_poll_only(self: &Arc<Self>, account: &Account) {
        self.st.lock().poll_only.insert(account.id().to_owned());
        tracing::info!(
            "Poll-only mode for {} — {} at persistent-connection budget; polling INBOX on the interval instead of holding IDLE",
            account.log(),
            account.str("imap_host").unwrap_or("")
        );
        let (e, a) = (self.clone(), account.clone());
        tokio::spawn(async move { e.clear_account_error(&a).await });
        self.broadcast(json!({ "type": "account_connected", "accountId": account.id() }), Some(account.user_id()));
        let ms = effective_sync_interval_ms(
            account.profile(),
            self.st.lock().user_sync_interval_ms.get(account.user_id()).copied().unwrap_or(DEFAULT_SYNC_INTERVAL_MS),
        );
        let jitter = rand_below(ms.min(30_000));
        let (e, a) = (self.clone(), account.clone());
        let handle = tokio::spawn(async move {
            e.poll_only_tick(&a).await;
            tokio::time::sleep(Duration::from_millis(jitter)).await;
            let mut iv = tokio::time::interval(Duration::from_millis(ms));
            iv.tick().await;
            loop {
                iv.tick().await;
                if !e.st.lock().poll_only.contains(a.id()) {
                    return;
                }
                e.poll_only_tick(&a).await;
            }
        });
        self.st.lock().sync_timers.insert(account.id().to_owned(), handle);
    }

    async fn poll_only_tick(self: &Arc<Self>, account: &Account) {
        if self.cooling_down(account.id()) {
            return;
        }
        let Some(_syncing) = self.claim(|s| &mut s.syncing, account.id()) else { return };
        let host = account.host();
        let _slot = connect::SlotGuard::acquire(&self.bg_sem, &host).await;
        let run = async {
            let (fresh, mut client) =
                self.open_client_bounded(account, "Poll-only", Prio::Inbox, Kind::Background).await?;
            let result = async {
                if self.folder_sync_due_for(account) {
                    self.st.lock().last_folder_sync_at.insert(account.id().to_owned(), now_ms());
                    match race(20_000, "Poll-only folder sync", async { self.sync_folders(&fresh, &mut client).await })
                        .await
                    {
                        Ok(()) => self.broadcast(
                            json!({ "type": "folders_synced", "accountId": account.id() }),
                            Some(account.user_id()),
                        ),
                        Err(e) => tracing::warn!("Poll-only folder sync failed for {}: {e}", account.log()),
                    }
                }
                race(40_000, "Poll-only INBOX sync", self.sync_messages(&fresh, &mut client, "INBOX", 20, false, true))
                    .await
            }
            .await;
            client.logout().await;
            result
        };
        match run.await {
            Ok(r) => {
                self.st.lock().last_sync_ok_at.insert(account.id().to_owned(), now_ms());
                self.st.lock().connect_cooldown.remove(account.id());
                self.clear_account_error(account).await;
                if r.inserted > 0 && !r.broadcasted_new {
                    self.broadcast(
                        json!({ "type": "sync_complete", "accountId": account.id() }),
                        Some(account.user_id()),
                    );
                }
            }
            Err(e) => {
                let refused = is_connection_refusal(&e.0);
                if refused {
                    self.note_connection_refusal(account);
                }
                tracing::warn!("Poll-only sync error for {}: {e}", account.log());
                if refused {
                    self.record_account_error(account, &e.0).await;
                }
            }
        }
    }

    fn folder_sync_due_for(&self, account: &Account) -> bool {
        let st = self.st.lock();
        let interval =
            st.user_folder_sync_interval_ms.get(account.user_id()).copied().unwrap_or(DEFAULT_FOLDER_SYNC_INTERVAL_MS);
        folder_sync_due(Some(interval), st.last_folder_sync_at.get(account.id()).copied(), now_ms())
    }

    /// `_syncInboxWithFreshLogin`.
    pub async fn sync_inbox_with_fresh_login(self: &Arc<Self>, account: &Account) -> EResult<sync::SyncResult> {
        let (_, mut client) = self.open_client_bounded(account, "Fresh sync", Prio::Inbox, Kind::Background).await?;
        let r =
            race(55_000, "Fresh sync wall-clock", self.sync_messages(account, &mut client, "INBOX", 20, false, true))
                .await;
        client.close();
        r
    }

    fn start_sync_interval(self: &Arc<Self>, account: &Account, ms: u64) {
        let ms = effective_sync_interval_ms(account.profile(), ms);
        let jitter = rand_below(ms.min(30_000));
        let (e, a) = (self.clone(), account.clone());
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(jitter)).await;
            let mut iv = tokio::time::interval(Duration::from_millis(ms));
            loop {
                iv.tick().await;
                let (e2, a2) = (e.clone(), a.clone());
                tokio::spawn(async move { e2.sync_tick(a2).await });
            }
        });
        if let Some(old) = self.st.lock().sync_timers.insert(account.id().to_owned(), handle) {
            old.abort();
        }
    }

    /// Replace running intervals for a user's accounts after a preference change.
    pub async fn update_sync_interval_for_user(self: &Arc<Self>, user_id: &str, ms: u64) {
        self.st.lock().user_sync_interval_ms.insert(user_id.to_owned(), ms);
        let Ok(rows) = self
            .db
            .json_rows(
                "SELECT * FROM email_accounts WHERE user_id = $1 AND enabled = true AND protocol = 'imap'",
                params![user_id],
            )
            .await
        else {
            return;
        };
        for r in rows {
            let Value::Object(m) = r else { continue };
            let acc = Account::from_map(m);
            let had = self.st.lock().sync_timers.remove(acc.id());
            if let Some(h) = had {
                h.abort();
                self.start_sync_interval(&acc, ms);
            }
        }
    }

    pub fn update_folder_sync_interval_for_user(&self, user_id: &str, ms: u64) {
        self.st.lock().user_folder_sync_interval_ms.insert(user_id.to_owned(), ms);
    }

    /// `_syncTick`: the periodic INBOX sync on the persistent connection (reconnecting when
    /// needed), then folder refresh, other folders, flag poll and delete reconciliation.
    pub async fn sync_tick(self: Arc<Self>, account: Account) {
        {
            let mut st = self.st.lock();
            if let Some(n) = st.throttle_skips.get_mut(account.id())
                && *n > 0
            {
                *n -= 1;
                return;
            }
        }
        let Some(syncing) = self.claim(|s| &mut s.syncing, account.id()) else { return };
        self.st.lock().sync_started_at.insert(account.id().to_owned(), now_ms());
        let mut active = self.persistent(account.id());
        let mut sync_account = account.clone();
        if active.is_none() {
            if self.cooling_down(account.id()) {
                self.finish_tick(&account, syncing);
                return;
            }
            let Some(_connecting) = self.claim(|s| &mut s.connecting, account.id()) else {
                self.finish_tick(&account, syncing);
                return;
            };
            tracing::info!("Reconnecting {}...", account.log());
            let setup = race(20_000, "Reconnect setup", async {
                let Some(row) = load_account(&self.db, account.id()).await? else { return Ok(None) };
                if !row.enabled() {
                    return Ok(None);
                }
                let fresh = ensure_fresh_token(&self.db, row).await;
                let target = resolve_target(&self.db, &fresh).await?;
                Ok(Some((fresh, target)))
            })
            .await;
            let connected = match setup {
                Ok(None) => {
                    self.finish_tick(&account, syncing);
                    return;
                }
                Ok(Some((fresh, target))) => self
                    .connect_counted(&fresh, &target, "Reconnect", Prio::Inbox, Kind::Persistent)
                    .await
                    .map(|c| (fresh, c)),
                Err(e) => Err(e),
            };
            match connected {
                Ok((fresh, client)) => {
                    active = Some(self.install_persistent(&fresh, client));
                    sync_account = fresh;
                    self.st.lock().connect_cooldown.remove(account.id());
                    self.clear_account_error(&account).await;
                    tracing::info!("Reconnected {}", sync_account.log());
                }
                Err(e) => {
                    if is_connection_refusal(&e.0) {
                        self.note_connection_refusal(&account);
                    }
                    tracing::error!("Reconnect failed for {}: {e}", account.log());
                    self.record_account_error(&account, &e.0).await;
                    self.finish_tick(&account, syncing);
                    return;
                }
            }
        }
        if self.cooling_down(account.id()) {
            self.finish_tick(&account, syncing);
            return;
        }
        let p = active.expect("connection");
        let profile = sync_account.profile();
        let used_fresh = profile.fresh_inbox_sync;
        let result = if used_fresh {
            self.sync_inbox_with_fresh_login(&sync_account).await
        } else {
            let work = async {
                let mut g = p.lock().await;
                let c = g.client()?;
                self.sync_messages(&sync_account, c, "INBOX", 20, false, true).await
            };
            tokio::select! {
                r = race(55_000, "Sync wall-clock", work) => r,
                _ = p.closed() => Err(EngineError("Connection closed".into())),
            }
        };
        match result {
            Ok(r) => {
                {
                    let mut st = self.st.lock();
                    st.last_sync_ok_at.insert(account.id().to_owned(), now_ms());
                    st.connect_cooldown.remove(account.id());
                }
                self.clear_account_error(&account).await;
                if r.inserted > 0 && !r.broadcasted_new {
                    self.broadcast(
                        json!({ "type": "sync_complete", "accountId": account.id() }),
                        Some(account.user_id()),
                    );
                }
                self.warm_preload(&sync_account, r.inserted > 0);
                *self.st.lock().sync_tick_count.entry(account.id().to_owned()).or_insert(0) += 1;
                if self.folder_sync_due_for(&sync_account) {
                    self.st.lock().last_folder_sync_at.insert(account.id().to_owned(), now_ms());
                    let folders = async {
                        let mut g = p.lock().await;
                        let c = g.client()?;
                        race(20_000, "Periodic folder sync", async { self.sync_folders(&sync_account, c).await }).await
                    };
                    match folders.await {
                        Ok(()) => self.broadcast(
                            json!({ "type": "folders_synced", "accountId": account.id() }),
                            Some(sync_account.user_id()),
                        ),
                        Err(e) => tracing::warn!("Periodic folder sync failed for {}: {e}", sync_account.log()),
                    }
                }
                let (e, a) = (self.clone(), sync_account.clone());
                tokio::spawn(async move { e.sync_other_folders(&a).await });
                self.st.lock().pending_flag_sync.remove(account.id());
                let (e, a) = (self.clone(), sync_account.clone());
                tokio::spawn(async move { e.sync_flags_for_range_after(&a).await });
                let (e, a) = (self.clone(), sync_account.clone());
                tokio::spawn(async move {
                    if let Err(err) = e.reconcile_deletes(&a).await {
                        tracing::error!("Reconcile error for {}: {err}", a.log());
                    }
                });
            }
            Err(e) => {
                tracing::error!("Sync error for {}: {e}", account.log());
                if e.0.contains("THROTTLED") || e.0.contains("throttl") {
                    self.st.lock().throttle_skips.insert(account.id().to_owned(), 4);
                }
                if is_connection_refusal(&e.0) {
                    self.note_connection_refusal(&account);
                    self.record_account_error(&account, &e.0).await;
                }
                if !used_fresh {
                    self.drop_persistent(account.id(), &p);
                }
            }
        }
        self.finish_tick(&account, syncing);
    }

    fn finish_tick(self: &Arc<Self>, account: &Account, syncing: SetGuard) {
        drop(syncing);
        let requeue = {
            let mut st = self.st.lock();
            st.sync_started_at.remove(account.id());
            st.pending_inbox_sync.remove(account.id()) && st.connections.contains_key(account.id())
        };
        if requeue {
            let (e, a) = (self.clone(), account.clone());
            tokio::spawn(async move {
                if e.is_connected(a.id()) {
                    e.sync_tick(a).await;
                }
            });
        }
    }

    /// `syncNow`: a manual refresh of one or all of the user's accounts.
    pub async fn sync_now(self: &Arc<Self>, user_id: &str, account_id: Option<&str>) {
        let rows = self
            .db
            .json_rows(
                "SELECT * FROM email_accounts WHERE user_id = $1 AND enabled = true AND protocol = $2",
                params![user_id, "imap"],
            )
            .await
            .unwrap_or_default();
        let accounts: Vec<Account> = rows
            .into_iter()
            .filter_map(|r| match r {
                Value::Object(m) => Some(Account::from_map(m)),
                _ => None,
            })
            .filter(|a| account_id.is_none_or(|id| a.id() == id))
            .collect();
        futures_util::future::join_all(accounts.into_iter().map(|account| {
            let e = self.clone();
            async move {
                if e.st.lock().syncing.contains(account.id()) {
                    tracing::info!("syncNow: {} already syncing, skipping", account.log());
                    return;
                }
                let Some(p) = e.persistent(account.id()) else {
                    tracing::info!("syncNow: {} not connected, reconnecting", account.log());
                    e.connect_account(account).await;
                    return;
                };
                let Some(_syncing) = e.claim(|s| &mut s.syncing, account.id()) else { return };
                e.st.lock().sync_started_at.insert(account.id().to_owned(), now_ms());
                let fresh = account.profile().fresh_inbox_sync;
                let r = if fresh {
                    e.sync_inbox_with_fresh_login(&account).await.map(|_| ())
                } else {
                    let work = async {
                        let mut g = p.lock().await;
                        let c = g.client()?;
                        e.sync_messages(&account, c, "INBOX", 20, false, true).await.map(|_| ())
                    };
                    tokio::select! {
                        r = work => r,
                        _ = p.closed() => Err(EngineError("Connection closed".into())),
                    }
                };
                match r {
                    Ok(()) => tracing::info!("syncNow complete: {}", account.log()),
                    Err(err) => {
                        tracing::error!("syncNow error for {}: {err}", account.log());
                        if !fresh {
                            e.drop_persistent(account.id(), &p);
                        }
                    }
                }
                e.st.lock().sync_started_at.remove(account.id());
            }
        }))
        .await;
        self.broadcast(json!({ "type": "sync_complete", "accountId": account_id }), Some(user_id));
    }

    /// `syncFoldersNow`.
    pub async fn sync_folders_now(self: &Arc<Self>, user_id: &str, account_id: Option<&str>) {
        let rows = self
            .db
            .json_rows(
                "SELECT * FROM email_accounts WHERE user_id = $1 AND enabled = true AND protocol = $2",
                params![user_id, "imap"],
            )
            .await
            .unwrap_or_default();
        let accounts: Vec<Account> = rows
            .into_iter()
            .filter_map(|r| match r {
                Value::Object(m) => Some(Account::from_map(m)),
                _ => None,
            })
            .filter(|a| account_id.is_none_or(|id| a.id() == id))
            .collect();
        futures_util::future::join_all(accounts.into_iter().map(|account| {
            let e = self.clone();
            async move {
                let r: EResult<()> = match e.persistent(account.id()) {
                    None => {
                        tracing::info!("syncFoldersNow: {} not connected, reconnecting", account.log());
                        e.connect_account(account.clone()).await;
                        Ok(())
                    }
                    Some(p) => {
                        let mut g = p.lock().await;
                        match g.client() {
                            Ok(c) => {
                                race(20_000, "Manual folder sync", async { e.sync_folders(&account, c).await }).await
                            }
                            Err(err) => Err(err),
                        }
                    }
                };
                match r {
                    Ok(()) => {
                        e.st.lock().last_folder_sync_at.insert(account.id().to_owned(), now_ms());
                        e.broadcast(
                            json!({ "type": "folders_synced", "accountId": account.id() }),
                            Some(account.user_id()),
                        );
                    }
                    Err(err) => tracing::error!("syncFoldersNow error for {}: {err}", account.log()),
                }
            }
        }))
        .await;
    }

    /// `connectAllForUser`: load interval preferences, then connect with provider pacing.
    pub async fn connect_all_for_user(self: &Arc<Self>, user_id: &str) {
        if let Ok(Some(row)) = self.db.opt("SELECT preferences FROM users WHERE id = $1", params![user_id]).await {
            let prefs: Value = row.get::<_, Option<Value>>("preferences").unwrap_or(Value::Null);
            let num =
                |k: &str| prefs.get(k).map(inboxora_mail::text::js_string).and_then(|s| s.trim().parse::<f64>().ok());
            if let Some(sec) = num("syncInterval").filter(|s| (15.0..=120.0).contains(s)) {
                self.st.lock().user_sync_interval_ms.insert(user_id.to_owned(), (sec * 1000.0) as u64);
            }
            if let Some(sec) = num("folderSyncInterval").filter(|s| [0.0, 900.0, 1800.0, 3600.0].contains(s)) {
                self.st.lock().user_folder_sync_interval_ms.insert(user_id.to_owned(), (sec * 1000.0) as u64);
            }
        }
        let rows = self
            .db
            .json_rows(
                "SELECT * FROM email_accounts WHERE user_id = $1 AND enabled = true AND protocol = $2",
                params![user_id, "imap"],
            )
            .await
            .unwrap_or_default();
        let eligible: Vec<Account> = {
            let st = self.st.lock();
            rows.into_iter()
                .filter_map(|r| match r {
                    Value::Object(m) => Some(Account::from_map(m)),
                    _ => None,
                })
                .filter(|a| !st.connections.contains_key(a.id()) && !st.connecting.contains(a.id()))
                .collect()
        };
        if eligible.is_empty() {
            return;
        }
        let staggers: Vec<u64> = eligible.iter().map(|a| connect_stagger_for(a.profile(), eligible.len())).collect();
        let (min, max) = (*staggers.iter().min().unwrap_or(&0), *staggers.iter().max().unwrap_or(&0));
        let total: u64 = staggers.iter().sum();
        let range = if min == max { format!("{min}ms") } else { format!("{min}-{max}ms") };
        tracing::info!(
            "Auto-connecting {} account(s): connect stagger {range}, ~{}s total spread",
            eligible.len(),
            (total as f64 / 1000.0).round()
        );
        let mut delay = 0;
        for (i, account) in eligible.into_iter().enumerate() {
            let e = self.clone();
            let d = delay;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(d)).await;
                e.connect_account(account).await;
            });
            delay += staggers[i];
        }
    }

    pub fn record_warning(&self, code: &str, account_id: &str) {
        record_warning(code, Some(account_id));
    }

    pub fn record_signal(&self, sig: &str, account_id: &str, magnitude: Option<f64>) {
        record_sync_signal(sig, Some(account_id), magnitude);
    }
}

/// imapflow's auto-IDLE: hold IDLE on the persistent connection whenever nobody else needs it.
async fn idle_loop(p: Arc<Persistent>, keepalive: Duration) {
    loop {
        if p.is_closed() {
            if let Some(mut c) = p.client.lock().await.take() {
                c.close();
            }
            return;
        }
        let mut g = p.client.lock().await;
        if p.is_closed() {
            if let Some(mut c) = g.take() {
                c.close();
            }
            return;
        }
        let Some(c) = g.as_mut() else { return };
        if !c.is_usable() {
            return;
        }
        if p.wants.load(Ordering::SeqCst) > 0 {
            drop(g);
            tokio::task::yield_now().await;
            continue;
        }
        let notified = p.wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if p.wants.load(Ordering::SeqCst) > 0 || p.is_closed() {
            continue;
        }
        if c.mailbox().is_none()
            && let Err(e) = c.select("INBOX").await
        {
            tracing::warn!("IDLE select failed: {e}");
            if e.is_fatal() {
                return;
            }
            drop(g);
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        if let Err(e) = c.idle(keepalive, notified).await {
            if e.is_fatal() {
                return;
            }
            tracing::debug!("IDLE ended with {e}");
            drop(g);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

fn rand_below(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let b = inboxora_core::crypto::random_bytes::<8>();
    u64::from_le_bytes(b) % n
}
