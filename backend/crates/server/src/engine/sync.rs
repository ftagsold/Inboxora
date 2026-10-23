//! Folder and message synchronisation (`syncFolders`, `syncMessages`, the shared ingest upsert,
//! flag scans, the other-folder poll and delete reconciliation).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use inboxora_core::params;
use inboxora_core::pg::{Db, Q};
use inboxora_imap::{Client, FetchQuery, Message};
use serde_json::{Value, json};
use uuid::Uuid;

use super::connect::Account;
use super::mime;
use super::parse::{EnrichContext, Parsed, clean, clean_opt, enrich, parse_message, safe_date};
use super::policy::*;
use super::{EResult, Engine, EngineError};
use crate::services::conversation::{CopyFacts, RawCopy, persist_copy_for_row};

#[derive(Debug, Default, Clone, Copy)]
pub struct SyncResult {
    pub inserted: usize,
    pub broadcasted_new: bool,
}

/// A newly inserted unread INBOX message (notification / rules candidate).
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub id: Uuid,
    pub parsed: Parsed,
    pub account_id: String,
    pub folder: String,
}

impl NewMessage {
    pub fn to_json(&self) -> Value {
        let mut m = self.parsed.to_json();
        m.insert("id".into(), json!(self.id.to_string()));
        m.insert("accountId".into(), json!(self.account_id));
        m.insert("folder".into(), json!(self.folder));
        Value::Object(m)
    }
}

fn invalid_messageset(e: &EngineError) -> bool {
    e.0.to_lowercase().contains("invalid messageset")
}

/// `RELOCATE_MESSAGE_SQL` with the (empty, plugin-less) exempt guard and RETURNING.
const RELOCATE_SQL: &str = "UPDATE messages SET \
    folder = $1::text, uid = $2::bigint, is_deleted = false, date = $5::timestamptz \
  WHERE account_id = $3::uuid AND message_id = $4::text AND (folder != $1::text OR uid != $2::bigint) \
    AND 1 = (SELECT COUNT(*) FROM messages WHERE account_id = $3::uuid AND message_id = $4::text) \
    AND COALESCE((SELECT special_use FROM folders WHERE account_id = $3::uuid AND path = $1::text), '') NOT IN ('\\All', '\\Important') \
  RETURNING id";

/// The ingest upsert shared by sync and backfill (`INSERT … ON CONFLICT … DO UPDATE`). Only the
/// header index is stored; display fields go to the Redis cache.
const UPSERT_SQL: &str = "INSERT INTO messages ( \
    account_id, uid, folder, message_id, date, is_read, is_starred, has_attachments, flags, thread_id, is_bulk, category \
  ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
  ON CONFLICT (account_id, uid, folder) DO UPDATE SET \
    is_read = CASE WHEN messages.read_changed_at IS NOT NULL AND NOW() - messages.read_changed_at < interval '30 seconds' THEN messages.is_read ELSE EXCLUDED.is_read END, \
    is_starred = CASE WHEN messages.star_changed_at IS NOT NULL AND NOW() - messages.star_changed_at < interval '30 seconds' THEN messages.is_starred ELSE EXCLUDED.is_starred END, \
    flags = EXCLUDED.flags, \
    thread_id = CASE WHEN messages.thread_id = messages.message_id AND EXCLUDED.thread_id IS NOT NULL AND EXCLUDED.thread_id <> messages.message_id \
                     THEN EXCLUDED.thread_id ELSE COALESCE(messages.thread_id, EXCLUDED.thread_id) END, \
    is_bulk = COALESCE(messages.is_bulk, EXCLUDED.is_bulk), \
    category = COALESCE(messages.category, EXCLUDED.category) \
  WHERE (messages.is_read, messages.is_starred, messages.flags, messages.thread_id, messages.is_bulk, messages.category) \
    IS DISTINCT FROM (EXCLUDED.is_read, EXCLUDED.is_starred, EXCLUDED.flags, EXCLUDED.thread_id, EXCLUDED.is_bulk, EXCLUDED.category) \
  RETURNING id, (xmax = 0) AS is_new";

/// `computeThreadId`: the legacy list thread id from References/In-Reply-To.
pub async fn compute_thread_id(
    db: &Db,
    account_id: &str,
    message_id: &str,
    in_reply_to: &str,
    references: &str,
) -> EResult<Option<String>> {
    if message_id.is_empty() {
        return Ok(None);
    }
    let refs: Vec<String> = if references.is_empty() {
        vec![]
    } else {
        static RE: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]+>").expect("regex"));
        RE.find_iter(references).map(|m| m.as_str().to_owned()).collect()
    };
    let mut candidates = refs.clone();
    if !in_reply_to.is_empty() && !refs.iter().any(|r| r == in_reply_to) {
        candidates.push(in_reply_to.to_owned());
    }
    if candidates.is_empty() {
        return Ok(Some(message_id.to_owned()));
    }
    let rows = db
        .rows(
            "SELECT message_id, thread_id FROM messages WHERE account_id = $1 AND message_id = ANY($2::text[]) AND thread_id IS NOT NULL",
            params![account_id, candidates.clone()],
        )
        .await?;
    let found: HashMap<String, String> = rows
        .iter()
        .map(|r| (r.get::<_, Option<String>>("message_id").unwrap_or_default(), r.get::<_, String>("thread_id")))
        .collect();
    for c in &candidates {
        if let Some(t) = found.get(c) {
            return Ok(Some(t.clone()));
        }
    }
    Ok(Some(candidates[0].clone()))
}

