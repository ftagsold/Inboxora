//! User-driven IMAP operations over the pool: bodies, headers and attachments, flags,
//! folders, moves, copies and deletes, APPEND and Sent/Draft records.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use inboxora_core::params;
use inboxora_core::pg::Q;
use inboxora_imap::{Client, CreateResult, FetchQuery, uid_set};
use serde_json::{Value, json};
use uuid::Uuid;

use super::connect::{Account, load_account};
use super::mime::{self, AttachmentRef, Collected, decode_body, looks_like_text_payload, walk_structure};
use super::parse::{clean, safe_date};
use super::policy::{BODY_PREFETCH_PARTS, classify_move_by_search};
use super::sync::compute_thread_id;
use super::{EResult, Engine, EngineError, FlagOp};
use crate::services::conversation::{RawCopy, persist_copy_for_row};
use crate::services::mail_utils::{adjust_folder_counts, relocate_insert_cols};

/// Run `$body` with a pooled client bound to `$c`; the connection is evicted on error.
macro_rules! pooled {
    ($self:ident, $account:expr, |$c:ident| $body:block) => {
        async {
            let mut lease = $self.lease($account).await?;
            let r: EResult<_> = async {
                let $c: &mut Client = lease.client();
                $body
            }
            .await;
            if r.is_err() {
                lease.evict();
            }
            r
        }
        .await
    };
}

#[derive(Debug, Clone, Default)]
pub struct Body {
    pub html: Option<String>,
    pub text: Option<String>,
    pub attachments: Vec<AttachmentRef>,
}

