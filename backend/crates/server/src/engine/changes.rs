//! Change detection without re-reading every folder: one STATUS per folder decides which
//! folders changed since the last pass, and only those are synced (new mail, flags) and
//! reconciled (expunged mail). IMAP NOTIFY (RFC 5465), where the server offers it, reports
//! changes to any folder on the persistent connection, so a pass runs the moment mail is
//! moved or deleted on another device instead of on the next poll.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use inboxora_core::params;
use inboxora_core::pg::Q;
use inboxora_imap::{Client, MailboxStatus};
use serde_json::json;

use super::connect::{Account, SlotGuard};
use super::limits::Prio;
use super::{EResult, Engine, sync::recount_folder};

/// Debounce for change events (IDLE EXPUNGE/FETCH, NOTIFY STATUS): bursts collapse into one pass.
pub const CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);

/// What STATUS says about a folder; equal stamps mean nothing changed in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderStamp {
    validity: Option<u32>,
    uid_next: Option<u32>,
    messages: Option<u32>,
    modseq: Option<u64>,
}

impl FolderStamp {
    fn from_status(s: &MailboxStatus) -> Self {
        Self { validity: s.uid_validity, uid_next: s.uid_next, messages: s.messages, modseq: s.highest_modseq }
    }

    /// Whether the only change since `old` is mail appended: every UID handed out since is
    /// still there, so nothing was expunged and no reconcile is needed.
    fn only_appended(&self, old: &Self) -> bool {
        if self.validity != old.validity {
            return false;
        }
        match (self.messages, old.messages, self.uid_next, old.uid_next) {
            (Some(m), Some(om), Some(u), Some(ou)) => m >= om && u >= ou && m - om == u - ou,
            _ => false,
        }
    }
}

/// Outcome of one folder check.
struct Checked {
    stamp: FolderStamp,
    /// Rows were added, removed or had flags changed.
    changed: bool,
    /// `new_messages` already told the clients about the change.
    announced: bool,
}

impl Engine {
    /// Bring every changed folder of the account up to date. Concurrent calls coalesce: one
    /// that arrives while a pass runs makes that pass run once more.
    pub async fn sync_changed_folders(self: &Arc<Self>, account: &Account, poll_flags: bool) {
        let Some(_g) = self.claim(|s| &mut s.other_folders_syncing, account.id()) else {
            self.st.lock().changed_rerun.insert(account.id().to_owned());
            return;
        };
        loop {
            match self.changed_pass(account).await {
                Ok(inbox_has_modseq) => {
                    // Without CONDSTORE a flag change leaves STATUS untouched: poll INBOX flags.
                    if poll_flags && !inbox_has_modseq {
                        self.sync_flags_for_range(account).await;
                    }
                }
                Err(e) => tracing::warn!("Changed-folder sync failed for {}: {e}", account.log()),
            }
            if !self.st.lock().changed_rerun.remove(account.id()) {
                break;
            }
        }
    }

    /// Run [`Self::sync_changed_folders`] after [`CHANGE_DEBOUNCE`], restarting the wait on
    /// every call so a burst of events costs one pass.
    pub fn schedule_changed_sync(self: &Arc<Self>, account: &Account) {
        let (e, a) = (self.clone(), account.clone());
        let handle = tokio::spawn(async move {
            tokio::time::sleep(CHANGE_DEBOUNCE).await;
            e.st.lock().changed_debounce.remove(a.id());
            e.sync_changed_folders(&a, false).await;
        });
        if let Some(old) = self.st.lock().changed_debounce.insert(account.id().to_owned(), handle) {
            old.abort();
        }
    }

    /// Forget the folder stamps of an account (its next pass checks every folder in full).
    pub fn forget_folder_stamps(&self, account_id: &str) {
        self.st.lock().folder_stamps.remove(account_id);
    }