/// The fetch query for sync/backfill (`providerFetchQuery`).
pub fn ingest_query(account: &Account, client: &Client, body_parts: bool) -> FetchQuery {
    let provider = crate::services::conversation::classify_provider_host(&account.host());
    let caps = client.capabilities().join(" ");
    let thread =
        provider == "gmail" || caps.contains("OBJECTID") || caps.contains("THREADID") || caps.contains("X-GM-EXT-1");
    FetchQuery {
        uid: true,
        flags: true,
        envelope: true,
        body_structure: true,
        size: true,
        internal_date: true,
        headers: true,
        thread_id: thread,
        body_parts: if body_parts { BODY_PREFETCH_PARTS.iter().map(|s| (*s).to_owned()).collect() } else { vec![] },
        ..Default::default()
    }
}

pub enum Ingested {
    Relocated,
    Upserted { id: Uuid, is_new: bool },
}

impl Engine {
    /// Parse, relocate-or-upsert one fetched message and attach it to its conversation.
    pub async fn ingest_message(
        &self,
        account: &Account,
        folder: &str,
        msg: &Message,
        with_body: bool,
    ) -> EResult<Option<(Ingested, Parsed)>> {
        let mut parsed = parse_message(msg);
        enrich(
            &mut parsed,
            &EnrichContext {
                account_email: account.str("email_address"),
                account_name: account.str("name"),
                sender_name: account.str("sender_name"),
                folder_path: folder,
                sent_folder_path: account.mapping("sent"),
            },
        );
        if parsed.uid == 0 {
            tracing::warn!("Message sync skipped: IMAP FETCH returned no UID for {}/{folder}", account.log());
            return Ok(None);
        }
        let mut body: Option<super::cache::CachedBody> = None;
        if with_body {
            let (h, t, a) = mime::extract_body(msg.body_structure.as_ref(), &msg.body_parts);
            body = Some(super::cache::CachedBody {
                html: h.as_deref().and_then(super::cache::prepare_html),
                text: t.as_deref().map(clean).filter(|t| !t.is_empty()),
                attachments: Value::Array(a.iter().map(mime::AttachmentRef::to_json).collect()),
            });
        }
        let msg_id = clean_opt(parsed.message_id.as_deref());
        let in_reply_to = clean_opt(parsed.in_reply_to.as_deref());
        let refs = clean_opt(parsed.references.as_deref());
        let thread_id = compute_thread_id(&self.db, account.id(), &msg_id, &in_reply_to, &refs).await?;
        let date = safe_date(parsed.date);
        let has_snippet_parts = !msg.body_parts.is_empty();
        if !msg_id.is_empty() {
            let relocated =
                self.db.rows(RELOCATE_SQL, params![folder, i64::from(parsed.uid), account.id(), &msg_id, date]).await?;
            if let Some(r) = relocated.first() {
                self.remember_meta(r.get("id"), &parsed, has_snippet_parts).await;
                return Ok(Some((Ingested::Relocated, parsed)));
            }
        }
        let category = self.categorize(account, &parsed).await;
        let upserted = self
            .db
            .opt(
                UPSERT_SQL,
                params![
                    account.id(),
                    i64::from(parsed.uid),
                    folder,
                    msg_id.clone(),
                    date,
                    parsed.is_read,
                    parsed.is_starred,
                    parsed.has_attachments,
                    serde_json::to_string(&parsed.flags).unwrap_or_else(|_| "[]".into()),
                    thread_id.clone(),
                    parsed.is_bulk,
                    category
                ],
            )
            .await?;
        let (id, is_new): (Uuid, bool) = match upserted {
            Some(row) => (row.get("id"), row.get::<_, Option<bool>>("is_new").unwrap_or(false)),
            // Nothing changed, so the conditional upsert returned no row.
            None => (
                self.db
                    .opt(
                        "SELECT id FROM messages WHERE account_id = $1 AND uid = $2 AND folder = $3",
                        params![account.id(), i64::from(parsed.uid), folder],
                    )
                    .await?
                    .ok_or_else(|| EngineError("upsert returned no row".into()))?
                    .get("id"),
                false,
            ),
        };
        self.remember_meta(id, &parsed, has_snippet_parts).await;
        if let Some(b) = &body {
            self.cache.put(&super::cache::body_key(&id), b).await;
        }
        let mut facts = CopyFacts::from_parsed(&parsed);
        facts.body_text = body.as_ref().and_then(|b| b.text.clone());
        persist_copy_for_row(
            &self.db,
            id,
            account.id(),
            account.user_id(),
            &account.host(),
            &RawCopy {
                headers: Some(&parsed.headers),
                email_id: msg.email_id.as_deref(),
                thread_id: msg.thread_id.as_deref(),
                message_id: parsed.message_id.as_deref(),
            },
            &facts,
        )
        .await;
        if let Some(t) = thread_id
            && t != msg_id
        {
            self.db
                .exec(
                    "UPDATE messages SET thread_id = $1 WHERE account_id = $2 AND thread_id = $3 AND message_id != $3",
                    params![t, account.id(), &msg_id],
                )
                .await?;
        }
        Ok(Some((Ingested::Upserted { id, is_new }, parsed)))
    }

