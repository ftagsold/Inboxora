//! Bulk delete / move and the read-only mailbox cleanup analysis.

use std::collections::HashMap;

use axum::extract::State;
use axum::response::Response;
use inboxora_core::pg::Q;
use inboxora_core::{Result, params};
use serde_json::{Map, Value, json};

use super::{is_uuid, valid_folder_name};
use crate::engine::cache::Location;
use crate::engine::connect::{Account, load_account};
use crate::engine::search::FolderScope;
use crate::http::{Auth, Body, Qs};
use crate::routes::contacts::query_string;
use crate::routes::util::{err, ok};
use crate::services::mail_utils::{
    adjust_folder_counts, relocate_insert_cols, relocate_select_cols, resolve_all_drafts_paths,
    resolve_all_trash_paths, resolve_archive_folder, resolve_trash_folder,
};
use crate::state::AppState;

/// A selected message row (`m.*` plus account columns).
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub uid: i64,
    pub folder: String,
    pub account_id: String,
    pub is_read: bool,
    pub raw: Map<String, Value>,
}

impl Row {
    pub fn from_map(m: Map<String, Value>) -> Self {
        let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        Row {
            id: s("id"),
            uid: inboxora_core::json::int(m.get("uid")).unwrap_or(0),
            folder: s("folder"),
            account_id: s("account_id"),
            is_read: m.get("is_read").and_then(Value::as_bool).unwrap_or(false),
            raw: m,
        }
    }
}

pub fn bulk_ids(body: &Body, invalid: &str) -> std::result::Result<Vec<String>, Response> {
    let Some(ids) = body.get("ids").and_then(Value::as_array).filter(|a| !a.is_empty()) else {
        return Err(err(400, "ids array required"));
    };
    if ids.len() > 500 {
        return Err(err(400, "Too many ids — maximum 500 per request"));
    }
    let out: Vec<String> = ids.iter().filter_map(|v| v.as_str().filter(|s| is_uuid(s)).map(str::to_owned)).collect();
    if out.len() != ids.len() {
        return Err(err(400, invalid));
    }
    Ok(out)
}

/// Group preserving first-seen order (JS object insertion order).
pub fn group<K: PartialEq + Clone>(rows: &[Row], key: impl Fn(&Row) -> K) -> Vec<(K, Vec<&Row>)> {
    let mut out: Vec<(K, Vec<&Row>)> = Vec::new();
    for r in rows {
        let k = key(r);
        match out.iter_mut().find(|(x, _)| *x == k) {
            Some((_, v)) => v.push(r),
            None => out.push((k, vec![r])),
        }
    }
    out
}

/// Releases move guards on drop.
pub struct Guards<'a> {
    st: &'a AppState,
    entries: Vec<(String, String, i64)>,
}

impl<'a> Guards<'a> {
    pub fn new(st: &'a AppState, rows: &[Row]) -> Self {
        let entries: Vec<(String, String, i64)> =
            rows.iter().map(|m| (m.account_id.clone(), m.folder.clone(), m.uid)).collect();
        for (a, f, u) in &entries {
            st.mail.guard_move_uid(a, f, *u);
        }
        Guards { st, entries }
    }
}

impl Drop for Guards<'_> {
    fn drop(&mut self) {
        for (a, f, u) in &self.entries {
            self.st.mail.unguard_move_uid(a, f, *u);
        }
    }
}

/// DELETE the moved source rows and re-INSERT those with known destination UIDs.
pub async fn relocate_rows(st: &AppState, moved: &[String], with_uid: &[(String, i64)], dest: &str) -> Result<()> {
    st.db
        .exec(
            &format!(
                "WITH deleted AS (DELETE FROM messages WHERE id = ANY($1::uuid[]) RETURNING *), \
                 uid_map(src_id, new_uid) AS (SELECT * FROM unnest($2::uuid[], $3::bigint[])) \
                 INSERT INTO messages ({}) SELECT {} FROM deleted d JOIN uid_map u ON d.id = u.src_id \
                 ON CONFLICT (account_id, uid, folder) DO NOTHING",
                relocate_insert_cols(),
                relocate_select_cols()
            ),
            params![
                moved.to_vec(),
                with_uid.iter().map(|w| w.0.clone()).collect::<Vec<_>>(),
                with_uid.iter().map(|w| w.1).collect::<Vec<_>>(),
                dest
            ],
        )
        .await?;
    Ok(())
}