    /// One pass over every folder; returns whether INBOX reports HIGHESTMODSEQ.
    async fn changed_pass(self: &Arc<Self>, account: &Account) -> EResult<bool> {
        let mut queue: VecDeque<String> = std::iter::once("INBOX".to_owned())
            .chain(self.folders_by_priority(account, false).await?.into_iter().map(|f| f.0))
            .collect();
        let known = self.st.lock().folder_stamps.get(account.id()).cloned().unwrap_or_default();
        let host = account.host();
        let _slot = SlotGuard::acquire(&self.bg_sem, &host).await;
        let mut lease = self.lease_with(account, Prio::Inbox).await?;
        let mut stamps: HashMap<String, FolderStamp> = HashMap::new();
        let mut announce = false;
        let mut retries = 0;
        while let Some(folder) = queue.pop_front() {
            let key = format!("{}:{folder}", account.id());
            let Some(_od) = self.claim(|s| &mut s.on_demand_syncing, &key) else {
                // An on-demand sync of this folder is running; keep the old stamp so the next
                // pass looks again.
                if let Some(s) = known.get(&folder) {
                    stamps.insert(folder, *s);
                }
                continue;
            };
            match self.check_folder(account, lease.client(), &folder, known.get(&folder)).await {
                Ok(c) => {
                    announce |= c.changed && !c.announced;
                    stamps.insert(folder, c.stamp);
                }
                Err(e) => {
                    tracing::warn!("Folder check failed for {}/{folder}: {e}", account.log());
                    if !lease.client().is_usable() {
                        self.note_conn_failure(account, lease.client(), &e.0).await;
                        lease.evict();
                        drop(lease);
                        retries += 1;
                        if retries > 3 {
                            break;
                        }
                        queue.push_front(folder);
                        lease = self.lease_with(account, Prio::Inbox).await?;
                    }
                }
            }
        }
        let inbox_modseq = stamps.get("INBOX").is_some_and(|s| s.modseq.is_some());
        // Folders that vanished from the list drop out; failed checks keep no stamp and are
        // looked at in full next time.
        self.st.lock().folder_stamps.insert(account.id().to_owned(), stamps);
        if announce {
            self.broadcast(json!({ "type": "sync_complete", "accountId": account.id() }), Some(account.user_id()));
        }
        Ok(inbox_modseq)
    }

    async fn check_folder(
        self: &Arc<Self>,
        account: &Account,
        c: &mut Client,
        folder: &str,
        old: Option<&FolderStamp>,
    ) -> EResult<Checked> {
        // STATUS of the selected mailbox is unreliable on some servers (RFC 3501 6.3.10).
        if c.mailbox().is_some_and(|m| m.path.eq_ignore_ascii_case(&c.normalize_path(folder))) {
            c.unselect().await?;
        }
        let stamp = FolderStamp::from_status(&c.status(folder).await?);
        if old == Some(&stamp) {
            return Ok(Checked { stamp, changed: false, announced: false });
        }
        // First look since start (no stamp): full sync and reconcile, as a plain poll would.
        let reconcile = old.is_none_or(|o| !stamp.only_appended(o));
        let r = self.sync_messages(account, c, folder, 50, false, true).await?;
        let removed = if reconcile { self.reconcile_folder(account, c, folder).await? } else { 0 };
        let changed = old.is_some() || r.inserted > 0 || removed > 0;
        Ok(Checked { stamp, changed, announced: r.broadcasted_new && removed == 0 })
    }

    /// Drop rows of one folder whose UIDs the server no longer has; returns how many.
    pub async fn reconcile_folder(&self, account: &Account, c: &mut Client, folder: &str) -> EResult<usize> {
        let started: DateTime<Utc> = Utc::now();
        c.select(folder).await?;
        let server: HashSet<i64> = c.search_uids("ALL").await?.into_iter().map(i64::from).collect();
        let rows = self
            .db
            .rows(
                "SELECT uid FROM messages WHERE account_id = $1 AND folder = $2 AND (synced_at IS NULL OR synced_at < $3)",
                params![account.id(), folder, started],
            )
            .await?;
        let orphans: Vec<i64> = rows
            .iter()
            .map(|r| r.get::<_, i64>("uid"))
            .filter(|u| !server.contains(u) && !self.is_move_uid_guarded(account.id(), folder, *u))
            .collect();
        if orphans.is_empty() {
            return Ok(0);
        }
        tracing::info!("Reconcile: removing {} server-deleted message(s) from {}/{folder}", orphans.len(), account.log());
        self.db
            .exec(
                "DELETE FROM messages WHERE account_id = $1 AND folder = $2 AND uid = ANY($3::bigint[]) AND (synced_at IS NULL OR synced_at < $4)",
                params![account.id(), folder, orphans.clone(), started],
            )
            .await?;
        recount_folder(&self.db, account.id(), folder).await?;
        Ok(orphans.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(messages: u32, uid_next: u32) -> FolderStamp {
        FolderStamp { validity: Some(1), uid_next: Some(uid_next), messages: Some(messages), modseq: None }
    }

    #[test]
    fn appends_need_no_reconcile() {
        assert!(stamp(12, 112).only_appended(&stamp(10, 110)));
        assert!(stamp(10, 110).only_appended(&stamp(10, 110)));
    }

    #[test]
    fn expunges_need_reconcile() {
        // One expunged, one appended: same count, UIDNEXT moved.
        assert!(!stamp(10, 111).only_appended(&stamp(10, 110)));
        assert!(!stamp(9, 110).only_appended(&stamp(10, 110)));
        let mut other = stamp(10, 110);
        other.validity = Some(2);
        assert!(!other.only_appended(&stamp(10, 110)));
    }
}