#[derive(Debug, Clone, Default)]
pub struct BulkMove {
    pub uid_map: HashMap<u32, u32>,
    pub succeeded: Vec<u32>,
    pub failed: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentLookup {
    Found(u32),
    Missing,
    Ambiguous,
}

/// Fields of an authoritative Sent/Draft record written right after APPEND.
#[derive(Debug, Clone, Default)]
pub struct RecordFields {
    pub message_id: String,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub to: Value,
    pub cc: Value,
    pub bcc: Value,
    pub snippet: String,
    pub date: Option<DateTime<Utc>>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub body_html: Option<String>,
    pub body_text: Option<String>,
    pub alias_id: Option<String>,
    pub composition: Option<Value>,
    pub uid_validity: Option<i64>,
}

fn arr(v: &Value) -> String {
    if v.is_array() { v.to_string() } else { "[]".into() }
}

fn is_transient(detail: &str) -> bool {
    let l = detail.to_lowercase();
    detail == "Command failed"
        || l.contains("command canceled")
        || detail.contains("ECONNRESET")
        || l.contains("socket hang up")
        || detail.contains("ETIMEDOUT")
        || l.contains("timed out")
        || detail.contains("EPIPE")
        || detail == "Socket timeout"
        || detail == "Connection closed"
        || detail == "Connection not available"
}

async fn fetch_body_with(client: &mut Client, profile_speculative: bool, uid: u32, folder: &str) -> EResult<Body> {
    let set = uid.to_string();
    client.select(folder).await?;
    let mut prefetched: HashMap<String, Vec<u8>> = HashMap::new();
    let structure_q = FetchQuery { uid: true, body_structure: true, ..Default::default() };
    let mut structure = None;
    if profile_speculative {
        let q = FetchQuery {
            uid: true,
            body_structure: true,
            body_parts: BODY_PREFETCH_PARTS.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        };
        match client.fetch(&set, &q, true, None).await {
            Ok(msgs) => {
                for m in msgs {
                    if m.body_structure.is_some() {
                        structure = m.body_structure;
                    }
                    prefetched.extend(m.body_parts.into_iter().filter(|(_, v)| !v.is_empty()));
                }
            }
            Err(e) if e.is_fatal() => return Err(e.into()),
            Err(_) => {
                prefetched.clear();
                for m in client.fetch(&set, &structure_q, true, None).await? {
                    structure = m.body_structure.or(structure);
                }
            }
        }
    } else {
        for m in client.fetch(&set, &structure_q, true, None).await? {
            structure = m.body_structure.or(structure);
        }
    }
    let Some(structure) = structure else { return Err(EngineError("Command failed".into())) };
    let mut c = Collected::default();
    walk_structure(&structure, &mut c);
    if mime::should_fallback_to_text_part(&c) {
        let root = structure.mime_type.to_lowercase();
        c.text_parts.push(mime::PartRef {
            part: if structure.part.is_empty() { "1".into() } else { structure.part.clone() },
            mime_type: if matches!(root.as_str(), "text/html" | "text/plain" | "application/xhtml+xml") {
                "text/html".into()
            } else {
                "text/plain".into()
            },
            encoding: structure.encoding.clone(),
            charset: structure.charset.clone().unwrap_or_else(|| "utf-8".into()),
            cid: None,
        });
    }
    let mut needed: Vec<String> = Vec::new();
    for p in c.text_parts.iter().map(|p| &p.part).chain(c.inline_images.iter().map(|p| &p.part)) {
        if !prefetched.contains_key(p) && !needed.contains(p) {
            needed.push(p.clone());
        }
    }
    if !needed.is_empty() {
        let q = FetchQuery { uid: true, body_parts: needed, ..Default::default() };
        for m in client.fetch(&set, &q, true, None).await? {
            prefetched.extend(m.body_parts);
        }
    }
    let single = |part: &str| FetchQuery { uid: true, body_parts: vec![part.to_owned()], ..Default::default() };
    for p in &c.text_parts {
        match client.fetch(&set, &single(&p.part), true, None).await {
            Ok(msgs) => {
                for m in msgs {
                    if let Some(v) = m.body_parts.get(&p.part).filter(|v| !v.is_empty()) {
                        prefetched.insert(p.part.clone(), v.clone());
                    }
                }
            }
            Err(e) if e.is_fatal() => return Err(e.into()),
            Err(_) => {}
        }
    }
    for img in &c.inline_images {
        if !prefetched.get(&img.part).is_some_and(|b| looks_like_text_payload(b)) {
            continue;
        }
        if let Ok(msgs) = client.fetch(&set, &single(&img.part), true, None).await {
            for m in msgs {
                if let Some(v) = m.body_parts.get(&img.part).filter(|v| !v.is_empty()) {
                    prefetched.insert(img.part.clone(), v.clone());
                }
            }
        }
    }
    let (mut html, mut text) = (None::<String>, None::<String>);
    for p in &c.text_parts {
        let Some(buf) = prefetched.get(&p.part) else { continue };
        let decoded = decode_body(buf, &p.encoding, &p.charset);
        if p.mime_type == "text/html" && html.is_none() {
            html = Some(decoded);
        } else if p.mime_type == "text/plain" && text.is_none() {
            text = Some(decoded);
        }
    }
    if let Some(h) = html.as_mut() {
        for img in &c.inline_images {
            let Some(cid) = img.cid.as_deref().filter(|c| !c.is_empty()) else { continue };
            let Some(buf) = prefetched.get(&img.part) else { continue };
            if looks_like_text_payload(buf) {
                continue;
            }
            let b64 = if img.encoding.eq_ignore_ascii_case("base64") {
                buf.iter().filter(|b| !b.is_ascii_whitespace()).map(|b| *b as char).collect::<String>()
            } else {
                base64::engine::general_purpose::STANDARD.encode(buf)
            };
            let uri = format!("data:{};base64,{b64}", img.mime_type);
            let re =
                regex::RegexBuilder::new(&format!("cid:<?{}>?", regex::escape(cid))).case_insensitive(true).build();
            if let Ok(re) = re {
                *h = re.replace_all(h, regex::NoExpand(&uri)).into_owned();
            }
        }
    }
    Ok(Body { html: html.map(|h| clean(&h)), text: text.map(|t| clean(&t)), attachments: c.attachments })
}

impl Engine {
    /// `fetchMessageBody`: pool first (or a fresh login for providers that prefer it), one
    /// retry on a fresh login for transient failures.
    pub async fn fetch_message_body(self: &Arc<Self>, account: &Account, uid: i64, folder: &str) -> EResult<Body> {
        let uid = u32::try_from(uid).map_err(|_| EngineError("Command failed".into()))?;
        let speculative = account.profile().speculative_fetch;
        let first = if account.profile().prefer_fresh_body_fetch {
            self.fetch_body_fresh(account, speculative, uid, folder).await
        } else {
            pooled!(self, account, |c| { fetch_body_with(c, speculative, uid, folder).await })
        };
        match first {
            Ok(b) => Ok(b),
            Err(e) if is_transient(&e.0) => match self.fetch_body_fresh(account, speculative, uid, folder).await {
                Ok(b) => Ok(b),
                Err(r) if r.0 == "Command failed" => {
                    tracing::warn!(
                        "fetchMessageBody: uid={uid} folder={folder} account={} — no body after retry; message may be missing on server",
                        account.log()
                    );
                    Ok(Body::default())
                }
                Err(r) => Err(r),
            },
            Err(e) => Err(e),
        }
    }

    async fn fetch_body_fresh(&self, account: &Account, speculative: bool, uid: u32, folder: &str) -> EResult<Body> {
        let (_, mut c) = self.open_client(account, "IMAP fresh-login connect").await?;
        let r = fetch_body_with(&mut c, speculative, uid, folder).await;
        c.close();
        r
    }

    /// `fetchHeaders`: the raw header block (falling back to the leading source bytes).
    pub async fn fetch_headers(self: &Arc<Self>, account: &Account, uid: i64, folder: &str) -> EResult<String> {
        pooled!(self, account, |c| {
            c.select(folder).await?;
            let set = uid.to_string();
            let mut headers = String::new();
            for m in c.fetch(&set, &FetchQuery { uid: true, headers: true, ..Default::default() }, true, None).await? {
                if let Some(h) = m.headers {
                    headers = String::from_utf8_lossy(&h).into_owned();
                }
            }
            if headers.trim().is_empty() {
                let q = FetchQuery { uid: true, source: Some((0, Some(65536))), ..Default::default() };
                for m in c.fetch(&set, &q, true, None).await? {
                    if let Some(src) = m.source {
                        let raw = String::from_utf8_lossy(&src).into_owned();
                        static SEP: std::sync::LazyLock<regex::Regex> =
                            std::sync::LazyLock::new(|| regex::Regex::new(r"\r?\n\r?\n").expect("regex"));
                        headers = SEP.find(&raw).map_or(raw.clone(), |m| raw[..m.start()].to_owned());
                        break;
                    }
                }
            }
            Ok(headers)
        })
    }