    async fn categorize(&self, account: &Account, parsed: &Parsed) -> Option<String> {
        use crate::services::categorizer::{classify_message, global_categorization_enabled, load_social_domains};
        let enabled = account.bool("categorization_enabled").unwrap_or(false)
            || global_categorization_enabled(&self.db, account.user_id()).await.unwrap_or(false);
        if !enabled {
            return None;
        }
        let social = load_social_domains(&self.db, account.user_id()).await.ok()?;
        let c = classify_message(&parsed.headers, Some(&parsed.from_email), &social);
        (c != "primary").then(|| c.to_owned())
    }

    /// `syncFolders`: LIST → folders table, implicit INBOX, prune vanished folders.
    pub async fn sync_folders(&self, account: &Account, client: &mut Client) -> EResult<()> {
        let run = async {
            let boxes = client.list().await?;
            for mb in &boxes {
                let no_select = mb.flags.contains("\\Noselect") || mb.flags.contains("\\NonExistent");
                self.db
                    .exec(
                        "INSERT INTO folders (account_id, path, name, delimiter, special_use, no_select) VALUES ($1, $2, $3, $4, $5, $6) \
                         ON CONFLICT (account_id, path) DO UPDATE SET name = $3, special_use = $5, no_select = $6, updated_at = NOW()",
                        params![account.id(), &mb.path, &mb.name, &mb.delimiter, &mb.special_use, no_select],
                    )
                    .await?;
            }
            if !boxes.iter().any(|b| b.path == "INBOX") {
                let delimiter = boxes.first().and_then(|b| b.delimiter.clone()).unwrap_or_else(|| "/".into());
                self.db
                    .exec(
                        "INSERT INTO folders (account_id, path, name, delimiter, special_use) VALUES ($1, 'INBOX', 'INBOX', $2, NULL) \
                         ON CONFLICT (account_id, path) DO NOTHING",
                        params![account.id(), delimiter],
                    )
                    .await?;
            }
            if !boxes.is_empty() {
                let paths: Vec<String> = boxes.iter().map(|b| b.path.clone()).collect();
                let pruned = self
                    .db
                    .rows(
                        "DELETE FROM folders WHERE account_id = $1 AND path != 'INBOX' AND NOT (path = ANY($2)) RETURNING path",
                        params![account.id(), paths],
                    )
                    .await?;
                if !pruned.is_empty() {
                    let gone: Vec<String> = pruned.iter().map(|r| r.get::<_, String>("path")).collect();
                    let dropped = self
                        .db
                        .exec(
                            "DELETE FROM messages WHERE account_id = $1 AND folder = ANY($2)",
                            params![account.id(), gone.clone()],
                        )
                        .await?;
                    tracing::info!(
                        "Folder sync for {}: dropped {} folder(s) no longer on the server ({}) and {dropped} cached message(s)",
                        account.log(),
                        gone.len(),
                        gone.join(", ")
                    );
                }
            }
            Ok::<_, EngineError>(())
        };
        if let Err(e) = run.await {
            tracing::error!("Folder sync error for {}: {e}", account.log());
            // A dead socket must still surface to the caller (it owns reconnection).
            if !client.is_usable() {
                return Err(e);
            }
        }
        Ok(())
    }

