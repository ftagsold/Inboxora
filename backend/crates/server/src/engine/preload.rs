//! Meta preload and cache maintenance. Redis runs memory-only, so after every Redis (or server)
//! start each account's display fields are loaded for every folder before its user sees the
//! mail UI (the "cold" preload the client blocks on); after each sync only missing ids are
//! fetched in the background. Deleted rows leave tombstones, from which the janitor drops the
//! cached entries; a daily sweep removes whatever a missed tombstone left behind.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fred::prelude::*;
use inboxora_core::params;
use inboxora_core::pg::Q;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::cache::{Location, META_VERSION, body_key, full_key, meta_key, prefix};
use super::connect::Account;
use super::limits::{Prio, folder_prio};
use super::policy::skips_folder;
use super::{EResult, Engine, now_ms};

/// Marks a Redis that holds this server's preloaded cache; its absence means Redis restarted.
const SENTINEL: &str = "ibx:mc:sentinel";
/// Rows checked against the cache per MGET.
const CHECK_CHUNK: usize = 1000;
/// UIDs fetched per preload step (one progress update each): the step starts at `FETCH_STEP`,
/// halves after a timeout (some servers need well over a second per message) down to
/// `FETCH_STEP_MIN` and doubles again after a success up to `FETCH_STEP_MAX`.
const FETCH_STEP: usize = 200;
const FETCH_STEP_MIN: usize = 5;
const FETCH_STEP_MAX: usize = 200;
/// How long a folder waits for its backfill (both scanning one mailbox slows the server down).
const BACKFILL_WAIT: Duration = Duration::from_secs(600);
/// Minimum spacing of progress events per account.
const PROGRESS_EVERY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Done,
    Failed,
}

/// Progress of an account's cold preload (absent: not started yet).
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub state: State,
    pub done: u64,
    pub total: u64,
    /// Messages in the account (`total` counts them twice while indexing comes first).
    pub messages: u64,
    pub folder: Option<String>,
    pub error: Option<String>,
    #[serde(skip)]
    pub updated_ms: u64,
}

impl Engine {
    /// The account's selectable folders in sync/preload order: INBOX (when asked), Archive,
    /// Sent, Trash, Drafts, Junk, then the rest by size, smallest first.
    pub async fn folders_by_priority(&self, account: &Account, include_inbox: bool) -> EResult<Vec<(String, Prio)>> {
        let profile = account.profile();
        let rows = self
            .db
            .rows(
                "SELECT path, special_use, COALESCE(total_count, 0) AS total FROM folders WHERE account_id = $1 AND no_select = false",
                params![account.id()],
            )
            .await?;
        let mapped: HashMap<String, &str> = ["archive", "sent", "trash", "drafts", "spam"]
            .into_iter()
            .filter_map(|role| account.mapping(role).map(|p| (p.to_owned(), role)))
            .collect();
        let mut folders: Vec<(String, Prio, i32)> = rows
            .iter()
            .map(|r| {
                let path: String = r.get("path");
                let special: Option<String> = r.get("special_use");
                let role = mapped.get(&path).copied().or(special.as_deref());
                let prio = folder_prio(&path, role);
                (path, prio, r.get::<_, i32>("total"))
            })
            .filter(|(p, prio, _)| (include_inbox || *prio != Prio::Inbox) && !skips_folder(profile, p))
            .collect();
        folders.sort_by(|a, b| (a.1, a.2, &a.0).cmp(&(b.1, b.2, &b.0)));
        Ok(folders.into_iter().map(|(p, prio, _)| (p, prio)).collect())
    }

    pub fn preload_status(&self, account_id: &str) -> Option<Status> {
        self.preload.lock().get(account_id).cloned()
    }

