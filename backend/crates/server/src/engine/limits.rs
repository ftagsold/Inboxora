//! Per-account IMAP connection budget. Every socket an account opens (persistent IDLE, pool,
//! temporary, background) holds a [`Permit`]; at the limit, openers queue by [`Prio`] so the UI
//! and INBOX are served before Archive, Sent, Trash … and the cache preload.
//!
//! The limit starts at the server's start value (admin-set, else `IMAP_MAX_CONNECTIONS_PER_ACCOUNT`,
//! default 10) and is only ever lowered: a refused connect or a socket the server drops shortly
//! after login steps it down by one (at most every 30 s, never below 2). The learned value is
//! stored on the account so it survives restarts; an admin resets it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// Fewest connections an account can work with: the persistent IDLE one plus one more.
pub const LIMIT_FLOOR: usize = 2;
/// Largest start value an admin can set.
pub const LIMIT_MAX: usize = 50;
pub const DEFAULT_START_LIMIT: usize = 10;
/// Pool connections are capped here even when the limit allows more.
pub const POOL_MAX: usize = 4;
/// Minimum spacing between two automatic step-downs.
const LOWER_SPACING: Duration = Duration::from_secs(30);
/// A socket the server closes this soon after login counts as a limit refusal.
pub const EARLY_DROP: Duration = Duration::from_secs(5);

/// Queue rank of a connection request; lower is served first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Prio {
    Ui = 0,
    /// The blocking cold cache preload: the UI is waiting on it.
    PreloadCold = 1,
    Inbox = 2,
    Archive = 3,
    Sent = 4,
    Trash = 5,
    Drafts = 6,
    Junk = 7,
    Other = 8,
    /// Background preload of misses after a sync.
    Preload = 9,
}

/// What a connection is used for (diagnostics only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Persistent = 0,
    Pool = 1,
    Temporary = 2,
    Background = 3,
    Preload = 4,
}

const KINDS: [&str; 5] = ["persistent", "pool", "temporary", "background", "preload"];

/// The folder rank used for sync and preload ordering: INBOX, Archive, Sent, Trash, Drafts,
/// Junk, then everything else. `role` is the account's configured mapping or the folder's
/// SPECIAL-USE attribute.
pub fn folder_prio(path: &str, role: Option<&str>) -> Prio {
    if path.eq_ignore_ascii_case("INBOX") {
        return Prio::Inbox;
    }
    match role.map(|r| r.trim_start_matches('\\').to_ascii_lowercase()).as_deref() {
        Some("archive" | "all") => Prio::Archive,
        Some("sent") => Prio::Sent,
        Some("trash") => Prio::Trash,
        Some("drafts") => Prio::Drafts,
        Some("junk" | "spam") => Prio::Junk,
        _ => Prio::Other,
    }
}

/// The limit an account runs with.
pub fn effective_limit(start: usize, learned: Option<usize>, auto_lower: bool) -> usize {
    let start = start.clamp(LIMIT_FLOOR, LIMIT_MAX);
    if auto_lower { learned.map_or(start, |l| l.clamp(LIMIT_FLOOR, start)) } else { start }
}

/// The next limit after a refusal seen with `observed` connections open (the failing one
/// included), or `None` when it would not go down.
pub fn stepped_down(limit: usize, observed: usize) -> Option<usize> {
    let next = (limit.saturating_sub(1)).min(observed.saturating_sub(1)).max(LIMIT_FLOOR);
    (next < limit).then_some(next)
}

/// Whether an error text means the server refused a connection for being over its limit
/// (Dovecot "Maximum number of connections from user+IP exceeded", "Too many simultaneous
/// connections", …). A plain dead socket or timeout is not one: those must not ratchet the
/// limit down.
pub fn is_limit_signal(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    d.contains("too many")
        || d.contains("simultaneous")
        || (d.contains("connection") && (d.contains("limit") || d.contains("maximum")))
}

#[derive(Debug, Clone)]
pub struct Config {
    pub start: usize,
    pub learned: Option<usize>,
    pub auto_lower: bool,
}

struct St {
    cfg: Config,
    limit: usize,
    open: [usize; 5],
    total: usize,
    seq: u64,
    waiters: BTreeMap<(Prio, u64), oneshot::Sender<()>>,
    last_lower: Option<Instant>,
    last_reason: Option<String>,
    last_lowered_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub struct Limiter {
    st: Mutex<St>,
}

/// One counted connection; dropping it frees the slot for the next waiter.
pub struct Permit {
    limiter: Arc<Limiter>,
    kind: Kind,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.limiter.release(self.kind);
    }
}

/// A queued request; removes itself (or returns a slot granted too late) when dropped.
struct Waiting {
    limiter: Arc<Limiter>,
    key: (Prio, u64),
    rx: Option<oneshot::Receiver<()>>,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let Some(mut rx) = self.rx.take() else { return };
        let removed = self.limiter.st.lock().waiters.remove(&self.key).is_some();
        if !removed && rx.try_recv().is_ok() {
            // Granted after the caller gave up: the slot is ours, hand it on.
            self.limiter.release_slot();
        }
    }
}