pub async fn bulk_delete(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let ids = match bulk_ids(&body, "Invalid message id format") {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let run = async {
        let owned: Vec<Row> = st
            .db
            .json_rows(
                "SELECT m.*, a.user_id, a.folder_mappings FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.id = ANY($2::uuid[]) AND a.user_id = $1",
                params![&a.user_id, ids],
            )
            .await?
            .into_iter()
            .filter_map(|v| if let Value::Object(m) = v { Some(Row::from_map(m)) } else { None })
            .collect();
        if owned.is_empty() {
            return Ok::<_, crate::engine::EngineError>(json!({ "ok": true, "deleted": [] }));
        }
        let _guards = Guards::new(&st, &owned);
        let mut expunged: Vec<&Row> = Vec::new();
        let mut trashed: Vec<(&Row, String, Option<i64>)> = Vec::new();
        let mut accounts: HashMap<String, Account> = HashMap::new();
        for (account_id, msgs) in group(&owned, |r| r.account_id.clone()) {
            let Some(account) = load_account(&st.db, &account_id).await? else { continue };
            accounts.insert(account_id.clone(), account.clone());
            let mappings = msgs[0].raw.get("folder_mappings");
            let trash = resolve_trash_folder(&st.db, &account_id, mappings).await?;
            let all_trash = resolve_all_trash_paths(&st.db, &account_id, mappings).await?;
            let all_drafts = resolve_all_drafts_paths(&st.db, &account_id, mappings).await?;
            let Some(trash) = trash else {
                tracing::error!(
                    "bulk-delete: no Trash folder found for account {account_id} — skipping {} messages",
                    msgs.len()
                );
                continue;
            };
            let (to_expunge, to_move): (Vec<&Row>, Vec<&Row>) =
                msgs.into_iter().partition(|m| all_trash.contains(&m.folder) || all_drafts.contains(&m.folder));
            for (folder, fm) in group_refs(&to_expunge, |r| r.folder.clone()) {
                let uids: Vec<u32> = fm.iter().map(|m| m.uid as u32).collect();
                let (succeeded, failed) = st.mail.bulk_permanent_delete(&account, &uids, &folder).await;
                for u in succeeded {
                    if let Some(m) = fm.iter().find(|m| m.uid == i64::from(u)) {
                        expunged.push(m);
                    }
                }
                for u in failed {
                    tracing::error!("bulk-delete IMAP expunge uid {u} from {folder}: IMAP delete failed");
                }
            }
            for (src, fm) in group_refs(&to_move, |r| r.folder.clone()) {
                let uids: Vec<u32> = fm.iter().map(|m| m.uid as u32).collect();
                let res = st.mail.bulk_move_messages(&account, &uids, &src, &trash).await;
                for u in &res.succeeded {
                    if let Some(m) = fm.iter().find(|m| m.uid == i64::from(*u)) {
                        trashed.push((m, trash.clone(), res.uid_map.get(u).map(|n| i64::from(*n))));
                    }
                }
                for u in res.failed {
                    tracing::error!("bulk-delete IMAP move uid {u}: IMAP move failed");
                }
            }
        }
        if !expunged.is_empty() {
            st.db
                .exec(
                    "DELETE FROM messages WHERE id = ANY($1::uuid[])",
                    params![expunged.iter().map(|m| m.id.clone()).collect::<Vec<_>>()],
                )
                .await?;
        }
        if !trashed.is_empty() {
            let mut by_trash: Vec<(String, Vec<&(&Row, String, Option<i64>)>)> = Vec::new();
            for t in &trashed {
                match by_trash.iter_mut().find(|(p, _)| *p == t.1) {
                    Some((_, v)) => v.push(t),
                    None => by_trash.push((t.1.clone(), vec![t])),
                }
            }
            for (path, entries) in by_trash {
                let all: Vec<String> = entries.iter().map(|e| e.0.id.clone()).collect();
                let with: Vec<(String, i64)> =
                    entries.iter().filter_map(|e| e.2.map(|n| (e.0.id.clone(), n))).collect();
                relocate_rows(&st, &all, &with, &path).await?;
            }
            let mut resync: Vec<(String, String)> = Vec::new();
            for t in trashed.iter().filter(|t| t.2.is_none()) {
                let k = (t.0.account_id.clone(), t.1.clone());
                if !resync.contains(&k) {
                    resync.push(k);
                }
            }
            for (acct, path) in resync {
                if let Some(account) = accounts.get(&acct).cloned() {
                    let m = st.mail.clone();
                    tokio::spawn(async move { m.sync_folder_on_demand(&account, &path).await });
                }
            }
        }
        let all: Vec<String> =
            expunged.iter().map(|m| m.id.clone()).chain(trashed.iter().map(|t| t.0.id.clone())).collect();
        if !all.is_empty() {
            let mut src: Vec<((String, String), i64, i64)> = Vec::new();
            for m in expunged.iter().copied().chain(trashed.iter().map(|t| t.0)) {
                let key = (m.account_id.clone(), m.folder.clone());
                let unread = i64::from(!m.is_read);
                match src.iter_mut().find(|(k, ..)| *k == key) {
                    Some(e) => {
                        e.1 += 1;
                        e.2 += unread;
                    }
                    None => src.push((key, 1, unread)),
                }
            }
            for ((acct, path), total, unread) in src {
                adjust_folder_counts(&st.db, &acct, &path, -total, -unread);
            }
            let mut dst: Vec<((String, String), i64, i64)> = Vec::new();
            for (m, path, _) in &trashed {
                let key = (m.account_id.clone(), path.clone());
                let unread = i64::from(!m.is_read);
                match dst.iter_mut().find(|(k, ..)| *k == key) {
                    Some(e) => {
                        e.1 += 1;
                        e.2 += unread;
                    }
                    None => dst.push((key, 1, unread)),
                }
            }
            for ((acct, path), total, unread) in &dst {
                adjust_folder_counts(&st.db, acct, path, *total, *unread);
            }
            for ((acct, path), ..) in dst {
                st.mail.broadcast(
                    json!({ "type": "folder_updated", "folder": path, "accountId": acct }),
                    Some(&a.user_id),
                );
            }
        }
        Ok(json!({ "ok": true, "deleted": all }))
    };
    Ok(match run.await {
        Ok(v) => ok(v),
        Err(e) => {
            tracing::error!("bulk-delete error: {e}");
            err(500, "Failed to delete messages")
        }
    })
}

fn group_refs<'a, K: PartialEq + Clone>(rows: &[&'a Row], key: impl Fn(&Row) -> K) -> Vec<(K, Vec<&'a Row>)> {
    let mut out: Vec<(K, Vec<&Row>)> = Vec::new();
    for r in rows {
        let k = key(r);
        match out.iter_mut().find(|(x, _)| *x == k) {
            Some((_, v)) => v.push(r),
            None => out.push((k, vec![r])),
        }
    }
    out
}