    fn set_preload(&self, account: &Account, f: impl FnOnce(&mut Status), force_event: bool) {
        let snapshot = {
            let mut map = self.preload.lock();
            let s = map.entry(account.id().to_owned()).or_insert(Status {
                state: State::Running,
                done: 0,
                total: 0,
                messages: 0,
                folder: None,
                error: None,
                updated_ms: 0,
            });
            f(s);
            let due = force_event || now_ms().saturating_sub(s.updated_ms) >= PROGRESS_EVERY.as_millis() as u64;
            if !due {
                return;
            }
            s.updated_ms = now_ms();
            s.clone()
        };
        self.broadcast(
            json!({
                "type": "meta_preload",
                "accountId": account.id(),
                "state": snapshot.state,
                "done": snapshot.done,
                "total": snapshot.total,
                "messages": snapshot.messages,
                "folder": snapshot.folder,
                "error": snapshot.error,
            }),
            Some(account.user_id()),
        );
    }

    /// Mark an account's cold preload failed (it could not connect), so its user is not blocked.
    pub fn preload_failed(&self, account: &Account, error: &str) {
        let cold = !matches!(self.preload_status(account.id()), Some(Status { state: State::Done, .. }));
        if cold {
            self.set_preload(
                account,
                |s| {
                    s.state = State::Failed;
                    s.error = Some(error.to_owned());
                },
                true,
            );
        }
    }

    /// Load the display fields of every indexed message of `account` into the cache. A cold run
    /// reports progress (the UI is blocked on it) and queues right after the UI for connections;
    /// a warm run fetches only misses at the lowest priority.
    pub async fn preload_account(self: &Arc<Self>, account: &Account, cold: bool) {
        if self.cache.redis().is_none() {
            if cold {
                self.set_preload(account, |s| s.state = State::Done, true);
            }
            return;
        }
        let key = format!("{}:{}", account.id(), if cold { "cold" } else { "warm" });
        let Some(_running) = self.claim(|s| &mut s.preload_running, &key) else { return };
        let started = Instant::now();
        if cold {
            // Block the user's UI right away: indexing and the row query can take a while
            // before the first progress event.
            self.set_preload(
                account,
                |s| {
                    s.state = State::Running;
                    s.error = None;
                },
                true,
            );
        }
        let indexed = cold && self.wait_for_initial_backfill(account).await;
        let prio = if cold { Prio::PreloadCold } else { Prio::Preload };
        let r = self.preload_inner(account, cold, prio, indexed).await;
        match r {
            Ok((total, fetched, failed)) => {
                if cold {
                    let error = (failed > 0).then(|| format!("{failed} messages could not be loaded"));
                    self.set_preload(
                        account,
                        |s| {
                            s.state = State::Done;
                            s.done = s.total;
                            s.folder = None;
                            s.error = error;
                        },
                        true,
                    );
                }
                if fetched > 0 || cold {
                    tracing::info!(
                        "[preload] {} {} rows, {fetched} fetched, {failed} failed in {}ms ({})",
                        account.log(),
                        total,
                        started.elapsed().as_millis(),
                        if cold { "cold" } else { "warm" }
                    );
                }
            }
            Err(e) => {
                tracing::warn!("[preload] {} failed: {e}", account.log());
                if cold {
                    self.preload_failed(account, &e.0);
                }
            }
        }
    }