impl Limiter {
    pub fn new(cfg: Config) -> Arc<Self> {
        let limit = effective_limit(cfg.start, cfg.learned, cfg.auto_lower);
        Arc::new(Self {
            st: Mutex::new(St {
                cfg,
                limit,
                open: [0; 5],
                total: 0,
                seq: 0,
                waiters: BTreeMap::new(),
                last_lower: None,
                last_reason: None,
                last_lowered_at: None,
            }),
        })
    }

    pub fn limit(&self) -> usize {
        self.st.lock().limit
    }

    pub fn total(&self) -> usize {
        self.st.lock().total
    }

    /// Pool connections this account may hold: everything but the persistent one, capped.
    pub fn pool_size(&self) -> usize {
        (self.limit().saturating_sub(1)).clamp(1, POOL_MAX)
    }

    pub fn try_acquire(self: &Arc<Self>, kind: Kind) -> Option<Permit> {
        let mut s = self.st.lock();
        if s.total < s.limit && s.waiters.is_empty() {
            s.total += 1;
            s.open[kind as usize] += 1;
            return Some(Permit { limiter: self.clone(), kind });
        }
        None
    }

    /// Wait for a slot in priority order.
    pub async fn acquire(self: &Arc<Self>, prio: Prio, kind: Kind) -> Permit {
        let (key, rx) = {
            let mut s = self.st.lock();
            let jump = s.waiters.keys().next().is_none_or(|(p, _)| prio < *p);
            if s.total < s.limit && jump {
                s.total += 1;
                s.open[kind as usize] += 1;
                return Permit { limiter: self.clone(), kind };
            }
            s.seq += 1;
            let key = (prio, s.seq);
            let (tx, rx) = oneshot::channel();
            s.waiters.insert(key, tx);
            (key, rx)
        };
        let mut w = Waiting { limiter: self.clone(), key, rx: Some(rx) };
        let rx = w.rx.as_mut().expect("receiver");
        // The sender lives in the queue until it grants; a closed channel cannot happen while
        // the waiter is queued, so a grant is the only outcome.
        let _ = rx.await;
        w.rx = None;
        self.st.lock().open[kind as usize] += 1;
        Permit { limiter: self.clone(), kind }
    }

    pub fn queued(&self) -> usize {
        self.st.lock().waiters.len()
    }

    fn release(&self, kind: Kind) {
        {
            let mut s = self.st.lock();
            s.open[kind as usize] = s.open[kind as usize].saturating_sub(1);
        }
        self.release_slot();
    }

    /// Give a freed slot to the best waiter, or return it.
    fn release_slot(&self) {
        let mut s = self.st.lock();
        s.total = s.total.saturating_sub(1);
        Self::grant(&mut s);
    }

    fn grant(s: &mut St) {
        while s.total < s.limit {
            let Some((_, tx)) = s.waiters.pop_first() else { break };
            if tx.send(()).is_ok() {
                s.total += 1;
            }
        }
    }

    /// Apply new settings (admin change or reset); waiters are let in when the limit rises.
    pub fn configure(&self, cfg: Config) {
        let mut s = self.st.lock();
        s.limit = effective_limit(cfg.start, cfg.learned, cfg.auto_lower);
        s.cfg = cfg;
        if s.cfg.learned.is_none() {
            s.last_lower = None;
        }
        Self::grant(&mut s);
    }

    /// Record a refusal seen with `observed` connections open; returns the new limit when it
    /// went down.
    pub fn lower(&self, observed: usize, reason: &str) -> Option<usize> {
        let mut s = self.st.lock();
        if !s.cfg.auto_lower || s.last_lower.is_some_and(|t| t.elapsed() < LOWER_SPACING) {
            return None;
        }
        let next = stepped_down(s.limit, observed)?;
        s.limit = next;
        s.cfg.learned = Some(next);
        s.last_lower = Some(Instant::now());
        s.last_reason = Some(reason.to_owned());
        s.last_lowered_at = Some(chrono::Utc::now());
        Some(next)
    }

