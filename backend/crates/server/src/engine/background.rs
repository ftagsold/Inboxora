//! Recovery and maintenance timers: health check, staleness probe,
//! durable flag-push reconciler, snooze wake-up and startup auto-connect.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use inboxora_core::params;
use inboxora_core::pg::Q;
use inboxora_imap::FetchQuery;
use serde_json::{Value, json};

use super::connect::{Account, load_account};
use super::policy::*;
use super::{EResult, Engine, EngineError, now_ms, race};
use crate::services::mail_utils::adjust_folder_counts;

fn every(period: Duration, f: impl Fn() -> futures_util::future::BoxFuture<'static, ()> + Send + 'static) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(period);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        iv.tick().await;
        loop {
            iv.tick().await;
            f().await;
        }
    });
}

impl Engine {
    /// Start the engine's timers (Node's constructor + `startSnoozeWatcher`).
    pub fn start(self: &Arc<Self>) {
        let e = self.clone();
        every(Duration::from_secs(90), move || {
            let e = e.clone();
            Box::pin(async move { e.health_check().await })
        });
        let e = self.clone();
        every(Duration::from_millis(STALENESS_CHECK_MS), move || {
            let e = e.clone();
            Box::pin(async move { e.staleness_check().await })
        });
        let e = self.clone();
        every(Duration::from_millis(FLAG_PUSH_RECONCILE_MS), move || {
            let e = e.clone();
            Box::pin(async move {
                if let Err(err) = e.reconcile_flag_pushes().await {
                    tracing::error!("Flag-push reconciler error: {err}");
                }
            })
        });
        let e = self.clone();
        every(Duration::from_secs(30), move || {
            let e = e.clone();
            Box::pin(async move {
                if let Err(err) = e.tombstone_janitor().await {
                    tracing::warn!("[mail-cache] tombstone janitor error: {err}");
                }
            })
        });
        let e = self.clone();
        every(Duration::from_secs(60), move || {
            let e = e.clone();
            Box::pin(async move {
                e.check_cache_sentinel().await;
                e.check_redis_memory().await;
            })
        });
        let e = self.clone();
        tokio::spawn(async move {
            e.check_cache_sentinel().await;
            tokio::time::sleep(Duration::from_secs(600)).await;
            loop {
                match e.orphan_sweep().await {
                    Ok(n) if n > 0 => tracing::info!("[mail-cache] orphan sweep removed {n} entries"),
                    Ok(_) => {}
                    Err(err) => tracing::warn!("[mail-cache] orphan sweep error: {err}"),
                }
                tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
            }
        });
        let e = self.clone();
        every(Duration::from_secs(60), move || {
            let e = e.clone();
            Box::pin(async move {
                if let Err(err) = e.run_snooze_wakeup().await {
                    tracing::error!("Snooze wakeup error: {err}");
                }
            })
        });
    }