    /// `fetchAttachment`: one decoded part.
    pub async fn fetch_attachment(
        self: &Arc<Self>,
        account: &Account,
        uid: i64,
        folder: &str,
        part: &str,
    ) -> EResult<Option<Vec<u8>>> {
        pooled!(self, account, |c| {
            c.select(folder).await?;
            let q =
                FetchQuery { uid: true, body_structure: true, body_parts: vec![part.to_owned()], ..Default::default() };
            let mut out = None;
            for m in c.fetch(&uid.to_string(), &q, true, None).await? {
                let mut encoding = "base64".to_owned();
                if let Some(s) = &m.body_structure {
                    let mut col = Collected::default();
                    walk_structure(s, &mut col);
                    encoding = mime::attachment_transfer_encoding(&col, part, "base64");
                }
                if let Some(buf) = m.body_parts.get(part) {
                    out = Some(mime::decode_attachment(buf, &encoding));
                }
            }
            Ok(out)
        })
    }

    /// `fetchMultipleAttachments`: several decoded parts in one round trip.
    pub async fn fetch_multiple_attachments(
        self: &Arc<Self>,
        account: &Account,
        uid: i64,
        folder: &str,
        parts: &[(String, Option<String>)],
    ) -> EResult<HashMap<String, Vec<u8>>> {
        pooled!(self, account, |c| {
            c.select(folder).await?;
            let q = FetchQuery {
                uid: true,
                body_structure: true,
                body_parts: parts.iter().map(|p| p.0.clone()).collect(),
                ..Default::default()
            };
            let mut out = HashMap::new();
            for m in c.fetch(&uid.to_string(), &q, true, None).await? {
                let mut live: HashMap<String, String> = HashMap::new();
                if let Some(s) = &m.body_structure {
                    let mut col = Collected::default();
                    walk_structure(s, &mut col);
                    live.extend(col.attachments.into_iter().map(|a| (a.part, a.encoding)));
                }
                for (part, buf) in m.body_parts {
                    if buf.is_empty() {
                        continue;
                    }
                    let enc = live
                        .get(&part)
                        .cloned()
                        .filter(|e| !e.is_empty())
                        .or_else(|| parts.iter().find(|p| p.0 == part).and_then(|p| p.1.clone()))
                        .unwrap_or_else(|| "base64".into());
                    out.insert(part, mime::decode_attachment(&buf, &enc));
                }
            }
            Ok(out)
        })
    }