    /// A new account is indexed (backfilled) first; its user waits on the preload screen with
    /// the indexing progress (rows indexed of the server's message count) instead of opening
    /// empty folders. Indexing fills the first half of the one progress bar, loading the
    /// display fields the second half. Returns whether indexing ran.
    async fn wait_for_initial_backfill(&self, account: &Account) -> bool {
        // The backfill task is spawned just before this preload; give it a moment to claim.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut ran = false;
        while self.st.lock().backfill_all_running.contains(account.id()) {
            ran = true;
            let counts = self
                .db
                .opt(
                    "SELECT (SELECT COUNT(*) FROM messages WHERE account_id = $1)::bigint AS done, \
                       (SELECT COALESCE(SUM(total_count), 0) FROM folders WHERE account_id = $1)::bigint AS total",
                    params![account.id()],
                )
                .await
                .ok()
                .flatten();
            if let Some(r) = counts {
                let (done, total) = (r.get::<_, i64>("done").max(0) as u64, r.get::<_, i64>("total").max(0) as u64);
                self.set_preload(
                    account,
                    |s| {
                        let total = total.max(done);
                        s.state = State::Running;
                        s.total = total * 2;
                        s.done = done;
                        s.messages = total;
                        s.folder = None;
                    },
                    false,
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        ran
    }

    async fn preload_inner(
        self: &Arc<Self>,
        account: &Account,
        cold: bool,
        prio: Prio,
        indexed: bool,
    ) -> EResult<(u64, u64, u64)> {
        let rows = self
            .db
            .rows(
                "SELECT id, folder, uid FROM messages WHERE account_id = $1 AND is_deleted = false ORDER BY date DESC",
                params![account.id()],
            )
            .await?;
        let mut by_folder: HashMap<String, Vec<Location>> = HashMap::new();
        for r in &rows {
            let folder: String = r.get("folder");
            by_folder.entry(folder.clone()).or_default().push(Location {
                id: r.get::<_, Uuid>("id"),
                account_id: account.id().to_owned(),
                folder,
                uid: r.get::<_, i64>("uid"),
            });
        }
        let total = rows.len() as u64;
        // After indexing, this pass is the second half of the bar.
        let base = if indexed { total } else { 0 };
        if cold {
            self.set_preload(
                account,
                |s| {
                    *s = Status {
                        state: State::Running,
                        done: base,
                        total: base + total,
                        messages: total,
                        folder: None,
                        error: None,
                        updated_ms: 0,
                    };
                },
                true,
            );
        }
        let mut order: Vec<(String, Prio)> = self.folders_by_priority(account, true).await?;
        let mut rest: Vec<String> = by_folder.keys().filter(|f| !order.iter().any(|(p, _)| p == *f)).cloned().collect();
        rest.sort();
        order.extend(rest.into_iter().map(|f| (f, Prio::Other)));
        let (mut done, mut fetched, mut failed) = (0u64, 0u64, 0u64);
        let mut step = FETCH_STEP;
        // A folder being backfilled goes to the back of the queue until its backfill is done.
        let mut queue: std::collections::VecDeque<(String, Option<Instant>)> =
            order.into_iter().map(|(f, _)| (f, None)).collect();
        while let Some((folder, waiting_since)) = queue.pop_front() {
            let backfilling = self.st.lock().backfill_running.contains(&format!("{}:{folder}", account.id()));
            let since = waiting_since.unwrap_or_else(Instant::now);
            if backfilling && since.elapsed() < BACKFILL_WAIT {
                let all_waiting = queue.iter().all(|(_, w)| w.is_some());
                queue.push_back((folder, Some(since)));
                if all_waiting {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                continue;
            }
            let Some(locs) = by_folder.remove(&folder) else { continue };
            if cold {
                self.set_preload(account, |s| s.folder = Some(folder.clone()), false);
            }
            for chunk in locs.chunks(CHECK_CHUNK) {
                let keys: Vec<String> = chunk.iter().map(|l| meta_key(&l.id)).collect();
                let hits: Vec<Option<Value>> = self.cache.get_many(&keys).await;
                let misses: Vec<&Location> =
                    chunk.iter().zip(hits).filter(|(_, h)| h.is_none()).map(|(l, _)| l).collect();
                done += (chunk.len() - misses.len()) as u64;
                let mut i = 0;
                while i < misses.len() {
                    let part = &misses[i..(i + step).min(misses.len())];
                    match self.fetch_meta_group(account.id(), &folder, part, prio).await {
                        Ok(found) => {
                            fetched += found.len() as u64;
                            failed += (part.len() - found.len()) as u64;
                            step = (step * 2).min(FETCH_STEP_MAX);
                        }
                        Err(e) if e.0.to_ascii_lowercase().contains("timeout") && step > FETCH_STEP_MIN => {
                            step = (step / 2).max(FETCH_STEP_MIN);
                            tracing::warn!("[preload] {}/{folder}: {e} — retrying {step} at a time", account.log());
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!("[preload] {}/{folder}: {e}", account.log());
                            failed += part.len() as u64;
                        }
                    }
                    i += part.len();
                    done += part.len() as u64;
                    if cold {
                        self.set_preload(account, |s| s.done = base + done, false);
                    }
                }
                if cold {
                    self.set_preload(account, |s| s.done = base + done, false);
                }
            }
        }
        Ok((total, fetched, failed))
    }

    /// After a sync: fetch display fields of new rows in the background (right away when mail
    /// arrived, else at most every five minutes). Waits for the account's cold preload.
    pub fn warm_preload(self: &Arc<Self>, account: &Account, arrived: bool) {
        if !matches!(self.preload_status(account.id()), Some(Status { state: State::Done | State::Failed, .. })) {
            return;
        }
        {
            let mut st = self.st.lock();
            let last = st.last_warm_preload.get(account.id()).copied().unwrap_or(0);
            if !arrived && now_ms().saturating_sub(last) < 300_000 {
                return;
            }
            st.last_warm_preload.insert(account.id().to_owned(), now_ms());
        }
        let (e, a) = (self.clone(), account.clone());
        tokio::spawn(async move { e.preload_account(&a, false).await });
    }

    /// Start the cold preload of a freshly connected account (unless one already finished).
    pub fn cold_preload(self: &Arc<Self>, account: &Account) {
        if matches!(self.preload_status(account.id()), Some(Status { state: State::Done, .. })) {
            self.warm_preload(account, true);
            return;
        }
        let (e, a) = (self.clone(), account.clone());
        tokio::spawn(async move { e.preload_account(&a, true).await });
    }

    /// Detect a Redis that lost the cache (restart, flush): mark every account cold and preload
    /// again. Called at startup and from the maintenance timer.
    pub async fn check_cache_sentinel(self: &Arc<Self>) {
        let Some(redis) = self.cache.redis() else { return };
        let present: Result<Option<String>, _> = redis.get(SENTINEL).await;
        match present {
            Ok(Some(_)) => {}
            Ok(None) => {
                let _: Result<(), _> = redis.set(SENTINEL, now_ms().to_string(), None, None, false).await;
                let had_status = !self.preload.lock().is_empty();
                if had_status {
                    tracing::warn!("[preload] Redis cache is empty — preloading every account again");
                    self.preload.lock().clear();
                    let ids: Vec<String> = self.st.lock().connections.keys().cloned().collect();
                    for id in ids {
                        if let Ok(Some(account)) = super::connect::load_account(&self.db, &id).await {
                            let e = self.clone();
                            tokio::spawn(async move { e.preload_account(&account, true).await });
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("[preload] sentinel check failed: {e}"),
        }
    }

    /// Drop cached entries of deleted rows (from their tombstones) and prune old tombstones.
    pub async fn tombstone_janitor(&self) -> EResult<()> {
        loop {
            let rows = self
                .db
                .rows(
                    "SELECT seq, id FROM message_tombstones WHERE NOT cache_cleared ORDER BY seq LIMIT 2000",
                    params![],
                )
                .await?;
            if rows.is_empty() {
                break;
            }
            let ids: Vec<Uuid> = rows.iter().map(|r| r.get("id")).collect();
            let seqs: Vec<i64> = rows.iter().map(|r| r.get("seq")).collect();
            let keys: Vec<String> = ids.iter().flat_map(|id| [meta_key(id), body_key(id)]).collect();
            self.cache.forget(keys).await;
            self.db
                .exec("UPDATE message_tombstones SET cache_cleared = true WHERE seq = ANY($1)", params![seqs])
                .await?;
            if rows.len() < 2000 {
                break;
            }
        }
        self.db
            .exec(
                "DELETE FROM message_tombstones WHERE cache_cleared AND deleted_at < NOW() - INTERVAL '1 day'",
                params![],
            )
            .await?;
        Ok(())
    }

    /// Remove cached display fields whose row no longer exists or whose format is outdated.
    pub async fn orphan_sweep(&self) -> EResult<u64> {
        let Some(redis) = self.cache.redis() else { return Ok(0) };
        let pattern = format!("{}h:*", prefix());
        let current = format!("{}h:{META_VERSION}:", prefix());
        let mut cursor = "0".to_owned();
        let mut removed = 0u64;
        loop {
            let (next, keys): (String, Vec<String>) = redis
                .next()
                .scan_page(cursor.clone(), pattern.clone(), Some(1000), None)
                .await
                .map_err(|e| super::EngineError(e.to_string()))?;
            let mut stale: Vec<String> = keys.iter().filter(|k| !k.starts_with(&current)).cloned().collect();
            let ids: Vec<Uuid> = keys.iter().filter_map(|k| k.strip_prefix(&current)?.parse().ok()).collect();
            if !ids.is_empty() {
                let present: std::collections::HashSet<Uuid> = self
                    .db
                    .rows("SELECT id FROM messages WHERE id = ANY($1::uuid[])", params![ids.clone()])
                    .await?
                    .iter()
                    .map(|r| r.get("id"))
                    .collect();
                stale.extend(ids.iter().filter(|id| !present.contains(id)).map(|id| full_key(&meta_key(id))));
            }
            if !stale.is_empty() {
                removed += stale.len() as u64;
                let _: Result<i64, _> = redis.del(stale).await;
            }
            if next == "0" {
                break;
            }
            cursor = next;
        }
        Ok(removed)
    }

    /// Warn when the cache fills Redis: display fields never expire, so a full Redis would
    /// evict sessions and then refuse writes.
    pub async fn check_redis_memory(&self) {
        let Some(redis) = self.cache.redis() else { return };
        let info: Result<String, _> = redis.next().info(Some(fred::types::InfoKind::Memory)).await;
        let Ok(info) = info else { return };
        let field = |name: &str| {
            info.lines().find_map(|l| l.strip_prefix(&format!("{name}:"))).and_then(|v| v.trim().parse::<u64>().ok())
        };
        if let (Some(used), Some(max)) = (field("used_memory"), field("maxmemory"))
            && max > 0
            && used * 100 / max >= 80
        {
            tracing::warn!(
                "[mail-cache] Redis memory at {}% of maxmemory ({} / {} MB): raise REDIS_MAXMEMORY",
                used * 100 / max,
                used / 1_048_576,
                max / 1_048_576
            );
        }
    }

    /// Status of every enabled IMAP account of a user for the preload screen. An account the
    /// engine has not reached within five minutes of start counts as failed, so a stuck account
    /// never blocks its user.
    pub async fn preload_overview(&self, user_id: &str) -> EResult<Value> {
        let rows = self
            .db
            .rows(
                "SELECT id, email_address FROM email_accounts WHERE user_id = $1 AND enabled = true AND protocol = 'imap' ORDER BY sort_order NULLS LAST, created_at",
                params![user_id],
            )
            .await?;
        let grace = self.started.elapsed() > Duration::from_secs(300);
        let mut ready = true;
        let accounts: Vec<Value> = rows
            .iter()
            .map(|r| {
                let id = r.get::<_, Uuid>("id").to_string();
                let email: Option<String> = r.get("email_address");
                let st = self.preload_status(&id);
                let state = match &st {
                    Some(s) => serde_json::to_value(s.state).unwrap_or(Value::Null),
                    None if grace => json!("failed"),
                    None => json!("pending"),
                };
                if state == json!("running") || state == json!("pending") {
                    ready = false;
                }
                json!({
                    "id": id,
                    "email": email,
                    "state": state,
                    "done": st.as_ref().map_or(0, |s| s.done),
                    "total": st.as_ref().map_or(0, |s| s.total),
                    "messages": st.as_ref().map_or(0, |s| s.messages),
                    "folder": st.as_ref().and_then(|s| s.folder.clone()),
                    "error": st.as_ref().and_then(|s| s.error.clone()),
                })
            })
            .collect();
        Ok(json!({ "ready": ready, "accounts": accounts }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_lowercase() {
        assert_eq!(serde_json::to_value(State::Running).unwrap(), json!("running"));
        assert_eq!(serde_json::to_value(State::Failed).unwrap(), json!("failed"));
    }
}