    pub fn snapshot(&self) -> Value {
        let s = self.st.lock();
        let open: serde_json::Map<String, Value> =
            KINDS.iter().enumerate().map(|(i, k)| ((*k).to_owned(), json!(s.open[i]))).collect();
        json!({
            "limit": s.limit,
            "startLimit": s.cfg.start,
            "learnedLimit": s.cfg.learned,
            "autoLower": s.cfg.auto_lower,
            "open": s.total,
            "openByKind": open,
            "queued": s.waiters.len(),
            "lastLoweredAt": s.last_lowered_at.map(|t| t.to_rfc3339()),
            "lastReason": s.last_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(start: usize) -> Config {
        Config { start, learned: None, auto_lower: true }
    }

    #[test]
    fn folder_order() {
        let order = [
            folder_prio("INBOX", None),
            folder_prio("Archiv", Some("\\Archive")),
            folder_prio("Gesendet", Some("sent")),
            folder_prio("Papierkorb", Some("\\Trash")),
            folder_prio("Entwürfe", Some("\\Drafts")),
            folder_prio("Spam", Some("spam")),
            folder_prio("Projects", None),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        assert!(Prio::Ui < Prio::PreloadCold && Prio::PreloadCold < Prio::Inbox && Prio::Other < Prio::Preload);
    }

    #[test]
    fn effective_and_step_down() {
        assert_eq!(effective_limit(10, None, true), 10);
        assert_eq!(effective_limit(10, Some(6), true), 6);
        assert_eq!(effective_limit(10, Some(6), false), 10);
        assert_eq!(effective_limit(1, None, true), LIMIT_FLOOR);
        assert_eq!(effective_limit(5, Some(9), true), 5);
        assert_eq!(stepped_down(10, 11), Some(9));
        assert_eq!(stepped_down(10, 6), Some(5));
        assert_eq!(stepped_down(3, 1), Some(2));
        assert_eq!(stepped_down(2, 5), None);
        assert!(is_limit_signal("Too many simultaneous connections"));
        assert!(is_limit_signal("NO [LIMIT] Connection limit reached"));
        assert!(is_limit_signal(
            "Maximum number of connections from user+IP exceeded (mail_max_userip_connections=10)"
        ));
        assert!(!is_limit_signal("Authentication failed"));
        assert!(!is_limit_signal("Connection not available"));
        assert!(!is_limit_signal("IMAP connect timeout (30000ms)"));
    }

    #[test]
    fn lowering_is_spaced_and_floored() {
        let l = Limiter::new(cfg(10));
        assert_eq!(l.lower(11, "refused"), Some(9));
        assert_eq!(l.lower(11, "refused"), None, "second step within 30s");
        let l = Limiter::new(Config { start: 10, learned: None, auto_lower: false });
        assert_eq!(l.lower(11, "refused"), None, "auto-lower off");
        let l = Limiter::new(Config { start: 10, learned: Some(2), auto_lower: true });
        assert_eq!(l.lower(3, "refused"), None, "floor");
    }

    #[tokio::test]
    async fn queue_serves_priority_order() {
        let l = Limiter::new(cfg(2));
        let a = l.acquire(Prio::Inbox, Kind::Persistent).await;
        let b = l.acquire(Prio::Ui, Kind::Pool).await;
        assert_eq!(l.total(), 2);
        assert!(l.try_acquire(Kind::Temporary).is_none());
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for p in [Prio::Preload, Prio::Trash, Prio::Ui, Prio::Sent] {
            let (l, order) = (l.clone(), order.clone());
            tasks.push(tokio::spawn(async move {
                let permit = l.acquire(p, Kind::Background).await;
                order.lock().push(p);
                drop(permit);
            }));
            tokio::task::yield_now().await;
        }
        while l.queued() < 4 {
            tokio::task::yield_now().await;
        }
        drop(a);
        for t in tasks {
            t.await.unwrap();
        }
        drop(b);
        assert_eq!(*order.lock(), vec![Prio::Ui, Prio::Sent, Prio::Trash, Prio::Preload]);
        assert_eq!(l.total(), 0);
    }

    #[tokio::test]
    async fn cancelled_waiter_frees_its_place() {
        let l = Limiter::new(cfg(2));
        let _a = l.acquire(Prio::Inbox, Kind::Persistent).await;
        let b = l.acquire(Prio::Ui, Kind::Pool).await;
        let waiting = tokio::time::timeout(Duration::from_millis(20), l.acquire(Prio::Other, Kind::Background)).await;
        assert!(waiting.is_err());
        assert_eq!(l.queued(), 0);
        drop(b);
        assert_eq!(l.total(), 1);
        assert!(l.try_acquire(Kind::Pool).is_some());
    }

    #[tokio::test]
    async fn raising_the_limit_lets_waiters_in() {
        let l = Limiter::new(Config { start: 10, learned: Some(2), auto_lower: true });
        let _a = l.acquire(Prio::Inbox, Kind::Persistent).await;
        let _b = l.acquire(Prio::Ui, Kind::Pool).await;
        let l2 = l.clone();
        let t = tokio::spawn(async move { l2.acquire(Prio::Other, Kind::Background).await });
        while l.queued() < 1 {
            tokio::task::yield_now().await;
        }
        l.configure(Config { start: 10, learned: None, auto_lower: true });
        let p = t.await.unwrap();
        assert_eq!(l.total(), 3);
        drop(p);
        assert_eq!(l.limit(), 10);
        assert_eq!(l.pool_size(), POOL_MAX);
    }
}