    /// `setFlag`: up to two attempts, the second on a fresh pooled connection.
    pub async fn set_flag(
        self: &Arc<Self>,
        account: &Account,
        uid: i64,
        folder: &str,
        flag: &str,
        value: bool,
    ) -> EResult<()> {
        tracing::info!("setFlag: uid={uid} folder={folder} flag={flag} value={value}");
        let mut last = EngineError("setFlag failed".into());
        for attempt in 1..=2 {
            let r = pooled!(self, account, |c| {
                c.select(folder).await?;
                c.store_flags(&uid.to_string(), value, &[flag]).await?;
                Ok(())
            });
            match r {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = e;
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    }
                }
            }
        }
        tracing::error!("setFlag failed after retry: uid={uid} {flag}={value}: {last}");
        Err(last)
    }

    pub async fn create_folder(self: &Arc<Self>, account: &Account, path: &str) -> EResult<()> {
        pooled!(self, account, |c| {
            c.create(&[path]).await?;
            Ok(())
        })
    }

    /// `ensureFolder` / `ensureMailbox`: create idempotently and report the real path.
    pub async fn ensure_folder(
        self: &Arc<Self>,
        account: &Account,
        path: &str,
        resolve_path: bool,
    ) -> EResult<CreateResult> {
        pooled!(self, account, |c| { ensure_mailbox(c, path, resolve_path).await })
    }

    /// `moveMessage` / `moveMessageGetNewUid`: the destination UID when UIDPLUS reports it.
    pub async fn move_message(
        self: &Arc<Self>,
        account: &Account,
        uid: i64,
        from: &str,
        to: &str,
    ) -> EResult<Option<i64>> {
        let r = pooled!(self, account, |c| {
            c.select(from).await?;
            let res = c.mv(&uid.to_string(), to).await?;
            Ok(res.uid_map.and_then(|m| m.get(&(uid as u32)).copied()).map(i64::from))
        });
        if let Err(e) = &r {
            tracing::error!("moveMessage failed: uid={uid}: {e}");
        }
        r
    }

    pub async fn delete_folder(self: &Arc<Self>, account: &Account, path: &str) -> EResult<()> {
        pooled!(self, account, |c| {
            if c.mailbox().is_some_and(|m| m.path.eq_ignore_ascii_case(path)) {
                c.select("INBOX").await?;
            }
            c.delete_mailbox(path).await?;
            Ok(())
        })
    }

    pub async fn rename_folder(self: &Arc<Self>, account: &Account, from: &str, to: &str) -> EResult<()> {
        pooled!(self, account, |c| {
            c.rename_mailbox(from, to).await?;
            Ok(())
        })
    }

    /// Apply a chunked whole-folder write (`_chunkedFolderOp`).
    async fn chunked_op(c: &mut Client, folder: &str, search: &str, label: &str, delete: bool) -> EResult<usize> {
        let mut uids = c.search_uids(search).await?;
        uids.sort_unstable();
        if uids.is_empty() {
            return Ok(0);
        }
        let mut done = 0;
        for chunk in uids.chunks(500) {
            let range = chunk.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            let mut ok = false;
            for attempt in 0..2 {
                let r = if delete {
                    c.delete_messages(&range).await
                } else {
                    c.store_flags(&range, true, &["\\Seen"]).await
                };
                match r {
                    Ok(()) => {
                        ok = true;
                        break;
                    }
                    Err(e) if attempt == 1 || e.is_fatal() => return Err(e.into()),
                    Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
                }
            }
            if !ok {
                return Err(EngineError(format!(
                    "{label} could not be confirmed for {folder} after {done}/{} messages",
                    uids.len()
                )));
            }
            done += chunk.len();
        }
        Ok(done)
    }

    pub async fn empty_folder(self: &Arc<Self>, account: &Account, folder: &str) -> EResult<()> {
        pooled!(self, account, |c| {
            let exists = c.select(folder).await?.exists;
            if exists == 0 {
                return Ok(());
            }
            match Self::chunked_op(c, folder, "ALL", "messageDelete", true).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    let m = e.0.to_lowercase();
                    if m.contains("no messages") || m.contains("empty") || m.contains("nothing") {
                        Ok(())
                    } else {
                        Err(e)
                    }
                }
            }
        })
    }

    pub async fn mark_all_read_imap(self: &Arc<Self>, account: &Account, folder: &str) -> EResult<()> {
        pooled!(self, account, |c| {
            let exists = c.select(folder).await?.exists;
            if exists == 0 {
                return Ok(());
            }
            if let Err(e) = Self::chunked_op(c, folder, "UNSEEN", "messageFlagsAdd", false).await {
                tracing::warn!("markAllRead IMAP warning for {folder}: {e}");
                if !c.is_usable() {
                    return Err(e);
                }
            }
            Ok(())
        })
    }

    pub async fn permanent_delete_message(
        self: &Arc<Self>,
        account: &Account,
        uid: i64,
        folder: &str,
        expected_validity: Option<i64>,
    ) -> EResult<()> {
        pooled!(self, account, |c| {
            let mb = c.select(folder).await?.clone();
            if let Some(expected) = expected_validity
                && mb.uid_validity.map(i64::from) != Some(expected)
            {
                return Err(EngineError(format!(
                    "messageDelete refused: UIDVALIDITY changed or is unavailable for {folder}"
                )));
            }
            c.delete_messages(&uid.to_string()).await?;
            Ok(())
        })
    }

    /// `copyMessage`: COPY to `to` and insert the sibling row when UIDPLUS names it.
    pub async fn copy_message(
        self: &Arc<Self>,
        account_id: &str,
        uid: i64,
        from: &str,
        to: &str,
    ) -> EResult<Option<i64>> {
        let account = load_account(&self.db, account_id)
            .await?
            .ok_or_else(|| EngineError(format!("copyMessage: account {account_id} not found")))?;
        let new_uid = pooled!(self, &account, |c| {
            c.select(from).await?;
            let res = c.copy(&uid.to_string(), to).await?;
            Ok(res.uid_map.and_then(|m| m.get(&(uid as u32)).copied()).map(i64::from))
        })
        .inspect_err(|e| tracing::error!("copyMessage failed: uid={uid}: {e}"))?;
        let Some(new_uid) = new_uid else { return Ok(None) };
        self.insert_copied_sibling(account_id, uid, from, to, new_uid).await?;
        Ok(Some(new_uid))
    }

    /// `insertCopiedSibling`.
    pub async fn insert_copied_sibling(
        &self,
        account_id: &str,
        uid: i64,
        from: &str,
        to: &str,
        new_uid: i64,
    ) -> EResult<Option<Uuid>> {
        let cols = crate::services::mail_utils::RELOCATE_COPY_COLS.join(", ");
        let row = self
            .db
            .opt(
                &format!(
                    "INSERT INTO messages (account_id, uid, folder, {cols}) SELECT account_id, $4, $5, {cols} FROM messages \
                     WHERE account_id = $1 AND folder = $2 AND uid = $3 ON CONFLICT (account_id, uid, folder) DO NOTHING RETURNING id, is_read"
                ),
                params![account_id, from, uid, new_uid, to],
            )
            .await?;
        Ok(row.map(|r| {
            let read: Option<bool> = r.get("is_read");
            adjust_folder_counts(&self.db, account_id, to, 1, if read.unwrap_or(false) { 0 } else { 1 });
            r.get("id")
        }))
    }

    /// `removeMessageCopy`: delete one folder's copy on the server and its row.
    pub async fn remove_message_copy(self: &Arc<Self>, account_id: &str, uid: i64, folder: &str) -> EResult<u64> {
        let account = load_account(&self.db, account_id)
            .await?
            .ok_or_else(|| EngineError(format!("removeMessageCopy: account {account_id} not found")))?;
        self.permanent_delete_message(&account, uid, folder, None).await?;
        let row = self
            .db
            .opt(
                "DELETE FROM messages WHERE account_id = $1 AND uid = $2 AND folder = $3 RETURNING is_read",
                params![account_id, uid, folder],
            )
            .await?;
        Ok(match row {
            Some(r) => {
                let read: Option<bool> = r.get("is_read");
                adjust_folder_counts(&self.db, account_id, folder, -1, if read.unwrap_or(false) { 0 } else { -1 });
                1
            }
            None => 0,
        })
    }

    /// `bulkMoveMessages`: one MOVE; without a UIDPLUS map, reconcile by UID SEARCH (#407).
    pub async fn bulk_move_messages(
        self: &Arc<Self>,
        account: &Account,
        uids: &[u32],
        from: &str,
        to: &str,
    ) -> BulkMove {
        if uids.is_empty() {
            return BulkMove::default();
        }
        let dest_next = match pooled!(self, account, |c| { Ok(c.status(to).await?) }) {
            Ok(s) => s.uid_next,
            Err(e) => {
                tracing::warn!("bulkMoveMessages STATUS {to} failed ({e}) — reconciliation skipped");
                None
            }
        };
        let set = uid_set(uids);
        let moved = pooled!(self, account, |c| {
            c.select(from).await?;
            let r = c.mv(&set, to).await?;
            Ok(r.uid_map.filter(|m| !m.is_empty()))
        });
        match moved {
            Ok(Some(map)) => {
                let succeeded: Vec<u32> = uids.iter().copied().filter(|u| map.contains_key(u)).collect();
                let failed: Vec<u32> = uids.iter().copied().filter(|u| !map.contains_key(u)).collect();
                if !failed.is_empty() {
                    self.record_signal("stale_mutation_uid", account.id(), Some(failed.len() as f64));
                }
                BulkMove { uid_map: map, succeeded, failed }
            }
            Ok(None) => {
                let (res, stale) = self.reconcile_move_by_search(account, uids, from, to, dest_next).await;
                if let Some(n) = stale.filter(|n| *n > 0) {
                    self.record_signal("stale_mutation_uid", account.id(), Some(n as f64));
                    if to != from {
                        let (e, a, f) = (self.clone(), account.clone(), to.to_owned());
                        tokio::spawn(async move { e.sync_folder_on_demand(&a, &f).await });
                    }
                }
                res
            }
            Err(e) => {
                tracing::warn!("bulkMoveMessages {from} → {to}: batch failed ({e}), verifying via UID SEARCH");
                self.reconcile_move_by_search(account, uids, from, to, dest_next).await.0
            }
        }
    }

    async fn reconcile_move_by_search(
        self: &Arc<Self>,
        account: &Account,
        uids: &[u32],
        from: &str,
        to: &str,
        dest_next: Option<u32>,
    ) -> (BulkMove, Option<usize>) {
        let set = uid_set(uids);
        let remaining = match pooled!(self, account, |c| {
            c.select(from).await?;
            Ok(c.search_uids(&format!("UID {set}")).await?)
        }) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    "bulkMoveMessages: source UID SEARCH failed ({e}) — leaving all {} for next sync",
                    uids.len()
                );
                return (BulkMove { failed: uids.to_vec(), ..Default::default() }, None);
            }
        };
        let mut dest_new: Vec<u32> = Vec::new();
        let mut arrived = None;
        if let Some(next) = dest_next {
            match pooled!(self, account, |c| {
                c.select(to).await?;
                Ok(c.search_uids(&format!("UID {next}:*")).await?)
            }) {
                Ok(v) => {
                    // `n:*` reports the highest UID even below n.
                    dest_new = v.into_iter().filter(|u| *u >= next).collect();
                    arrived = Some(dest_new.len());
                }
                Err(e) => {
                    tracing::warn!("bulkMoveMessages: destination verification failed ({e}) — trusting source-absence")
                }
            }
        }
        let c = classify_move_by_search(uids, &remaining, arrived);
        if let Some(n) = c.stale_count.filter(|n| *n > 0) {
            tracing::warn!(
                "bulkMoveMessages {from} → {to}: {n} stale UID(s) in batch — reporting all {} failed for the next sync to reconcile",
                uids.len()
            );
        }
        let mut map = HashMap::new();
        if c.mappable {
            let mut src = c.succeeded.clone();
            src.sort_unstable();
            dest_new.sort_unstable();
            map.extend(src.into_iter().zip(dest_new));
        }
        (BulkMove { uid_map: map, succeeded: c.succeeded, failed: c.failed }, c.stale_count)
    }

    /// `bulkPermanentDelete`: flag + (UID) EXPUNGE, protecting other `\Deleted` messages
    /// without UIDPLUS; verified by search on failure.
    pub async fn bulk_permanent_delete(
        self: &Arc<Self>,
        account: &Account,
        uids: &[u32],
        folder: &str,
    ) -> (Vec<u32>, Vec<u32>) {
        if uids.is_empty() {
            return (vec![], vec![]);
        }
        let set = uid_set(uids);
        let r = pooled!(self, account, |c| {
            c.select(folder).await?;
            if c.has_capability("UIDPLUS") {
                c.delete_messages(&set).await?;
            } else {
                let ours: std::collections::HashSet<u32> = uids.iter().copied().collect();
                let others: Vec<u32> =
                    c.search_uids("DELETED").await?.into_iter().filter(|u| !ours.contains(u)).collect();
                if !others.is_empty() {
                    c.store_flags(&uid_set(&others), false, &["\\Deleted"]).await?;
                }
                let r = c.delete_messages(&set).await;
                if !others.is_empty() {
                    c.store_flags(&uid_set(&others), true, &["\\Deleted"]).await?;
                }
                r?;
            }
            Ok(())
        });
        match r {
            Ok(()) => (uids.to_vec(), vec![]),
            Err(e) => {
                tracing::warn!("bulkPermanentDelete {folder}: batch failed ({e}), verifying via UID SEARCH");
                match pooled!(self, account, |c| {
                    c.select(folder).await?;
                    Ok(c.search_uids(&format!("UID {set}")).await?)
                }) {
                    Ok(remaining) => {
                        let rem: std::collections::HashSet<u32> = remaining.into_iter().collect();
                        let succeeded: Vec<u32> = uids.iter().copied().filter(|u| !rem.contains(u)).collect();
                        let failed: Vec<u32> = uids.iter().copied().filter(|u| rem.contains(u)).collect();
                        if !succeeded.is_empty() {
                            tracing::info!(
                                "bulkPermanentDelete: {}/{} messages confirmed deleted via UID SEARCH",
                                succeeded.len(),
                                uids.len()
                            );
                        }
                        (succeeded, failed)
                    }
                    Err(e) => {
                        tracing::error!("bulkPermanentDelete: UID SEARCH verification failed: {e}");
                        (vec![], uids.to_vec())
                    }
                }
            }
        }
    }

    /// `appendToFolder`: APPEND and report (uid, uidvalidity) when the server says.
    pub async fn append_to_folder(
        self: &Arc<Self>,
        account: &Account,
        folder: &str,
        raw: &[u8],
        flags: &[&str],
    ) -> EResult<(Option<i64>, Option<i64>)> {
        let r = pooled!(self, account, |c| {
            let mb = c.select(folder).await?.clone();
            let appended = c.append(folder, flags, None, raw).await?;
            let validity = appended.map(|(v, _)| v).or(mb.uid_validity).filter(|v| *v > 0).map(i64::from);
            Ok((appended.map(|(_, u)| i64::from(u)), validity))
        })?;
        tracing::info!(
            "Appended to IMAP {}/{folder} uid={}",
            account.log(),
            r.0.map_or_else(|| "null".to_owned(), |u| u.to_string())
        );
        Ok(r)
    }

    /// `upsertSentMessageRecord`.
    pub async fn upsert_sent_record(&self, account: &Account, folder: &str, uid: i64, f: &RecordFields) -> EResult<()> {
        if uid == 0 || folder.is_empty() {
            return Ok(());
        }
        let mid = clean(&f.message_id);
        let thread = if mid.is_empty() {
            None
        } else {
            compute_thread_id(
                &self.db,
                account.id(),
                &mid,
                &clean(f.in_reply_to.as_deref().unwrap_or("")),
                &clean(f.references.as_deref().unwrap_or("")),
            )
            .await?
        };
        let nonempty = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(clean);
        self.db
            .exec(
                "INSERT INTO messages (account_id, uid, folder, message_id, subject, from_name, from_email, to_addresses, cc_addresses, \
                   in_reply_to, thread_references, date, snippet, is_read, is_starred, has_attachments, flags, thread_id) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8::jsonb,$9::jsonb,$10,$11,$12,$13,true,false,false,'[]',$14) \
                 ON CONFLICT (account_id, uid, folder) DO UPDATE SET \
                   message_id = COALESCE(EXCLUDED.message_id, messages.message_id), \
                   subject = CASE WHEN EXCLUDED.subject IS NOT NULL AND EXCLUDED.subject <> '' AND EXCLUDED.subject <> '(no subject)' THEN EXCLUDED.subject ELSE messages.subject END, \
                   from_name = COALESCE(NULLIF(EXCLUDED.from_name, ''), messages.from_name), \
                   from_email = COALESCE(NULLIF(EXCLUDED.from_email, ''), messages.from_email), \
                   to_addresses = CASE WHEN EXCLUDED.to_addresses::text IS NOT NULL AND EXCLUDED.to_addresses::text <> '[]' THEN EXCLUDED.to_addresses ELSE messages.to_addresses END, \
                   cc_addresses = CASE WHEN EXCLUDED.cc_addresses::text IS NOT NULL AND EXCLUDED.cc_addresses::text <> '[]' THEN EXCLUDED.cc_addresses ELSE messages.cc_addresses END, \
                   in_reply_to = COALESCE(EXCLUDED.in_reply_to, messages.in_reply_to), \
                   thread_references = COALESCE(EXCLUDED.thread_references, messages.thread_references), \
                   date = EXCLUDED.date, \
                   snippet = CASE WHEN EXCLUDED.snippet <> '' THEN EXCLUDED.snippet ELSE messages.snippet END, \
                   is_read = true, \
                   thread_id = CASE WHEN messages.thread_id = messages.message_id AND EXCLUDED.thread_id IS NOT NULL AND EXCLUDED.thread_id <> messages.message_id \
                                    THEN EXCLUDED.thread_id ELSE COALESCE(messages.thread_id, EXCLUDED.thread_id) END",
                params![
                    account.id(),
                    uid,
                    folder,
                    mid.clone(),
                    clean(f.subject.as_deref().filter(|s| !s.is_empty()).unwrap_or("(no subject)")),
                    clean(f.from_name.as_deref().unwrap_or("")),
                    clean(f.from_email.as_deref().unwrap_or("")),
                    arr(&f.to),
                    arr(&f.cc),
                    nonempty(f.in_reply_to.as_deref()).unwrap_or_default(),
                    nonempty(f.references.as_deref()).unwrap_or_default(),
                    safe_date(f.date),
                    clean(&f.snippet),
                    thread
                ],
            )
            .await?;
        self.persist_record_copy(account, folder, uid, f).await
    }

    async fn persist_record_copy(&self, account: &Account, folder: &str, uid: i64, f: &RecordFields) -> EResult<()> {
        let row = self
            .db
            .opt(
                "SELECT id FROM messages WHERE account_id = $1 AND uid = $2 AND folder = $3",
                params![account.id(), uid, folder],
            )
            .await?;
        if let Some(r) = row {
            persist_copy_for_row(
                &self.db,
                r.get("id"),
                account.id(),
                account.user_id(),
                &account.host(),
                &RawCopy { message_id: Some(&f.message_id), ..Default::default() },
            )
            .await;
        }
        Ok(())
    }

    /// `upsertDraftMessageRecord`: an authoritative snapshot of the appended draft.
    pub async fn upsert_draft_record(
        &self,
        account: &Account,
        folder: &str,
        uid: i64,
        f: &RecordFields,
    ) -> EResult<()> {
        if uid == 0 || folder.is_empty() {
            return Ok(());
        }
        let mid = clean(&f.message_id);
        self.db
            .exec(
                "INSERT INTO messages (account_id, uid, folder, message_id, subject, from_name, from_email, to_addresses, cc_addresses, \
                   in_reply_to, date, snippet, is_read, is_starred, has_attachments, flags, body_html, body_text, thread_id, draft_uid_validity, \
                   draft_bcc_addresses, draft_alias_id, draft_in_reply_to, draft_references, draft_composition) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8::jsonb,$9::jsonb,$10,$11,$12,true,false,false,$13::jsonb,$14,$15,$16,$17,$18::jsonb,$19,$20,$21,$22::jsonb) \
                 ON CONFLICT (account_id, uid, folder) DO UPDATE SET \
                   message_id = EXCLUDED.message_id, subject = EXCLUDED.subject, from_name = EXCLUDED.from_name, from_email = EXCLUDED.from_email, \
                   to_addresses = EXCLUDED.to_addresses, cc_addresses = EXCLUDED.cc_addresses, in_reply_to = EXCLUDED.in_reply_to, date = EXCLUDED.date, \
                   snippet = EXCLUDED.snippet, flags = EXCLUDED.flags, body_html = EXCLUDED.body_html, body_text = EXCLUDED.body_text, \
                   draft_uid_validity = EXCLUDED.draft_uid_validity, draft_bcc_addresses = EXCLUDED.draft_bcc_addresses, \
                   draft_alias_id = EXCLUDED.draft_alias_id, draft_in_reply_to = EXCLUDED.draft_in_reply_to, \
                   draft_references = EXCLUDED.draft_references, draft_composition = EXCLUDED.draft_composition \
                 RETURNING id",
                params![
                    account.id(),
                    uid,
                    folder,
                    mid.clone(),
                    clean(f.subject.as_deref().filter(|s| !s.is_empty()).unwrap_or("(no subject)")),
                    clean(f.from_name.as_deref().unwrap_or("")),
                    clean(f.from_email.as_deref().unwrap_or("")),
                    arr(&f.to),
                    arr(&f.cc),
                    f.in_reply_to.clone().filter(|s| !s.is_empty()),
                    safe_date(f.date),
                    clean(&f.snippet),
                    json!(["\\Draft", "\\Seen"]).to_string(),
                    f.body_html.as_deref().map(clean),
                    f.body_text.as_deref().map(clean),
                    Some(mid).filter(|m| !m.is_empty()),
                    f.uid_validity.filter(|v| *v > 0),
                    arr(&f.bcc),
                    f.alias_id.clone(),
                    f.in_reply_to.clone(),
                    f.references.clone(),
                    f.composition.as_ref().map(Value::to_string)
                ],
            )
            .await?;
        self.persist_record_copy(account, folder, uid, f).await
    }

    /// `findSentMessageByMessageId`.
    pub async fn find_sent_message_by_message_id(
        self: &Arc<Self>,
        account: &Account,
        folder: &str,
        message_id: &str,
    ) -> EResult<SentLookup> {
        let mid: String = message_id.chars().filter(|c| *c != '<' && *c != '>').collect::<String>().trim().to_owned();
        if mid.is_empty() || folder.is_empty() {
            return Ok(SentLookup::Missing);
        }
        pooled!(self, account, |c| {
            c.select(folder).await?;
            let q = format!("HEADER Message-ID \"{}\"", mid.replace('\\', "\\\\").replace('"', "\\\""));
            let uids = c.search_uids(&q).await?;
            Ok(match uids.as_slice() {
                [] => SentLookup::Missing,
                [one] => SentLookup::Found(*one),
                many => {
                    tracing::warn!("Ambiguous Sent Message-ID match: {} UIDs for {mid}", many.len());
                    SentLookup::Ambiguous
                }
            })
        })
    }

    pub async fn find_uid_by_message_id(
        self: &Arc<Self>,
        account: &Account,
        folder: &str,
        message_id: &str,
    ) -> EResult<Option<u32>> {
        Ok(match self.find_sent_message_by_message_id(account, folder, message_id).await? {
            SentLookup::Found(u) => Some(u),
            _ => None,
        })
    }

    // ── durable flag push ───────────────────────────────────────────────────────────────

    /// Queue a failed read/star push for the reconciler.
    pub fn enqueue_flag_push(&self, account_id: &str, message_id: &str, flag: &str, value: bool) {
        if account_id.is_empty() || message_id.is_empty() {
            return;
        }
        let mut st = self.st.lock();
        let ops = st.pending_flag_push.entry(account_id.to_owned()).or_default();
        let key = format!("{message_id}:{flag}");
        let attempts = ops.get(&key).map_or(0, |o| o.attempts);
        ops.insert(
            key,
            FlagOp {
                message_id: message_id.to_owned(),
                flag: flag.to_owned(),
                value,
                attempts,
                resolved: Arc::new(AtomicBool::new(false)),
            },
        );
    }

    /// A later push of the same message+flag succeeded: drop the queued op.
    pub fn resolve_flag_push(&self, account_id: &str, message_id: &str, flag: &str) {
        let mut st = self.st.lock();
        let Some(ops) = st.pending_flag_push.get_mut(account_id) else { return };
        if let Some(op) = ops.remove(&format!("{message_id}:{flag}")) {
            op.resolved.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if ops.is_empty() {
            st.pending_flag_push.remove(account_id);
        }
    }
}