    /// Reconnect every user's enabled accounts at startup, three users at a time.
    pub async fn connect_all_at_startup(self: &Arc<Self>) {
        let rows = match self
            .db
            .rows("SELECT DISTINCT user_id FROM email_accounts WHERE enabled = true AND protocol = 'imap'", params![])
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Startup account connection error: {e}");
                return;
            }
        };
        if rows.is_empty() {
            return;
        }
        tracing::info!("Reconnecting accounts for {} user(s) on startup", rows.len());
        let users: Vec<String> =
            rows.iter().filter_map(|r| r.get::<_, Option<uuid::Uuid>>("user_id")).map(|u| u.to_string()).collect();
        let queue = Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::from(users)));
        for _ in 0..3 {
            let (e, q) = (self.clone(), queue.clone());
            tokio::spawn(async move {
                loop {
                    let Some(user) = q.lock().pop_front() else { return };
                    e.connect_all_for_user(&user).await;
                }
            });
        }
    }

    async fn health_check(self: &Arc<Self>) {
        let rows = match self
            .db
            .rows("SELECT id, email_address FROM email_accounts WHERE enabled = true AND protocol = 'imap'", params![])
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Health check error: {e}");
                return;
            }
        };
        for r in rows {
            let id = r.get::<_, uuid::Uuid>("id").to_string();
            let (connected, connecting, poll_healthy, cooling, last) = {
                let st = self.st.lock();
                (
                    st.connections.contains_key(&id),
                    st.connecting.contains(&id),
                    st.poll_only.contains(&id) && st.sync_timers.contains_key(&id),
                    st.connect_cooldown.get(&id).is_some_and(|(until, _)| now_ms() < *until),
                    st.last_sync_ok_at.get(&id).copied(),
                )
            };
            if !connected && !connecting && !poll_healthy {
                if cooling {
                    continue;
                }
                let Ok(Some(account)) = load_account(&self.db, &id).await else { continue };
                tracing::info!("Health check: reconnecting {} (not connected)", account.log());
                let e = self.clone();
                tokio::spawn(async move {
                    e.connect_account(account).await;
                });
            } else if connected
                && let Some(last) = last
                && now_ms().saturating_sub(last) > STALE_SYNC_WARN_MS
            {
                let email: Option<String> = r.get("email_address");
                tracing::warn!(
                    "Health check: {} connected but no successful sync in {}m — possible stale connection",
                    super::connect::redact_email(email.as_deref().unwrap_or("")),
                    (now_ms() - last) / 60_000
                );
            }
        }
    }

    /// Probe each persistent connection with a fresh login: mail above our INBOX watermark
    /// that we don't already store means the connection went deaf — reconnect it.
    async fn staleness_check(self: &Arc<Self>) {
        let Some(_running) = self.claim(|s| &mut s.reconciling, "__staleness__") else { return };
        let ids: Vec<String> = self.st.lock().connections.keys().cloned().collect();
        for id in ids {
            if self.st.lock().connecting.contains(&id) {
                continue;
            }
            let Some(observed) = self.persistent(&id) else { continue };
            let r: EResult<()> = async {
                let Some(account) = load_account(&self.db, &id).await? else { return Ok(()) };
                let max_uid: i64 = self
                    .db
                    .opt("SELECT MAX(uid)::bigint AS maxuid FROM messages WHERE account_id = $1 AND folder = 'INBOX'", params![&id])
                    .await?
                    .and_then(|r| r.get::<_, Option<i64>>("maxuid"))
                    .unwrap_or(0);
                if max_uid == 0 {
                    return Ok(());
                }
                let missed = race(25_000, "Staleness probe", async {
                    let (_, mut probe) = self.open_client_bounded(&account, "Staleness", super::limits::Prio::Inbox, super::limits::Kind::Background).await?;
                    let r = async {
                        probe.select("INBOX").await?;
                        let above = probe.search_uids(&format!("UID {}:*", max_uid + 1)).await?;
                        let mut candidates: Vec<u32> = above.into_iter().filter(|u| i64::from(*u) > max_uid).collect();
                        candidates.sort_unstable();
                        let candidates: Vec<u32> = candidates[candidates.len().saturating_sub(200)..].to_vec();
                        if candidates.is_empty() {
                            return Ok::<_, EngineError>(0usize);
                        }
                        let set = candidates.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                        let fetched: Vec<Option<String>> = probe
                            .fetch(&set, &FetchQuery { uid: true, envelope: true, ..Default::default() }, true, None)
                            .await?
                            .into_iter()
                            .map(|m| {
                                m.envelope
                                    .and_then(|e| e.message_id)
                                    .map(|mid| mid.chars().filter(|c| *c != '<' && *c != '>').collect::<String>().trim().to_owned())
                            })
                            .collect();
                        if fetched.is_empty() {
                            return Ok(0);
                        }
                        let with_mid: Vec<&String> = fetched.iter().flatten().filter(|m| !m.is_empty()).collect();
                        let mut have: HashSet<String> = HashSet::new();
                        if !with_mid.is_empty() {
                            let mut forms: Vec<String> = Vec::new();
                            for m in &with_mid {
                                forms.push((*m).clone());
                                forms.push(format!("<{m}>"));
                            }
                            have = self
                                .db
                                .rows("SELECT message_id FROM messages WHERE account_id = $1 AND message_id = ANY($2::text[])", params![&id, forms])
                                .await?
                                .iter()
                                .map(|r| {
                                    r.get::<_, String>("message_id").chars().filter(|c| *c != '<' && *c != '>').collect::<String>().trim().to_owned()
                                })
                                .collect();
                        }
                        Ok(fetched.iter().filter(|m| !m.as_ref().is_some_and(|m| !m.is_empty() && have.contains(m))).count())
                    }
                    .await;
                    probe.close();
                    r
                })
                .await?;
                if missed == 0 {
                    return Ok(());
                }
                if self.st.lock().connecting.contains(&id) {
                    return Ok(());
                }
                if self.persistent(&id).is_none_or(|p| p.generation != observed.generation) {
                    return Ok(());
                }
                let was_syncing = self.st.lock().syncing.contains(&id);
                if was_syncing {
                    let started = self.st.lock().sync_started_at.get(&id).copied();
                    if started.is_none_or(|s| now_ms().saturating_sub(s) < SYNC_HUNG_MS) {
                        return Ok(());
                    }
                }
                tracing::warn!(
                    "Staleness check: {} server has {missed} INBOX message(s) above synced UID {max_uid} — persistent connection {}, forcing reconnect",
                    account.log(),
                    if was_syncing { "hung mid-sync" } else { "missed mail" }
                );
                self.record_signal("staleness_missed_mail", &id, Some(missed as f64));
                self.drop_persistent(&id, &observed);
                self.evict_pool(&id);
                let e = self.clone();
                tokio::spawn(async move {
                    if was_syncing {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                    e.sync_tick(account).await;
                });
                Ok(())
            }
            .await;
            if let Err(e) = r {
                self.record_warning("staleness_error", &id);
                tracing::warn!("Staleness check error for {id}: {e}");
            }
        }
    }

    /// Re-push read/star changes whose IMAP write failed, holding the local-wins window.
    pub async fn reconcile_flag_pushes(self: &Arc<Self>) -> EResult<()> {
        let accounts: Vec<String> = self.st.lock().pending_flag_push.keys().cloned().collect();
        for account_id in accounts {
            let ops: Vec<(String, super::FlagOp)> = match self.st.lock().pending_flag_push.get(&account_id) {
                Some(o) if !o.is_empty() => o.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                _ => {
                    self.st.lock().pending_flag_push.remove(&account_id);
                    continue;
                }
            };
            let read_ids: Vec<String> =
                ops.iter().filter(|(_, o)| o.flag == "\\Seen").map(|(_, o)| o.message_id.clone()).collect();
            let star_ids: Vec<String> =
                ops.iter().filter(|(_, o)| o.flag != "\\Seen").map(|(_, o)| o.message_id.clone()).collect();
            if !read_ids.is_empty() {
                let _ = self
                    .db
                    .exec("UPDATE messages SET read_changed_at = NOW() WHERE id = ANY($1::uuid[])", params![read_ids])
                    .await;
            }
            if !star_ids.is_empty() {
                let _ = self
                    .db
                    .exec("UPDATE messages SET star_changed_at = NOW() WHERE id = ANY($1::uuid[])", params![star_ids])
                    .await;
            }
            if !self.is_connected(&account_id) {
                continue;
            }
            let Some(account) = load_account(&self.db, &account_id).await? else {
                self.st.lock().pending_flag_push.remove(&account_id);
                continue;
            };
            for (key, op) in ops.into_iter().take(FLAG_PUSH_PER_CYCLE) {
                if !self.st.lock().pending_flag_push.get(&account_id).is_some_and(|m| m.contains_key(&key)) {
                    continue;
                }
                let row =
                    self.db.opt("SELECT uid, folder FROM messages WHERE id = $1", params![&op.message_id]).await?;
                let Some(row) = row else {
                    self.drop_flag_op(&account_id, &key);
                    continue;
                };
                if op.resolved.load(Ordering::SeqCst) {
                    continue;
                }
                let (uid, folder): (i64, String) = (row.get("uid"), row.get("folder"));
                let col = if op.flag == "\\Seen" {
                    "is_read = $1, read_changed_at = NOW()"
                } else {
                    "is_starred = $1, star_changed_at = NOW()"
                };
                let _ = self
                    .db
                    .exec(&format!("UPDATE messages SET {col} WHERE id = $2"), params![op.value, &op.message_id])
                    .await;
                match self.set_flag(&account, uid, &folder, &op.flag, op.value).await {
                    Ok(()) => {
                        if !op.resolved.load(Ordering::SeqCst) {
                            self.clear_flag_marker(&op.message_id, &op.flag).await;
                        }
                        self.drop_flag_op(&account_id, &key);
                    }
                    Err(e) => {
                        let attempts = {
                            let mut st = self.st.lock();
                            let Some(entry) = st.pending_flag_push.get_mut(&account_id).and_then(|m| m.get_mut(&key))
                            else {
                                continue;
                            };
                            entry.attempts += 1;
                            entry.attempts
                        };
                        if attempts >= FLAG_PUSH_MAX_ATTEMPTS {
                            tracing::warn!(
                                "Flag-push giving up after {attempts} attempts ({} msg={}): {e}",
                                op.flag,
                                op.message_id
                            );
                            self.clear_flag_marker(&op.message_id, &op.flag).await;
                            self.drop_flag_op(&account_id, &key);
                        }
                    }
                }
            }
            let mut st = self.st.lock();
            if st.pending_flag_push.get(&account_id).is_some_and(|m| m.is_empty()) {
                st.pending_flag_push.remove(&account_id);
            }
        }
        Ok(())
    }

    fn drop_flag_op(&self, account_id: &str, key: &str) {
        let mut st = self.st.lock();
        if let Some(m) = st.pending_flag_push.get_mut(account_id) {
            m.remove(key);
        }
    }

    async fn clear_flag_marker(&self, message_id: &str, flag: &str) {
        let col = if flag == "\\Seen" { "read_changed_at" } else { "star_changed_at" };
        let _ = self.db.exec(&format!("UPDATE messages SET {col} = NULL WHERE id = $1"), params![message_id]).await;
    }

    /// `_runSnoozeWakeup`: move due snoozed messages back and mark them unread.
    pub async fn run_snooze_wakeup(self: &Arc<Self>) -> EResult<()> {
        let Some(_running) = self.claim(|s| &mut s.reconciling, "__snooze__") else { return Ok(()) };
        let due = self
            .db
            .rows(
                "SELECT sm.id AS snooze_id, sm.user_id, sm.account_id, sm.message_id_header, sm.original_folder, sm.snoozed_folder, m.uid, m.is_read \
                 FROM snoozed_messages sm JOIN messages m ON m.account_id = sm.account_id AND m.message_id = sm.message_id_header \
                   AND m.folder = sm.snoozed_folder AND m.is_deleted = false \
                 WHERE sm.snooze_until <= NOW()",
                params![],
            )
            .await?;
        for row in due {
            let snooze_id: uuid::Uuid = row.get("snooze_id");
            let r: EResult<()> = async {
                let account_id = row.get::<_, uuid::Uuid>("account_id").to_string();
                let user_id = row.get::<_, uuid::Uuid>("user_id").to_string();
                let mid: Option<String> = row.get("message_id_header");
                let (original, snoozed): (String, String) = (row.get("original_folder"), row.get("snoozed_folder"));
                let uid: i64 = row.get("uid");
                let was_read: Option<bool> = row.get("is_read");
                let Some(account) = load_account(&self.db, &account_id).await? else { return Ok(()) };
                self.guard_move_uid(&account_id, &snoozed, uid);
                let moved: EResult<()> = async {
                    let new_uid = self.move_message(&account, uid, &snoozed, &original).await?;
                    if let Some(n) = new_uid {
                        self.set_flag(&account, n, &original, "\\Seen", false).await?;
                    } else if let Some(m) = mid.as_deref().filter(|m| !m.is_empty())
                        && let Err(e) = self.unseen_by_message_id(&account, &original, m).await {
                            tracing::warn!("Snooze wakeup: could not mark message unread on server (no UIDPLUS): {e}");
                        }
                    match new_uid {
                        Some(n) => {
                            self.db
                                .exec(
                                    "UPDATE messages SET folder = $1, is_read = false, read_changed_at = NOW(), uid = $4 WHERE account_id = $2 AND message_id = $3 AND folder = $5",
                                    params![&original, &account_id, &mid, n, &snoozed],
                                )
                                .await?;
                        }
                        None => {
                            self.guard_move_uid(&account_id, &original, uid);
                            self.db
                                .exec(
                                    "UPDATE messages SET folder = $1, is_read = false, read_changed_at = NOW() WHERE account_id = $2 AND message_id = $3 AND folder = $4",
                                    params![&original, &account_id, &mid, &snoozed],
                                )
                                .await?;
                            let (e, a, f) = (self.clone(), account_id.clone(), original.clone());
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_secs(10)).await;
                                e.unguard_move_uid(&a, &f, uid);
                            });
                        }
                    }
                    Ok(())
                }
                .await;
                self.unguard_move_uid(&account_id, &snoozed, uid);
                moved?;
                self.db.exec("DELETE FROM snoozed_messages WHERE id = $1", params![snooze_id]).await?;
                adjust_folder_counts(&self.db, &account_id, &snoozed, -1, if was_read.unwrap_or(false) { 0 } else { -1 });
                adjust_folder_counts(&self.db, &account_id, &original, 1, 1);
                self.broadcast(json!({ "type": "snooze_wakeup", "accountId": account_id }), Some(&user_id));
                tracing::info!("Snooze wakeup: message {} restored to {original}", mid.as_deref().unwrap_or(""));
                Ok(())
            }
            .await;
            if let Err(e) = r {
                tracing::error!("Snooze wakeup failed for snooze_id {snooze_id}: {e}");
            }
        }
        self.db
            .exec(
                "DELETE FROM snoozed_messages sm WHERE sm.snooze_until <= NOW() - INTERVAL '5 minutes' AND NOT EXISTS ( \
                   SELECT 1 FROM messages m WHERE m.account_id = sm.account_id AND m.message_id = sm.message_id_header \
                     AND m.folder = sm.snoozed_folder AND m.is_deleted = false)",
                params![],
            )
            .await?;
        Ok(())
    }

    async fn unseen_by_message_id(self: &Arc<Self>, account: &Account, folder: &str, message_id: &str) -> EResult<()> {
        let mut lease = self.lease(account).await?;
        let r: EResult<()> = async {
            let c = lease.client();
            c.select(folder).await?;
            let q = format!("HEADER Message-ID \"{}\"", message_id.replace('\\', "\\\\").replace('"', "\\\""));
            let uids = c.search_uids(&q).await?;
            match uids.first() {
                Some(u) => c.store_flags(&u.to_string(), false, &["\\Seen"]).await?,
                None => tracing::warn!(
                    "Snooze wakeup: could not find message in {folder} to mark unread (Message-ID: {message_id})"
                ),
            }
            Ok(())
        }
        .await;
        if r.is_err() {
            lease.evict();
        }
        r
    }

    /// Diagnostics snapshot of the engine's live state.
    pub fn diagnostics(&self) -> Value {
        let st = self.st.lock();
        json!({
            "connections": st.connections.len(),
            "pollOnly": st.poll_only.len(),
            "connecting": st.connecting.len(),
            "syncing": st.syncing.len(),
            "backfillRunning": st.backfill_running.len(),
            "pendingFlagPushes": st.pending_flag_push.values().map(|m| m.len()).sum::<usize>(),
        })
    }
}