    /// `syncMessages`: UIDVALIDITY check, UID-watermark new-mail phase, CONDSTORE-planned
    /// flag scan, rule/notification flush, watermark advance and badge recompute.
    pub fn sync_messages<'a>(
        self: &'a Arc<Self>,
        account: &'a Account,
        client: &'a mut Client,
        folder: &'a str,
        limit: u32,
        prefetch_body: bool,
        no_body_parts: bool,
    ) -> futures_util::future::BoxFuture<'a, EResult<SyncResult>> {
        // Boxed with an explicit `Send` type: rules → moves → on-demand sync → rules is a
        // cycle the compiler can only check through a nominal future type.
        Box::pin(async move {
            let r = self.sync_messages_inner(account, client, folder, limit, prefetch_body, no_body_parts).await;
            if let Err(e) = &r {
                tracing::error!("Message sync error for {}/{folder}: {e}", account.log());
            }
            r
        })
    }

    async fn sync_messages_inner(
        self: &Arc<Self>,
        account: &Account,
        client: &mut Client,
        folder: &str,
        limit: u32,
        prefetch_body: bool,
        no_body_parts: bool,
    ) -> EResult<SyncResult> {
        let provider = account.profile();
        let mailbox = client.select(folder).await?.clone();
        if mailbox.exists == 0 {
            stamp_last_sync(&self.db, account.id()).await?;
            return Ok(SyncResult::default());
        }
        let current_validity = mailbox.uid_validity.filter(|v| *v > 0).map(i64::from);
        let server_modseq = mailbox.highest_modseq;
        let mut stored_modseq: Option<u64> = None;
        let mut validity_changed = false;
        if let Some(validity) = current_validity {
            let row = self
                .db
                .opt(
                    "SELECT uid_validity, highest_modseq::text AS highest_modseq FROM folders WHERE account_id = $1 AND path = $2",
                    params![account.id(), folder],
                )
                .await?;
            let stored_validity =
                row.as_ref().and_then(|r| r.get::<_, Option<i64>>("uid_validity")).filter(|v| *v != 0);
            stored_modseq =
                row.as_ref().and_then(|r| parse_modseq(r.get::<_, Option<String>>("highest_modseq").as_deref()));
            if let Some(stored) = stored_validity
                && stored != validity
            {
                validity_changed = true;
                self.record_signal("uidvalidity_change", account.id(), None);
                tracing::warn!(
                    "UIDVALIDITY changed for {}/{folder}: {stored} → {validity}. Purging stale messages and re-backfilling.",
                    account.log()
                );
                self.db
                    .exec("DELETE FROM messages WHERE account_id = $1 AND folder = $2", params![account.id(), folder])
                    .await?;
                self.db
                    .exec(
                        "UPDATE folders SET highest_modseq = NULL WHERE account_id = $1 AND path = $2",
                        params![account.id(), folder],
                    )
                    .await?;
                stored_modseq = None;
                let (e, a, f) = (self.clone(), account.clone(), folder.to_owned());
                tokio::spawn(async move {
                    let host = a.host();
                    let _slot = super::connect::SlotGuard::acquire(&e.bg_sem, &host).await;
                    e.backfill_messages(&a, &f).await;
                });
            }
        }
        let unread: i64 = self
            .db
            .opt(
                "SELECT COUNT(*) FILTER (WHERE is_read = false) AS n FROM messages WHERE account_id = $1 AND folder = $2",
                params![account.id(), folder],
            )
            .await?
            .map_or(0, |r| r.get("n"));
        self.db
            .exec(
                "INSERT INTO folders (account_id, path, name, total_count, unread_count, uid_validity) VALUES ($1, $2, $2, $3, $4, $5) \
                 ON CONFLICT (account_id, path) DO UPDATE SET total_count = $3, unread_count = $4, uid_validity = COALESCE($5, folders.uid_validity), updated_at = NOW()",
                params![account.id(), folder, i64::from(mailbox.exists), unread, current_validity],
            )
            .await?;
        let query = ingest_query(account, client, provider.fetch_body && !no_body_parts);
        let max_known: i64 = self
            .db
            .opt(
                "SELECT COALESCE(MAX(uid), 0) AS max_uid FROM messages WHERE account_id = $1 AND folder = $2",
                params![account.id(), folder],
            )
            .await?
            .map_or(0, |r| r.get("max_uid"));
        let max_known = max_known.max(0) as u64;
        let with_body = prefetch_body && provider.fetch_body;
        let mut new_messages: Vec<NewMessage> = Vec::new();
        let mut inserted = 0usize;
        let mut broadcasted = false;
        let mut calendar: Vec<(Message, Uuid)> = Vec::new();

        let plan = plan_modseq_sync(stored_modseq, server_modseq, validity_changed, max_known, mailbox.exists);

        let process = async |msgs: Vec<Message>,
                             new_messages: &mut Vec<NewMessage>,
                             inserted: &mut usize,
                             calendar: &mut Vec<(Message, Uuid)>| {
            for msg in msgs {
                match self.ingest_message(account, folder, &msg, with_body).await {
                    Ok(Some((Ingested::Upserted { id, is_new }, parsed))) => {
                        if is_new {
                            *inserted += 1;
                            if !parsed.is_read {
                                new_messages.push(NewMessage {
                                    id,
                                    parsed,
                                    account_id: account.id().to_owned(),
                                    folder: folder.to_owned(),
                                });
                            }
                        }
                        calendar.push((msg, id));
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!("Message sync parse error: {e}"),
                }
            }
        };

        if max_known > 0 {
            match client.fetch(&format!("{}:*", max_known + 1), &query, true, None).await {
                Ok(msgs) => process(msgs, &mut new_messages, &mut inserted, &mut calendar).await,
                Err(e) => {
                    let e = EngineError::from(e);
                    if !invalid_messageset(&e) {
                        return Err(e);
                    }
                    tracing::warn!(
                        "New-mail sync skipped for {}/{folder}: stale UID range after concurrent expunge",
                        account.log()
                    );
                }
            }
        }
        if self.flush_new_messages(account, folder, &mut new_messages).await {
            broadcasted = true;
        }

        let mut scan_complete = true;
        match plan {
            SyncPlan::Delta => {
                let low = if provider.ignores_changed_since {
                    max_known.saturating_sub(DELTA_SCAN_UID_WINDOW - 1).max(1)
                } else {
                    1
                };
                let q = FetchQuery { flags: true, ..Default::default() };
                let started = std::time::Instant::now();
                match tokio::time::timeout(
                    Duration::from_millis(FLAG_SCAN_TIMEOUT_MS),
                    client.fetch(&format!("{low}:*"), &q, true, stored_modseq),
                )
                .await
                {
                    Err(_) => {
                        scan_complete = false;
                        tracing::warn!(
                            "Delta flag scan deferred for {}/{folder}: over {FLAG_SCAN_TIMEOUT_MS}ms (provider throttling) — retrying next tick",
                            account.log()
                        );
                        // The connection is mid-command; it cannot be reused.
                        client.close();
                    }
                    Ok(Err(e)) => {
                        let e = EngineError::from(e);
                        if !invalid_messageset(&e) {
                            return Err(e);
                        }
                        scan_complete = false;
                        tracing::warn!(
                            "Delta flag scan skipped for {}/{folder}: stale range after concurrent expunge",
                            account.log()
                        );
                    }
                    Ok(Ok(msgs)) => {
                        let updates: Vec<(i64, bool, bool)> = msgs
                            .iter()
                            .filter_map(|m| {
                                m.uid.map(|u| (i64::from(u), m.has_flag("\\Seen"), m.has_flag("\\Flagged")))
                            })
                            .collect();
                        let changed = self.apply_flag_updates(account, folder, &updates).await?;
                        tracing::debug!(
                            "Delta flag scan OK for {}/{folder}: {} fetched, {changed} changed in {}ms (uid>={low})",
                            account.log(),
                            updates.len(),
                            started.elapsed().as_millis()
                        );
                        if changed > 0 {
                            self.broadcast(
                                json!({ "type": "flags_synced", "accountId": account.id() }),
                                Some(account.user_id()),
                            );
                        }
                    }
                }
            }
            SyncPlan::Full => {
                let live = client.mailbox().map_or(0, |m| m.exists);
                let range = if live > limit { format!("{}:{live}", live - limit + 1) } else { "1:*".into() };
                let scan = async {
                    let msgs = client.fetch(&range, &query, false, None).await?;
                    Ok::<_, inboxora_imap::Error>(msgs)
                };
                match tokio::time::timeout(Duration::from_millis(FLAG_SCAN_TIMEOUT_MS), scan).await {
                    Err(_) => {
                        scan_complete = false;
                        tracing::warn!(
                            "Sequence flag scan deferred for {}/{folder}: over {FLAG_SCAN_TIMEOUT_MS}ms (provider throttling) — retrying next tick",
                            account.log()
                        );
                        client.close();
                    }
                    Ok(Err(e)) => {
                        let e = EngineError::from(e);
                        if !invalid_messageset(&e) {
                            return Err(e);
                        }
                        scan_complete = false;
                        tracing::warn!(
                            "Message sync sequence scan skipped for {}/{folder}: stale sequence range after concurrent expunge",
                            account.log()
                        );
                    }
                    Ok(Ok(msgs)) => {
                        process(msgs, &mut new_messages, &mut inserted, &mut calendar).await;
                        if max_known > 0 {
                            let low = max_known.saturating_sub(DELTA_SCAN_UID_WINDOW - 1).max(1);
                            let q = FetchQuery { flags: true, ..Default::default() };
                            if let Ok(Ok(msgs)) = tokio::time::timeout(
                                Duration::from_millis(FLAG_SCAN_TIMEOUT_MS),
                                client.fetch(&format!("{low}:*"), &q, true, None),
                            )
                            .await
                            {
                                let updates: Vec<(i64, bool, bool)> = msgs
                                    .iter()
                                    .filter_map(|m| {
                                        m.uid.map(|u| (i64::from(u), m.has_flag("\\Seen"), m.has_flag("\\Flagged")))
                                    })
                                    .collect();
                                if !updates.is_empty() && self.apply_flag_updates(account, folder, &updates).await? > 0
                                {
                                    self.broadcast(
                                        json!({ "type": "flags_synced", "accountId": account.id() }),
                                        Some(account.user_id()),
                                    );
                                }
                            } else if !client.is_usable() {
                                scan_complete = false;
                            }
                        }
                    }
                }
            }
            SyncPlan::Unchanged => {}
        }
        if let Some(m) = server_modseq
            && !validity_changed
            && plan != SyncPlan::Unchanged
            && scan_complete
        {
            self.db
                .exec(
                    "UPDATE folders SET highest_modseq = $1::numeric WHERE account_id = $2 AND path = $3",
                    params![m.to_string(), account.id(), folder],
                )
                .await?;
        }
        if self.flush_new_messages(account, folder, &mut new_messages).await {
            broadcasted = true;
        }
        self.db
            .exec(
                "UPDATE folders SET unread_count = (SELECT COUNT(*) FILTER (WHERE m.is_read = false) FROM messages m WHERE m.account_id = $1 AND m.folder = $2) \
                 WHERE account_id = $1 AND path = $2",
                params![account.id(), folder],
            )
            .await?;
        stamp_last_sync(&self.db, account.id()).await?;
        if client.is_usable() {
            for (msg, id) in calendar {
                if let Err(e) =
                    crate::services::invitations::persist_inbound_from_message(&self.db, client, &msg, id).await
                {
                    tracing::warn!("Inbound calendar invitation persistence failed: {e}");
                }
            }
        }
        if !client.is_usable() {
            return Err(EngineError("Connection not available".into()));
        }
        Ok(SyncResult { inserted, broadcasted_new: broadcasted })
    }

    /// Notify, apply the block list and inbox rules, push, prefetch and learn senders for a
    /// batch of new unread messages. Returns whether `new_messages` was broadcast.
    async fn flush_new_messages(
        self: &Arc<Self>,
        account: &Account,
        folder: &str,
        batch: &mut Vec<NewMessage>,
    ) -> bool {
        if batch.is_empty() {
            return false;
        }
        let mut messages = std::mem::take(batch);
        let mut muted: HashSet<Uuid> = HashSet::new();
        if folder == "INBOX" {
            match crate::services::inbox_rules::apply_block_list(self, account, messages.clone()).await {
                Ok(m) => messages = m,
                Err(e) => tracing::error!("blockList error: {e}"),
            }
            match crate::services::inbox_rules::apply_inbox_rules(self, account, messages.clone()).await {
                Ok((remaining, m)) => {
                    messages = remaining;
                    muted = m;
                }
                Err(e) => tracing::error!("inboxRules error: {e}"),
            }
        }
        if messages.is_empty() {
            return false;
        }
        let alert: Vec<&NewMessage> = messages.iter().filter(|m| !muted.contains(&m.id)).collect();
        let tail = |v: Vec<Value>| -> Vec<Value> { v[v.len().saturating_sub(5)..].to_vec() };
        self.broadcast(
            json!({
                "type": "new_messages",
                "accountId": account.id(),
                "folder": folder,
                "messages": tail(messages.iter().map(NewMessage::to_json).collect()),
                "count": messages.len(),
                "alertMessages": tail(alert.iter().map(|m| m.to_json()).collect()),
                "alertCount": alert.len(),
            }),
            Some(account.user_id()),
        );
        if folder == "INBOX"
            && let Some(latest) = alert.last()
        {
            let (db, user, latest, count) =
                (self.db.clone(), account.user_id().to_owned(), (*latest).clone(), alert.len());
            tokio::spawn(async move {
                let unread = db
                    .opt(
                        "SELECT COUNT(*)::int AS total FROM messages m JOIN email_accounts a ON a.id = m.account_id \
                         WHERE a.user_id = $1 AND a.enabled = true AND m.folder = 'INBOX' AND m.is_read = false AND m.is_deleted = false",
                        params![&user],
                    )
                    .await
                    .ok()
                    .flatten()
                    .map(|r| i64::from(r.get::<_, i32>("total")));
                let event =
                    crate::services::push_dispatch::mail_notification_event(&user, &latest.to_json(), count, unread);
                if let Err(e) = crate::services::push_dispatch::dispatch_mail_notification(&db, event).await {
                    tracing::warn!("Push notification error: {e}");
                }
            });
        }
        let profile = account.profile();
        if messages.len() <= 5 && profile.prefetch_new_bodies {
            let limit = profile.prefetch_new_bodies_limit.unwrap_or(messages.len()).max(1);
            let warm: Vec<(Uuid, i64, String)> = messages[messages.len().saturating_sub(limit)..]
                .iter()
                .map(|m| (m.id, i64::from(m.parsed.uid), m.folder.clone()))
                .collect();
            let (e, a) = (self.clone(), account.clone());
            tokio::spawn(async move { e.prefetch_new_message_bodies(&a, &warm).await });
        }
        if folder == "INBOX" {
            static ROBOT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
                regex::Regex::new(r"(?i)^(noreply|no-reply|donotreply|mailer-daemon|notifications?|bounce[^@]*)@")
                    .expect("regex")
            });
            let senders: Vec<(String, String)> = messages
                .iter()
                .filter(|m| {
                    !m.parsed.from_email.is_empty() && !m.parsed.is_bulk && !ROBOT.is_match(&m.parsed.from_email)
                })
                .map(|m| (m.parsed.from_email.clone(), m.parsed.from_name.clone()))
                .collect();
            if !senders.is_empty() {
                let (e, user) = (self.clone(), account.user_id().to_owned());
                tokio::spawn(async move { e.upsert_auto_contacts(&user, &senders).await });
            }
        }
        true
    }

    /// Bulk-apply fetched read/star state, keeping the 30 s local-wins window.
    pub async fn apply_flag_updates(
        &self,
        account: &Account,
        folder: &str,
        updates: &[(i64, bool, bool)],
    ) -> EResult<u64> {
        if updates.is_empty() {
            return Ok(0);
        }
        let uids: Vec<i64> = updates.iter().map(|u| u.0).collect();
        let reads: Vec<bool> = updates.iter().map(|u| u.1).collect();
        let stars: Vec<bool> = updates.iter().map(|u| u.2).collect();
        Ok(self
            .db
            .exec(
                "UPDATE messages SET \
                   is_read = CASE WHEN messages.read_changed_at IS NOT NULL AND NOW() - messages.read_changed_at < interval '30 seconds' THEN messages.is_read ELSE updates.is_read END, \
                   is_starred = CASE WHEN messages.star_changed_at IS NOT NULL AND NOW() - messages.star_changed_at < interval '30 seconds' THEN messages.is_starred ELSE updates.is_starred END \
                 FROM (SELECT unnest($1::bigint[]) AS uid, unnest($2::boolean[]) AS is_read, unnest($3::boolean[]) AS is_starred) AS updates \
                 WHERE messages.account_id = $4 AND messages.folder = $5 AND messages.uid = updates.uid \
                   AND ((messages.star_changed_at IS NULL OR NOW() - messages.star_changed_at >= interval '30 seconds') AND messages.is_starred != updates.is_starred \
                     OR (messages.read_changed_at IS NULL OR NOW() - messages.read_changed_at >= interval '30 seconds') AND messages.is_read != updates.is_read)",
                params![uids, reads, stars, account.id(), folder],
            )
            .await?)
    }

    /// `_syncFlagsForRange`: flags of the newest 200 INBOX messages over a pooled connection.
    pub async fn sync_flags_for_range(self: &Arc<Self>, account: &Account) {
        if self.st.lock().syncing.contains(account.id()) {
            self.st.lock().pending_flag_sync.insert(account.id().to_owned());
            return;
        }
        let run = async {
            let mut lease = self.lease(account).await?;
            let r = async {
                let c = lease.client();
                let exists = c.select("INBOX").await?.exists;
                if exists == 0 {
                    return Ok::<_, EngineError>(0);
                }
                let range = if exists > 200 { format!("{}:{exists}", exists - 199) } else { "1:*".into() };
                let msgs =
                    c.fetch(&range, &FetchQuery { uid: true, flags: true, ..Default::default() }, false, None).await?;
                let updates: Vec<(i64, bool, bool)> = msgs
                    .iter()
                    .filter_map(|m| m.uid.map(|u| (i64::from(u), m.has_flag("\\Seen"), m.has_flag("\\Flagged"))))
                    .collect();
                if updates.is_empty() {
                    return Ok(0);
                }
                self.apply_flag_updates(account, "INBOX", &updates).await
            }
            .await;
            if r.is_err() {
                lease.evict();
            }
            r
        };
        match run.await {
            Ok(changed) if changed > 0 => {
                tracing::info!("Flag sync: {changed} flag change(s) for {}, broadcasting", account.log());
                self.broadcast(json!({ "type": "flags_synced", "accountId": account.id() }), Some(account.user_id()));
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("Flag range sync error for {}: {e}", account.log()),
        }
    }

    /// The post-tick flag poll (`setImmediate` after the tick released its sync lock).
    pub async fn sync_flags_for_range_after(self: &Arc<Self>, account: &Account) {
        for _ in 0..100 {
            if !self.st.lock().syncing.contains(account.id()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.sync_flags_for_range(account).await;
    }

    /// `syncFolderOnDemand`: the newest 100 messages of one folder over a pooled connection.
    pub async fn sync_folder_on_demand(self: &Arc<Self>, account: &Account, folder: &str) {
        let key = format!("{}:{folder}", account.id());
        let Some(_g) = self.claim(|s| &mut s.on_demand_syncing, &key) else {
            tracing::info!("syncFolderOnDemand skipped (already running): {}/{folder}", account.log());
            return;
        };
        tracing::info!("syncFolderOnDemand start: {}/{folder}", account.log());
        let run = async {
            let mut lease = self.lease(account).await?;
            let r = self.sync_messages(account, lease.client(), folder, 100, false, true).await;
            if r.is_err() {
                lease.evict();
            }
            r
        };
        match run.await {
            Ok(_) => {
                tracing::info!("syncFolderOnDemand done: {}/{folder}", account.log());
                self.broadcast(json!({ "type": "sync_complete", "accountId": account.id() }), Some(account.user_id()));
            }
            Err(e) => tracing::error!("On-demand sync error {}/{folder}: {e}", account.log()),
        }
    }

    /// `_syncOtherFolders`: every non-INBOX selectable folder on one pooled connection.
    pub async fn sync_other_folders(self: &Arc<Self>, account: &Account) {
        let Some(_g) = self.claim(|s| &mut s.other_folders_syncing, account.id()) else { return };
        let profile = account.profile();
        let run = async {
            let rows = self
                .db
                .rows("SELECT path FROM folders WHERE account_id = $1 AND path != 'INBOX' AND no_select = false ORDER BY path", params![
                    account.id()
                ])
                .await?;
            let folders: Vec<String> =
                rows.iter().map(|r| r.get::<_, String>("path")).filter(|p| !skips_folder(profile, p)).collect();
            if folders.is_empty() {
                return Ok::<_, EngineError>(());
            }
            let host = account.host();
            let _slot = super::connect::SlotGuard::acquire(&self.bg_sem, &host).await;
            let mut lease = self.lease(account).await?;
            let mut synced = false;
            for folder in folders {
                let key = format!("{}:{folder}", account.id());
                let Some(_od) = self.claim(|s| &mut s.on_demand_syncing, &key) else { continue };
                match self.sync_messages(account, lease.client(), &folder, 50, false, true).await {
                    Ok(_) => synced = true,
                    Err(e) => {
                        tracing::warn!("Periodic folder sync failed for {}/{folder}: {e}", account.log());
                        if !lease.client().is_usable() {
                            lease.evict();
                            break;
                        }
                    }
                }
            }
            if synced {
                self.broadcast(json!({ "type": "folders_synced", "accountId": account.id() }), Some(account.user_id()));
            }
            Ok(())
        };
        if let Err(e) = run.await {
            tracing::warn!("Periodic other-folders sync failed for {}: {e}", account.log());
        }
    }

    /// `reconcileDeletes`: drop rows whose UIDs the server no longer has.
    pub async fn reconcile_deletes(self: &Arc<Self>, account: &Account) -> EResult<()> {
        let Some(_g) = self.claim(|s| &mut s.reconciling, account.id()) else { return Ok(()) };
        let started: DateTime<Utc> = Utc::now();
        let rows = self
            .db
            .rows(
                "SELECT DISTINCT m.folder FROM messages m WHERE m.account_id = $1 \
                 AND EXISTS (SELECT 1 FROM folders f WHERE f.account_id = m.account_id AND f.path = m.folder)",
                params![account.id()],
            )
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let folders: Vec<String> = rows.iter().map(|r| r.get::<_, String>("folder")).collect();
        let mut server: Vec<(String, HashSet<i64>)> = Vec::new();
        {
            let mut lease = match self.lease(account).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("Reconcile connection error for {}: {e}", account.log());
                    return Ok(());
                }
            };
            for folder in folders {
                let c = lease.client();
                let r = async {
                    c.select(&folder).await?;
                    c.search_uids("ALL").await
                }
                .await;
                match r {
                    Ok(uids) => server.push((folder, uids.into_iter().map(i64::from).collect())),
                    Err(e) => {
                        tracing::warn!("Reconcile: could not open {}/{folder}: {e}", account.log());
                        if e.is_fatal() {
                            lease.evict();
                            tracing::warn!("Reconcile connection error for {}: {e}", account.log());
                            return Ok(());
                        }
                    }
                }
            }
        }
        let mut deleted = 0usize;
        for (folder, set) in server {
            let db_rows = self
                .db
                .rows(
                    "SELECT uid FROM messages WHERE account_id = $1 AND folder = $2 AND (synced_at IS NULL OR synced_at < $3)",
                    params![account.id(), &folder, started],
                )
                .await?;
            let orphans: Vec<i64> = db_rows
                .iter()
                .map(|r| r.get::<_, i64>("uid"))
                .filter(|u| !set.contains(u) && !self.is_move_uid_guarded(account.id(), &folder, *u))
                .collect();
            if orphans.is_empty() {
                continue;
            }
            tracing::info!(
                "Reconcile: removing {} server-deleted message(s) from {}/{folder}",
                orphans.len(),
                account.log()
            );
            self.db
                .exec(
                    "DELETE FROM messages WHERE account_id = $1 AND folder = $2 AND uid = ANY($3::bigint[]) AND (synced_at IS NULL OR synced_at < $4)",
                    params![account.id(), &folder, orphans.clone(), started],
                )
                .await?;
            recount_folder(&self.db, account.id(), &folder).await?;
            deleted += orphans.len();
        }
        if deleted > 0 {
            self.broadcast(json!({ "type": "sync_complete", "accountId": account.id() }), Some(account.user_id()));
        }
        Ok(())
    }
}

pub async fn stamp_last_sync(db: &Db, account_id: &str) -> EResult<()> {
    db.exec("UPDATE email_accounts SET last_sync = NOW() WHERE id = $1", params![account_id]).await?;
    Ok(())
}

/// Recompute a folder's cached total/unread counts from its rows.
pub async fn recount_folder(db: &Db, account_id: &str, folder: &str) -> EResult<()> {
    db.exec(
        "UPDATE folders f SET total_count = (SELECT COUNT(*) FROM messages m WHERE m.account_id = $1 AND m.folder = $2), \
         unread_count = (SELECT COUNT(*) FILTER (WHERE m.is_read = false) FROM messages m WHERE m.account_id = $1 AND m.folder = $2) \
         WHERE f.account_id = $1 AND f.path = $2",
        params![account_id, folder],
    )
    .await?;
    Ok(())
}