/// `ensureMailbox`: create via the namespace delimiter; already-exists → `created: false`.
pub async fn ensure_mailbox(c: &mut Client, path: &str, resolve_path: bool) -> EResult<CreateResult> {
    if path.contains('/') && c.namespace().is_some_and(|n| n.delimiter.as_deref().is_none_or(str::is_empty)) {
        return Err(EngineError("server does not support folder hierarchy".into()));
    }
    let segments: Vec<&str> = path.split('/').collect();
    match c.create(&segments).await {
        Ok(r) if r.created => Ok(r),
        Ok(r) => {
            let path = if resolve_path { resolve_casing(c, &r.path).await } else { r.path };
            Ok(CreateResult { path, created: false })
        }
        Err(e) => {
            let code = e.code().unwrap_or("").to_lowercase();
            let text = e.response_text().unwrap_or(&e.to_string()).to_lowercase();
            if code == "alreadyexists" || text.contains("alreadyexists") || text.contains("already exists") {
                let p = if resolve_path { resolve_casing(c, path).await } else { path.to_owned() };
                Ok(CreateResult { path: p, created: false })
            } else {
                Err(e.into())
            }
        }
    }
}

async fn resolve_casing(c: &mut Client, known: &str) -> String {
    let wanted = known.to_lowercase();
    match c.list().await {
        Ok(boxes) => {
            boxes.into_iter().find(|b| b.path.to_lowercase() == wanted).map_or_else(|| known.to_owned(), |b| b.path)
        }
        Err(_) => known.to_owned(),
    }
}

/// Bulk-move SQL helpers shared with the routes (`RELOCATE_INSERT_COLS`).
pub fn relocate_insert_columns() -> String {
    relocate_insert_cols()
}