pub async fn mailbox_usage(State(st): State<AppState>, a: Auth, q: Qs) -> Result<Response> {
    let Some(account_id) = query_string(&q, "accountId").filter(|v| is_uuid(v)) else {
        return Ok(err(400, "valid accountId required"));
    };
    let Some(acct) = st
        .db
        .json_opt(
            "SELECT id, folder_mappings FROM email_accounts WHERE id = $1 AND user_id = $2",
            params![&account_id, &a.user_id],
        )
        .await?
    else {
        return Ok(err(404, "Account not found"));
    };
    let archive = resolve_archive_folder(&st.db, &account_id, acct.get("folder_mappings")).await?;
    let summary = st
        .db
        .opt(
            "SELECT count(*)::int AS inbox_total, count(*) FILTER (WHERE is_bulk)::int AS bulk_total FROM messages WHERE account_id = $1 AND folder = 'INBOX'",
            params![&account_id],
        )
        .await?;
    // Sender and keyword statistics come from the server: envelopes of the bulk INBOX mail, and
    // one SEARCH per keyword.
    let bulk_rows = st
        .db
        .json_rows(
            "SELECT id, account_id, folder, uid FROM messages WHERE account_id = $1 AND folder = 'INBOX' AND is_bulk AND is_deleted = false \
             ORDER BY date DESC LIMIT 5000",
            params![&account_id],
        )
        .await?;
    let locs: Vec<Location> = bulk_rows.iter().filter_map(|r| r.as_object().and_then(Location::from_row)).collect();
    let mut by_sender: HashMap<String, (String, String, i32)> = HashMap::new();
    for (email, name) in st.mail.message_senders(&locs).await.into_values() {
        if email.is_empty() {
            continue;
        }
        let e = by_sender.entry(email.to_lowercase()).or_insert_with(|| (email.clone(), String::new(), 0));
        if email < e.0 {
            e.0 = email.clone();
        }
        if name > e.1 {
            e.1 = name;
        }
        e.2 += 1;
    }
    let mut senders: Vec<(String, String, i32)> = by_sender.into_values().collect();
    senders.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    senders.truncate(25);
    const KEYWORDS: [&str; 7] = ["% off", "deal", "sale", "newsletter", "coupon", "webinar", "last chance"];
    let inbox = FolderScope::Exact("INBOX".into());
    let accounts = [account_id.clone()];
    let counts = futures_util::future::join_all(KEYWORDS.iter().map(|k| {
        let q = inboxora_imap::quote(k);
        let criteria = format!("OR SUBJECT {q} BODY {q}");
        let (mail, inbox, accounts) = (st.mail.clone(), inbox.clone(), accounts.clone());
        async move { mail.search_ids(&accounts, &inbox, &criteria, usize::MAX).await.len() }
    }))
    .await;
    let (inbox, bulk) = summary.map_or((0, 0), |r| (r.get::<_, i32>("inbox_total"), r.get::<_, i32>("bulk_total")));
    Ok(ok(json!({
        "accountId": account_id,
        "inboxTotal": inbox,
        "bulkTotal": bulk,
        "archiveAvailable": archive.is_some(),
        "tier1Senders": senders.iter().map(|(email, name, count)| json!({
            "fromEmail": email,
            "fromName": name,
            "count": count,
        })).collect::<Vec<_>>(),
        "tier2Keywords": KEYWORDS.iter().zip(counts).map(|(k, n)| json!({ "keyword": k, "count": n })).collect::<Vec<_>>(),
    })))
}

