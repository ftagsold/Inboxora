//! Background catch-up: per-folder backfill on a dedicated connection, bulk-flag refresh,
//! auto-learned contacts and body cache warming.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use inboxora_core::params;
use inboxora_core::pg::Q;
use inboxora_imap::{Client, FetchQuery, Message};
use inboxora_mail::headers::{Headers, detect_bulk};
use serde_json::json;
use uuid::Uuid;

use super::connect::{Account, SlotGuard, load_account};

use super::policy::*;
use super::sync::{Ingested, ingest_query, recount_folder};
use super::{EResult, Engine, EngineError};

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

impl Engine {
    /// `backfillMessages`: fetch every UID the server has and the database lacks, newest
    /// first, in provider-tuned batches on a dedicated connection.
    pub async fn backfill_messages(self: &Arc<Self>, account: &Account, folder: &str) {
        let key = format!("{}:{folder}", account.id());
        let Some(_running) = self.claim(|s| &mut s.backfill_running, &key) else { return };
        let mut batch_size = account.profile().batch_size;
        let cfg = account.profile();
        let mut client: Option<Client> = None;
        let result: EResult<()> = async {
            let meta = self
                .db
                .opt("SELECT uid_validity, total_count FROM folders WHERE account_id = $1 AND path = $2", params![account.id(), folder])
                .await?;
            if let Some(m) = &meta {
                let validity: Option<i64> = m.get("uid_validity");
                let total: Option<i32> = m.get("total_count");
                if validity.is_some_and(|v| v != 0) && total.unwrap_or(0) > 0 {
                    let n: i64 = self
                        .db
                        .opt(
                            "SELECT COUNT(*) AS n FROM messages WHERE account_id = $1 AND folder = $2 AND is_deleted = false",
                            params![account.id(), folder],
                        )
                        .await?
                        .map_or(0, |r| r.get("n"));
                    if n >= i64::from(total.unwrap_or(0)) {
                        tracing::debug!(
                            "Backfill skipped for {}/{folder} — DB pre-check: {n} msgs ≥ cached total {}",
                            account.log(),
                            total.unwrap_or(0)
                        );
                        return Ok(());
                    }
                }
            }
            tracing::info!(
                "Starting backfill for {}/{folder} (batch={}, delay={}ms, fetchBody={})",
                account.log(),
                cfg.batch_size,
                cfg.batch_delay_ms,
                cfg.fetch_body
            );
            client = Some(self.open_backfill_client(account).await?);
            let mut batches_on_conn = 0usize;
            let server_uids: Vec<u32> = {
                let c = client.as_mut().expect("client");
                let mb = c.select(folder).await?.clone();
                if mb.exists == 0 {
                    tracing::debug!("Backfill {}: mailbox empty", account.log());
                    let _ = self
                        .db
                        .exec("UPDATE folders SET total_count = 0, unread_count = 0 WHERE account_id = $1 AND path = $2", params![
                            account.id(),
                            folder
                        ])
                        .await;
                    return Ok(());
                }
                let mut uids = c.search_uids("ALL").await?;
                uids.sort_unstable();
                if let Some(validity) = mb.uid_validity.filter(|v| *v > 0).map(i64::from) {
                    let stored: Option<i64> = self
                        .db
                        .opt("SELECT uid_validity FROM folders WHERE account_id = $1 AND path = $2", params![account.id(), folder])
                        .await?
                        .and_then(|r| r.get("uid_validity"));
                    if let Some(s) = stored.filter(|s| *s != 0)
                        && s != validity
                    {
                        tracing::warn!(
                            "Backfill: UIDVALIDITY changed for {}/{folder}: {s} → {validity}. Purging stale messages.",
                            account.log()
                        );
                        self.db.exec("DELETE FROM messages WHERE account_id = $1 AND folder = $2", params![account.id(), folder]).await?;
                    }
                    self.db
                        .exec("UPDATE folders SET uid_validity = $1 WHERE account_id = $2 AND path = $3", params![
                            validity,
                            account.id(),
                            folder
                        ])
                        .await?;
                }
                uids
            };
            let server_total = server_uids.len();
            let summary = self
                .db
                .opt(
                    "SELECT COUNT(*) AS count, COALESCE(MAX(uid), 0) AS max_uid FROM messages WHERE account_id = $1 AND folder = $2 AND is_deleted = false",
                    params![account.id(), folder],
                )
                .await?;
            let (db_count, max_db_uid): (i64, i64) = summary.map_or((0, 0), |r| (r.get("count"), r.get("max_uid")));
            let max_server = server_uids.last().copied().map_or(0, i64::from);
            if max_server > 0 && max_db_uid >= max_server && db_count >= server_total as i64 {
                tracing::info!(
                    "Backfill already complete for {}: maxDbUid={max_db_uid}, maxServerUid={max_server}, dbCount={db_count}",
                    account.log()
                );
                return Ok(());
            }
            let existing: HashSet<i64> = self
                .db
                .rows("SELECT uid FROM messages WHERE account_id = $1 AND folder = $2", params![account.id(), folder])
                .await?
                .iter()
                .map(|r| r.get::<_, i64>("uid"))
                .collect();
            let mut missing: Vec<u32> = server_uids.into_iter().filter(|u| !existing.contains(&i64::from(*u))).collect();
            missing.sort_unstable_by(|a, b| b.cmp(a));
            if missing.is_empty() {
                tracing::info!(
                    "Backfill {}: no missing UIDs ({db_count} in DB vs {server_total} on server — within tolerance)",
                    account.log()
                );
                let _ = recount_folder(&self.db, account.id(), folder).await;
                return Ok(());
            }
            tracing::info!("Backfill {}: {} missing of {server_total} ({db_count} already in DB)", account.log(), missing.len());
            self.broadcast(
                json!({ "type": "backfill_progress", "accountId": account.id(), "synced": db_count, "total": server_total }),
                Some(account.user_id()),
            );
            let mut consecutive_errors = 0u32;
            let mut i = 0usize;
            while i < missing.len() {
                if self.db.opt("SELECT id FROM email_accounts WHERE id = $1", params![account.id()]).await?.is_none() {
                    tracing::info!("Backfill stopping — account {} was deleted", account.log());
                    return Ok(());
                }
                if batches_on_conn >= cfg.batches_per_conn || client.is_none() {
                    if let Some(mut c) = client.take() {
                        c.logout().await;
                    }
                    match self.open_backfill_client(account).await {
                        Ok(c) => {
                            client = Some(c);
                            batches_on_conn = 0;
                        }
                        Err(e) => {
                            tracing::error!("Backfill reconnect failed for {}: {e}", account.log());
                            if e.0 == "Account deleted or disabled" {
                                return Ok(());
                            }
                            sleep_ms(cfg.error_delay_ms).await;
                            continue;
                        }
                    }
                }
                let batch: Vec<u32> = missing[i..(i + batch_size).min(missing.len())].to_vec();
                let set = batch.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                let c = client.as_mut().expect("client");
                let fetched: EResult<Vec<Message>> = async {
                    c.select(folder).await?;
                    let q = ingest_query(account, c, cfg.fetch_body);
                    Ok(c.fetch(&set, &q, true, None).await?)
                }
                .await;
                match fetched {
                    Ok(msgs) => {
                        let mut calendar: Vec<(Message, Uuid)> = Vec::new();
                        for msg in msgs {
                            match self.ingest_message(account, folder, &msg, cfg.fetch_body).await {
                                Ok(Some((Ingested::Upserted { id, .. }, _))) => calendar.push((msg, id)),
                                Ok(_) => {}
                                Err(e) => tracing::error!("Backfill parse error: {e}"),
                            }
                        }
                        for (msg, id) in calendar {
                            if let Err(e) = crate::services::invitations::persist_inbound_from_message(&self.db, c, &msg, id).await {
                                tracing::warn!("Inbound calendar invitation persistence failed: {e}");
                            }
                        }
                        i += batch.len();
                        batches_on_conn += 1;
                        consecutive_errors = 0;
                        if batches_on_conn % 10 == 1 || i >= missing.len() {
                            tracing::info!("Backfill {}: {i}/{} missing fetched", account.log(), missing.len());
                            self.broadcast(
                                json!({ "type": "backfill_progress", "accountId": account.id(), "synced": db_count + i as i64, "total": server_total }),
                                Some(account.user_id()),
                            );
                        }
                        sleep_ms(cfg.batch_delay_ms).await;
                    }
                    Err(e) => {
                        consecutive_errors += 1;
                        if let Some(mut c) = client.take() {
                            c.close();
                        }
                        batches_on_conn = cfg.batches_per_conn;
                        if consecutive_errors >= 3 {
                            let old = batch_size;
                            batch_size = (batch_size / 2).max(10);
                            tracing::warn!(
                                "Backfill reducing batch size for {}: {old} → {batch_size} after 3 failures ({e})",
                                account.log()
                            );
                            consecutive_errors = 0;
                            sleep_ms(cfg.batch_delay_ms).await;
                        } else {
                            let wait = cfg.error_delay_ms * u64::from(consecutive_errors.min(6));
                            tracing::error!(
                                "Backfill batch error for {}: {e} — retry {consecutive_errors}/3 after {wait}ms",
                                account.log()
                            );
                            sleep_ms(wait).await;
                        }
                    }
                }
            }
            tracing::info!("Backfill complete for {}/{folder}", account.log());
            if let Err(e) = recount_folder(&self.db, account.id(), folder).await {
                tracing::error!("Folder count update after backfill failed for {}/{folder}: {e}", account.log());
            }
            self.broadcast(json!({ "type": "backfill_complete", "accountId": account.id() }), Some(account.user_id()));
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::error!("Backfill failed for {}/{folder}: {e}", account.log());
        }
        if let Some(mut c) = client {
            c.logout().await;
        }
    }

    async fn open_backfill_client(&self, account: &Account) -> EResult<Client> {
        let row = load_account(&self.db, account.id()).await?;
        let Some(row) = row.filter(|r| r.enabled()) else {
            return Err(EngineError("Account deleted or disabled".into()));
        };
        Ok(self
            .open_client(&row, "Backfill connect", super::limits::Prio::Other, super::limits::Kind::Background)
            .await?
            .1)
    }

    /// `backfillAllFolders`: INBOX first, then every other folder the provider doesn't skip.
    pub async fn backfill_all_folders(self: &Arc<Self>, account: &Account) {
        let Some(_running) = self.claim(|s| &mut s.backfill_all_running, account.id()) else { return };
        self.broadcast(json!({ "type": "backfill_all_start", "accountId": account.id() }), Some(account.user_id()));
        {
            let host = account.host();
            let _slot = SlotGuard::acquire(&self.bg_sem, &host).await;
            let profile = account.profile();
            self.backfill_messages(account, "INBOX").await;
            // Archive, Sent, Trash, Drafts, Junk, then the rest (smallest first).
            let folders = self.folders_by_priority(account, false).await.unwrap_or_default();
            for (path, _) in folders {
                if skips_folder(profile, &path) {
                    continue;
                }
                self.backfill_messages(account, &path).await;
            }
        }
        drop(_running);
        self.broadcast(json!({ "type": "backfill_all_complete", "accountId": account.id() }), Some(account.user_id()));
        let (e, a) = (self.clone(), account.clone());
        tokio::spawn(async move { e.refresh_bulk_flags(&a).await });
    }