pub async fn cleanup_preview(State(st): State<AppState>, a: Auth, q: Qs) -> Result<Response> {
    let Some(account_id) = query_string(&q, "accountId").filter(|v| is_uuid(v)) else {
        return Ok(err(400, "valid accountId required"));
    };
    let Some(from) =
        query_string(&q, "fromEmail").map(|f| inboxora_mail::text::js_trim(&f).to_owned()).filter(|f| !f.is_empty())
    else {
        return Ok(err(400, "fromEmail required"));
    };
    if st
        .db
        .opt("SELECT id FROM email_accounts WHERE id = $1 AND user_id = $2", params![&account_id, &a.user_id])
        .await?
        .is_none()
    {
        return Ok(err(404, "Account not found"));
    }
    let criteria = format!("FROM {}", inboxora_imap::quote(&from));
    let hits: Vec<String> = st
        .mail
        .search_ids(std::slice::from_ref(&account_id), &FolderScope::Exact("INBOX".into()), &criteria, usize::MAX)
        .await
        .iter()
        .map(uuid::Uuid::to_string)
        .collect();
    let candidates = st
        .db
        .json_rows(
            "SELECT id, account_id, folder, uid FROM messages WHERE id = ANY($1::uuid[]) AND is_bulk",
            params![hits],
        )
        .await?;
    // SEARCH FROM is a substring match; keep exact sender matches only.
    let locs: Vec<Location> = candidates.iter().filter_map(|r| r.as_object().and_then(Location::from_row)).collect();
    let senders = st.mail.message_senders(&locs).await;
    let ids: Vec<String> = locs
        .iter()
        .filter(|l| senders.get(&l.id).is_some_and(|(e, _)| e.eq_ignore_ascii_case(&from)))
        .map(|l| l.id.to_string())
        .collect();
    Ok(ok(json!({ "accountId": account_id, "fromEmail": from, "count": ids.len(), "ids": ids })))
}