    /// `refreshBulkFlags`: evaluate list headers for rows whose `is_bulk` is still unknown.
    pub async fn refresh_bulk_flags(&self, account: &Account) {
        let Ok(rows) = self
            .db
            .rows(
                "SELECT id, uid, folder FROM messages WHERE account_id = $1 AND is_bulk IS NULL AND is_deleted = false ORDER BY folder, uid DESC LIMIT 5000",
                params![account.id()],
            )
            .await
        else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let mut by_folder: Vec<(String, Vec<(Uuid, i64)>)> = Vec::new();
        for r in &rows {
            let folder: String = r.get("folder");
            let entry = (r.get::<_, Uuid>("id"), r.get::<_, i64>("uid"));
            match by_folder.iter_mut().find(|(f, _)| *f == folder) {
                Some((_, v)) => v.push(entry),
                None => by_folder.push((folder, vec![entry])),
            }
        }
        tracing::info!("Bulk flag refresh: {} unevaluated messages for {}", rows.len(), account.log());
        for (folder, msgs) in by_folder {
            let run = async {
                let Some(row) = load_account(&self.db, account.id()).await? else { return Ok::<_, EngineError>(None) };
                let (_, mut c) = self
                    .open_client(&row, "Flag-sync connect", super::limits::Prio::Other, super::limits::Kind::Background)
                    .await?;
                let uid_to_id: HashMap<i64, Uuid> = msgs.iter().map(|(id, uid)| (*uid, *id)).collect();
                let r = async {
                    c.select(&folder).await?;
                    let set = msgs.iter().map(|(_, u)| u.to_string()).collect::<Vec<_>>().join(",");
                    let q = FetchQuery {
                        uid: true,
                        header_fields: ["list-unsubscribe", "list-id", "list-post", "precedence"]
                            .iter()
                            .map(|s| (*s).to_owned())
                            .collect(),
                        ..Default::default()
                    };
                    let fetched = c.fetch(&set, &q, true, None).await?;
                    Ok::<_, EngineError>(
                        fetched
                            .iter()
                            .filter_map(|m| {
                                let id = uid_to_id.get(&i64::from(m.uid?))?;
                                let h =
                                    Headers::parse(&String::from_utf8_lossy(m.headers.as_deref().unwrap_or_default()));
                                Some((*id, detect_bulk(&h)))
                            })
                            .collect::<Vec<_>>(),
                    )
                }
                .await;
                c.logout().await;
                let updates = r?;
                if !updates.is_empty() {
                    self.db
                        .exec(
                            "UPDATE messages SET is_bulk = v.is_bulk FROM (SELECT unnest($1::uuid[]) AS id, unnest($2::boolean[]) AS is_bulk) AS v WHERE messages.id = v.id",
                            params![
                                updates.iter().map(|u| u.0.to_string()).collect::<Vec<_>>(),
                                updates.iter().map(|u| u.1).collect::<Vec<_>>()
                            ],
                        )
                        .await?;
                }
                Ok(Some(updates.len()))
            };
            match run.await {
                Ok(Some(n)) => {
                    tracing::info!("Bulk flag refresh: {n}/{} updated in {folder} for {}", msgs.len(), account.log())
                }
                Ok(None) => return,
                Err(e) => tracing::warn!("Bulk flag refresh error for {}/{folder}: {e}", account.log()),
            }
        }
    }