pub async fn bulk_move(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let folder = body.get("folder").and_then(Value::as_str).unwrap_or("").to_owned();
    let valid_list = body.get("ids").and_then(Value::as_array).is_some_and(|v| !v.is_empty());
    if !valid_list || !inboxora_core::json::truthy(body.get("folder")) {
        return Ok(err(400, "ids array and folder required"));
    }
    if body.get("ids").and_then(Value::as_array).is_some_and(|v| v.len() > 500) {
        return Ok(err(400, "Too many ids — maximum 500 per request"));
    }
    if !valid_folder_name(&folder) {
        return Ok(err(400, "Invalid destination folder"));
    }
    let ids = match bulk_ids(&body, "Invalid message id format") {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let run = async {
        let owned: Vec<Row> = st
            .db
            .json_rows(
                "SELECT m.*, a.user_id FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.id = ANY($2::uuid[]) AND a.user_id = $1",
                params![&a.user_id, ids],
            )
            .await?
            .into_iter()
            .filter_map(|v| if let Value::Object(m) = v { Some(Row::from_map(m)) } else { None })
            .collect();
        if owned.is_empty() {
            return Ok::<_, crate::engine::EngineError>(json!({ "ok": true, "moved": [] }));
        }
        let _guards = Guards::new(&st, &owned);
        let mut moved: Vec<String> = Vec::new();
        let mut with_uid: Vec<(String, i64)> = Vec::new();
        let mut resync: Vec<Account> = Vec::new();
        for (account_id, msgs) in group(&owned, |r| r.account_id.clone()) {
            if st
                .db
                .opt("SELECT 1 FROM folders WHERE account_id = $1 AND path = $2", params![&account_id, &folder])
                .await?
                .is_none()
            {
                tracing::warn!("bulk-move: folder \"{folder}\" not found for account {account_id}, skipping");
                continue;
            }
            let Some(account) = load_account(&st.db, &account_id).await? else { continue };
            let mut missing = false;
            for (src, fm) in group_refs(&msgs, |r| r.folder.clone()) {
                let uids: Vec<u32> = fm.iter().map(|m| m.uid as u32).collect();
                let res = st.mail.bulk_move_messages(&account, &uids, &src, &folder).await;
                for u in &res.succeeded {
                    let Some(m) = fm.iter().find(|m| m.uid == i64::from(*u)) else { continue };
                    moved.push(m.id.clone());
                    match res.uid_map.get(u) {
                        Some(n) => with_uid.push((m.id.clone(), i64::from(*n))),
                        None => missing = true,
                    }
                }
                for u in res.failed {
                    tracing::error!("bulk-move IMAP uid {u}: IMAP move failed");
                }
            }
            if missing {
                resync.push(account);
            }
        }
        if !moved.is_empty() {
            relocate_rows(&st, &moved, &with_uid, &folder).await?;
            for account in resync {
                let (m, f) = (st.mail.clone(), folder.clone());
                tokio::spawn(async move { m.sync_folder_on_demand(&account, &f).await });
            }
            let mut src: Vec<((String, String), i64, i64)> = Vec::new();
            for m in owned.iter().filter(|m| moved.contains(&m.id)) {
                let key = (m.account_id.clone(), m.folder.clone());
                let unread = i64::from(!m.is_read);
                match src.iter_mut().find(|(k, ..)| *k == key) {
                    Some(e) => {
                        e.1 += 1;
                        e.2 += unread;
                    }
                    None => src.push((key, 1, unread)),
                }
            }
            for ((acct, path), total, unread) in &src {
                adjust_folder_counts(&st.db, acct, path, -total, -unread);
                adjust_folder_counts(&st.db, acct, &folder, *total, *unread);
            }
            for ((acct, _), ..) in &src {
                st.mail.broadcast(
                    json!({ "type": "folder_updated", "folder": folder, "accountId": acct }),
                    Some(&a.user_id),
                );
            }
        }
        Ok(json!({ "ok": true, "moved": moved }))
    };
    Ok(match run.await {
        Ok(v) => ok(v),
        Err(e) => {
            tracing::error!("bulk-move error: {e}");
            err(500, "Failed to move messages")
        }
    })
}