    /// `upsertAutoContacts`: add unknown inbound senders to the Personal address book.
    pub async fn upsert_auto_contacts(&self, user_id: &str, senders: &[(String, String)]) {
        let run = async {
            let ab: Uuid = self
                .db
                .opt(
                    "INSERT INTO address_books (user_id, name) VALUES ($1, 'Personal') ON CONFLICT (user_id, name) DO UPDATE SET updated_at = NOW() RETURNING id",
                    params![user_id],
                )
                .await?
                .ok_or_else(|| EngineError("address book upsert returned nothing".into()))?
                .get("id");
            let mut inserted = 0u64;
            for (email, name) in senders {
                let primary = email.to_lowercase();
                let display = if name.trim().is_empty() { primary.clone() } else { name.trim().to_owned() };
                let uid = Uuid::new_v4().to_string();
                let emails = json!([{ "value": primary, "type": "other", "primary": true }]);
                let vcard =
                    inboxora_pim::vcard::generate(&json!({ "uid": uid, "displayName": display, "emails": emails }));
                if let Ok(n) = self
                    .db
                    .exec(
                        "INSERT INTO contacts (address_book_id, user_id, uid, vcard, etag, display_name, primary_email, emails, is_auto) \
                         VALUES ($1, $2, $3, $4, md5($4), $5, $6, $7::jsonb, true) \
                         ON CONFLICT (address_book_id, primary_email) WHERE primary_email IS NOT NULL DO NOTHING",
                        params![ab, user_id, uid, vcard, display, primary, emails.to_string()],
                    )
                    .await { inserted += n }
            }
            if inserted > 0 {
                self.db
                    .exec("UPDATE address_books SET sync_token = gen_random_uuid()::text, updated_at = NOW() WHERE id = $1", params![ab])
                    .await?;
            }
            Ok::<_, EngineError>(())
        };
        if let Err(e) = run.await {
            tracing::warn!("upsertAutoContacts error for user {user_id}: {e}");
        }
    }

    /// `prefetchNewMessageBodies`: warm the body cache for just-arrived mail, so opening it (or a
    /// notification) doesn't wait on IMAP.
    pub async fn prefetch_new_message_bodies(self: &Arc<Self>, account: &Account, messages: &[(Uuid, i64, String)]) {
        for (id, uid, folder) in messages {
            let loc = super::cache::Location {
                id: *id,
                account_id: account.id().to_owned(),
                folder: folder.clone(),
                uid: *uid,
            };
            if let Err(e) = self.message_body(&loc).await {
                tracing::warn!("Body prefetch failed for uid {uid}: {e}");
            }
        }
    }
}
