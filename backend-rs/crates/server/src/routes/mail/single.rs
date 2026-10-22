//! Bulk archive and single-message actions: snooze, delete, spam/ham, category, unsubscribe.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use inboxora_core::pg::Q;
use inboxora_core::{Result, params};
use serde_json::{Map, Value, json};

use super::bulk::{Guards, Row, bulk_ids, group, relocate_rows};
use super::is_uuid;
use crate::engine::connect::{Account, load_account};
use crate::http::{Auth, Body, Qs};
use crate::routes::contacts::query_string;
use crate::routes::search::resolve_account_scope;
use crate::routes::util::{err, ok};
use crate::services::mail_utils::{
    DeleteStrategy, adjust_folder_counts, delete_strategy, is_all_mail_folder, resolve_all_drafts_paths,
    resolve_all_spam_paths, resolve_all_trash_paths, resolve_archive_folder, resolve_spam_folder, resolve_trash_folder,
};
use crate::state::AppState;

pub async fn bulk_archive(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let ids = match bulk_ids(&body, "Invalid message IDs") {
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
            return Ok::<_, crate::engine::EngineError>(json!({ "ok": true, "archived": [], "noArchiveFolder": [] }));
        }
        let _guards = Guards::new(&st, &owned);
        let mut archived: Vec<(String, String, String, Option<i64>)> = Vec::new();
        let mut no_archive: Vec<String> = Vec::new();
        let mut all_mail: HashSet<String> = HashSet::new();
        let mut accounts: HashMap<String, Account> = HashMap::new();
        for (account_id, msgs) in group(&owned, |r| r.account_id.clone()) {
            let Some(archive) = resolve_archive_folder(&st.db, &account_id, msgs[0].raw.get("folder_mappings")).await?
            else {
                no_archive.push(account_id);
                continue;
            };
            if is_all_mail_folder(&st.db, &account_id, Some(&archive)).await? {
                all_mail.insert(archive.clone());
            }
            let Some(account) = load_account(&st.db, &account_id).await? else { continue };
            accounts.insert(account_id.clone(), account.clone());
            let mut by_folder: Vec<(String, Vec<&Row>)> = Vec::new();
            for m in msgs {
                match by_folder.iter_mut().find(|(f, _)| *f == m.folder) {
                    Some((_, v)) => v.push(m),
                    None => by_folder.push((m.folder.clone(), vec![m])),
                }
            }
            for (src, fm) in by_folder {
                let uids: Vec<u32> = fm.iter().map(|m| m.uid as u32).collect();
                let res = st.mail.bulk_move_messages(&account, &uids, &src, &archive).await;
                for u in &res.succeeded {
                    if let Some(m) = fm.iter().find(|m| m.uid == i64::from(*u)) {
                        archived.push((
                            m.id.clone(),
                            account_id.clone(),
                            archive.clone(),
                            res.uid_map.get(u).map(|n| i64::from(*n)),
                        ));
                    }
                }
                for u in res.failed {
                    tracing::error!("bulk-archive IMAP uid {u}: IMAP move failed");
                }
            }
        }
        let mut by_dest: Vec<(String, Vec<&(String, String, String, Option<i64>)>)> = Vec::new();
        for e in &archived {
            match by_dest.iter_mut().find(|(f, _)| *f == e.2) {
                Some((_, v)) => v.push(e),
                None => by_dest.push((e.2.clone(), vec![e])),
            }
        }
        for (dest, entries) in &by_dest {
            let all: Vec<String> = entries.iter().map(|e| e.0.clone()).collect();
            if all_mail.contains(dest) {
                st.db.exec("DELETE FROM messages WHERE id = ANY($1::uuid[])", params![all]).await?;
                continue;
            }
            let with: Vec<(String, i64)> = entries.iter().filter_map(|e| e.3.map(|n| (e.0.clone(), n))).collect();
            relocate_rows(&st, &all, &with, dest).await?;
        }
        let mut resync: Vec<(String, String)> = Vec::new();
        for e in archived.iter().filter(|e| e.3.is_none() && !all_mail.contains(&e.2)) {
            let k = (e.1.clone(), e.2.clone());
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
        if !archived.is_empty() {
            let dest_of: HashMap<&str, &str> = archived.iter().map(|e| (e.0.as_str(), e.2.as_str())).collect();
            let mut deltas: Vec<((String, String), i64, i64)> = Vec::new();
            let mut bump = |acct: &str, path: &str, t: i64, u: i64| {
                let key = (acct.to_owned(), path.to_owned());
                match deltas.iter_mut().find(|(k, ..)| *k == key) {
                    Some(e) => {
                        e.1 += t;
                        e.2 += u;
                    }
                    None => deltas.push((key, t, u)),
                }
            };
            for m in &owned {
                let Some(dest) = dest_of.get(m.id.as_str()) else { continue };
                let unread = i64::from(!m.is_read);
                bump(&m.account_id, &m.folder, -1, -unread);
                if all_mail.contains(*dest) {
                    continue;
                }
                bump(&m.account_id, dest, 1, unread);
            }
            for ((acct, path), t, u) in deltas {
                adjust_folder_counts(&st.db, &acct, &path, t, u);
            }
            let mut dests: Vec<String> = Vec::new();
            for e in &archived {
                if !all_mail.contains(&e.2) && !dests.contains(&e.2) {
                    dests.push(e.2.clone());
                }
            }
            for dest in dests {
                let mut accts: Vec<String> = Vec::new();
                for e in archived.iter().filter(|e| e.2 == dest) {
                    if let Some(m) = owned.iter().find(|m| m.id == e.0)
                        && !accts.contains(&m.account_id)
                    {
                        accts.push(m.account_id.clone());
                    }
                }
                for acct in accts {
                    st.mail.broadcast(
                        json!({ "type": "folder_updated", "folder": dest, "accountId": acct }),
                        Some(&a.user_id),
                    );
                }
            }
        }
        Ok(
            json!({ "ok": true, "archived": archived.iter().map(|e| e.0.clone()).collect::<Vec<_>>(), "noArchiveFolder": no_archive }),
        )
    };
    Ok(match run.await {
        Ok(v) => ok(v),
        Err(e) => {
            tracing::error!("bulk-archive error: {e}");
            err(500, "Failed to archive messages")
        }
    })
}

#[derive(Debug, Clone)]
struct SnoozeRow {
    id: String,
    uid: i64,
    account_id: String,
    folder: String,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    thread_references: Option<String>,
    is_read: bool,
}

fn snooze_row(m: &Map<String, Value>) -> SnoozeRow {
    let s = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_owned);
    SnoozeRow {
        id: s("id").unwrap_or_default(),
        uid: m.get("uid").and_then(Value::as_i64).unwrap_or(0),
        account_id: s("account_id").unwrap_or_default(),
        folder: s("folder").unwrap_or_default(),
        message_id: s("message_id"),
        in_reply_to: s("in_reply_to"),
        thread_references: s("thread_references"),
        is_read: m.get("is_read").and_then(Value::as_bool).unwrap_or(false),
    }
}

/// `gatherSnoozeConversation`: the reply-chain component of `msg` in its folder.
async fn gather_snooze_conversation(st: &AppState, msg: &SnoozeRow, thread_id: Option<&str>) -> Result<Vec<SnoozeRow>> {
    let Some(thread) = thread_id.filter(|t| !t.is_empty()) else { return Ok(vec![msg.clone()]) };
    let mut pool: Vec<SnoozeRow> = st
        .db
        .json_rows(
            "SELECT id, uid, account_id, folder, message_id, in_reply_to, thread_references, is_read FROM messages \
             WHERE account_id = $1 AND thread_id = $2 AND message_id IS NOT NULL",
            params![&msg.account_id, thread],
        )
        .await?
        .iter()
        .filter_map(|v| v.as_object().map(snooze_row))
        .collect();
    if !pool.iter().any(|r| r.message_id == msg.message_id) {
        pool.push(msg.clone());
    }
    static IDS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]+>").expect("re"));
    let refs_of = |r: &SnoozeRow| -> Vec<String> {
        let mut ids: Vec<String> =
            IDS.find_iter(r.thread_references.as_deref().unwrap_or("")).map(|m| m.as_str().to_owned()).collect();
        if let Some(i) = r.in_reply_to.clone().filter(|i| !i.is_empty()) {
            ids.push(i);
        }
        ids
    };
    let mut adj: HashMap<Option<String>, HashSet<Option<String>>> = HashMap::new();
    for r in &pool {
        adj.entry(r.message_id.clone()).or_default();
    }
    for r in &pool {
        for reference in refs_of(r) {
            let key = Some(reference);
            if adj.contains_key(&key) {
                adj.entry(r.message_id.clone()).or_default().insert(key.clone());
                adj.entry(key).or_default().insert(r.message_id.clone());
            }
        }
    }
    let mut seen: HashSet<Option<String>> = [msg.message_id.clone()].into();
    let mut queue: VecDeque<Option<String>> = [msg.message_id.clone()].into();
    while let Some(cur) = queue.pop_front() {
        for nb in adj.get(&cur).cloned().unwrap_or_default() {
            if seen.insert(nb.clone()) {
                queue.push_back(nb);
            }
        }
    }
    let seen_ids: Vec<String> = seen.iter().flatten().cloned().collect();
    let already: HashSet<String> = st
        .db
        .rows(
            "SELECT message_id_header FROM snoozed_messages WHERE account_id = $1 AND message_id_header = ANY($2)",
            params![&msg.account_id, seen_ids],
        )
        .await?
        .iter()
        .filter_map(|r| r.get::<_, Option<String>>(0))
        .collect();
    let mut picked: Vec<SnoozeRow> = Vec::new();
    for r in &pool {
        if seen.contains(&r.message_id)
            && r.folder == msg.folder
            && !r.message_id.as_ref().is_some_and(|m| already.contains(m))
            && !picked.iter().any(|p| p.message_id == r.message_id)
        {
            picked.push(r.clone());
        }
    }
    let me = picked.iter().find(|p| p.message_id == msg.message_id).cloned().unwrap_or_else(|| msg.clone());
    let mut out = vec![me];
    out.extend(picked.into_iter().filter(|p| p.message_id != msg.message_id));
    Ok(out)
}

fn js_date(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    match v {
        Value::Number(n) => n.as_f64().and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64)),
        Value::String(s) => crate::routes::search::js_date_parse(s),
        _ => None,
    }
}

pub async fn snooze(State(st): State<AppState>, a: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let until_raw = body.get("until").cloned().unwrap_or(Value::Null);
    if !inboxora_core::json::truthy(Some(&until_raw)) {
        return Ok(err(400, "until is required"));
    }
    let Some(until) = js_date(&until_raw) else { return Ok(err(400, "until must be a valid ISO date")) };
    let now = chrono::Utc::now();
    if until <= now {
        return Ok(err(400, "until must be in the future"));
    }
    if until > now + chrono::Duration::days(30) {
        return Ok(err(400, "until must be within 30 days"));
    }
    let Some(m) = st
        .db
        .json_opt("SELECT m.*, a.user_id FROM messages m JOIN email_accounts a ON a.id = m.account_id WHERE m.id = $1 AND a.user_id = $2", params![
            &id,
            &a.user_id
        ])
        .await?
    else {
        return Ok(err(404, "Message not found"));
    };
    let msg = snooze_row(&m);
    let Some(mid) = msg.message_id.clone().filter(|m| !m.is_empty()) else {
        return Ok(err(400, "Message has no Message-ID header — cannot snooze"));
    };
    const SNOOZED: &str = "Snoozed";
    if msg.folder == SNOOZED {
        return Ok(err(400, "Message is already in Snoozed folder"));
    }
    if st
        .db
        .opt(
            "SELECT id FROM snoozed_messages WHERE account_id = $1 AND message_id_header = $2",
            params![&msg.account_id, &mid],
        )
        .await?
        .is_some()
    {
        return Ok(err(400, "Message is already snoozed"));
    }
    let Some(account) = load_account(&st.db, &msg.account_id).await? else { return Ok(err(404, "Message not found")) };
    let convo = gather_snooze_conversation(&st, &msg, m.get("thread_id").and_then(Value::as_str)).await?;
    if let Err(e) = st.mail.ensure_folder(&account, SNOOZED, false).await {
        tracing::error!("Snooze ensureFolder failed for message {id}: {e}");
        return Ok(err(500, "Failed to move message to Snoozed folder"));
    }
    let until_iso = inboxora_core::json::js_iso(&until);
    for tm in convo {
        st.mail.guard_move_uid(&tm.account_id, &tm.folder, tm.uid);
        let step: Result<Option<Response>> = async {
            let snoozed_uid = match st.mail.move_message(&account, tm.uid, &tm.folder, SNOOZED).await {
                Ok(u) => u,
                Err(e) => {
                    tracing::error!("Snooze IMAP move failed for message {}: {e}", tm.id);
                    if tm.id == msg.id {
                        return Ok(Some(err(500, "Failed to move message to Snoozed folder")));
                    }
                    return Ok(None);
                }
            };
            match snoozed_uid {
                Some(n) => {
                    st.db.exec("UPDATE messages SET folder = $1, uid = $2 WHERE id = $3", params![SNOOZED, n, &tm.id]).await?;
                }
                None => {
                    st.mail.guard_move_uid(&tm.account_id, SNOOZED, tm.uid);
                    st.db.exec("UPDATE messages SET folder = $1 WHERE id = $2", params![SNOOZED, &tm.id]).await?;
                    let (e, acct, uid) = (st.mail.clone(), tm.account_id.clone(), tm.uid);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        e.unguard_move_uid(&acct, SNOOZED, uid);
                    });
                }
            }
            st.db
                .exec(
                    "INSERT INTO snoozed_messages (user_id, account_id, message_id_header, original_folder, snooze_until, snoozed_folder) VALUES ($1, $2, $3, $4, $5, $6)",
                    params![&a.user_id, &tm.account_id, &tm.message_id, &tm.folder, &until_iso, SNOOZED],
                )
                .await?;
            adjust_folder_counts(&st.db, &tm.account_id, &tm.folder, -1, if tm.is_read { 0 } else { -1 });
            adjust_folder_counts(&st.db, &tm.account_id, SNOOZED, 1, if tm.is_read { 0 } else { 1 });
            Ok(None)
        }
        .await;
        st.mail.unguard_move_uid(&tm.account_id, &tm.folder, tm.uid);
        if let Some(r) = step? {
            return Ok(r);
        }
    }
    Ok(ok(json!({ "ok": true })))
}

pub async fn delete_message(State(st): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let Some(m) = st
        .db
        .json_opt("SELECT m.*, a.user_id FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.id = $1 AND a.user_id = $2", params![
            &id,
            &a.user_id
        ])
        .await?
    else {
        return Ok(err(404, "Message not found"));
    };
    let msg = Row::from_map(m);
    let Some(account) = load_account(&st.db, &msg.account_id).await? else { return Ok(err(404, "Message not found")) };
    let mappings = account.0.get("folder_mappings");
    let unread = i64::from(!msg.is_read);
    let folder_updated = |st: &AppState| {
        st.mail.broadcast(
            json!({ "type": "folder_updated", "folder": msg.folder, "accountId": msg.account_id }),
            Some(&a.user_id),
        );
    };
    if resolve_all_drafts_paths(&st.db, &msg.account_id, mappings).await?.contains(&msg.folder) {
        if let Err(e) = st.mail.permanent_delete_message(&account, msg.uid, &msg.folder, None).await {
            tracing::error!("IMAP permanent delete (draft) failed: {e}");
            return Ok(err(500, "Failed to delete draft"));
        }
        st.db.exec("DELETE FROM messages WHERE id = $1", params![&id]).await?;
        adjust_folder_counts(&st.db, &msg.account_id, &msg.folder, -1, -unread);
        folder_updated(&st);
        return Ok(ok(json!({ "ok": true })));
    }
    let trash = resolve_trash_folder(&st.db, &msg.account_id, mappings).await?;
    let all_trash = resolve_all_trash_paths(&st.db, &msg.account_id, mappings).await?;
    match delete_strategy(&msg.folder, trash.as_deref(), Some(&all_trash)) {
        DeleteStrategy::NoTrash => return Ok(err(422, "No Trash folder configured for this account")),
        DeleteStrategy::Move(trash) => {
            st.mail.guard_move_uid(&msg.account_id, &msg.folder, msg.uid);
            let r: Result<Option<Response>> = async {
                let new_uid = match st.mail.move_message(&account, msg.uid, &msg.folder, &trash).await {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::error!("IMAP move to trash failed: {e}");
                        return Ok(Some(err(500, "Failed to delete message")));
                    }
                };
                match new_uid {
                    Some(n) => {
                        st.db
                            .exec(
                                "DELETE FROM messages WHERE account_id = $1 AND uid = $2 AND folder = $3 AND id != $4",
                                params![&msg.account_id, n, &trash, &id],
                            )
                            .await?;
                        st.db
                            .exec("UPDATE messages SET folder = $1, uid = $2 WHERE id = $3", params![&trash, n, &id])
                            .await?;
                    }
                    None => {
                        st.mail.guard_move_uid(&msg.account_id, &trash, msg.uid);
                        st.db.exec("UPDATE messages SET folder = $1 WHERE id = $2", params![&trash, &id]).await?;
                        let (e, acct, t, uid) = (st.mail.clone(), msg.account_id.clone(), trash.clone(), msg.uid);
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            e.unguard_move_uid(&acct, &t, uid);
                        });
                    }
                }
                Ok(None)
            }
            .await;
            st.mail.unguard_move_uid(&msg.account_id, &msg.folder, msg.uid);
            if let Some(resp) = r? {
                return Ok(resp);
            }
            adjust_folder_counts(&st.db, &msg.account_id, &msg.folder, -1, -unread);
            adjust_folder_counts(&st.db, &msg.account_id, &trash, 1, unread);
        }
        DeleteStrategy::Expunge => {
            if let Err(e) = st.mail.permanent_delete_message(&account, msg.uid, &msg.folder, None).await {
                tracing::error!("IMAP permanent delete failed: {e}");
                return Ok(err(500, "Failed to delete message"));
            }
            st.db.exec("DELETE FROM messages WHERE id = $1", params![&id]).await?;
            adjust_folder_counts(&st.db, &msg.account_id, &msg.folder, -1, -unread);
        }
    }
    folder_updated(&st);
    Ok(ok(json!({ "ok": true })))
}

/// `moveForSpamLabel`.
async fn move_for_spam_label(st: &AppState, id: &str, user: &str, dest: &str, label: &str) -> Result<Response> {
    let Some(m) = st
        .db
        .json_opt(
            "SELECT m.*, a.user_id, a.folder_mappings FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.id = $1 AND a.user_id = $2",
            params![id, user],
        )
        .await?
    else {
        return Ok(err(404, "Message not found"));
    };
    let msg = Row::from_map(m.clone());
    let mid = m.get("message_id").cloned().unwrap_or(Value::Null);
    if msg.folder == dest {
        st.db
            .exec(
                "INSERT INTO spam_training_log (user_id, account_id, message_id_header, message_uid, folder, label) VALUES ($1, $2, $3, $4, $5, $6)",
                params![user, &msg.account_id, &mid, msg.uid, &msg.folder, label],
            )
            .await?;
        st.db
            .exec("UPDATE messages SET spam_user_override = $1, spam_verdict = $1, spam_analyzed_at = NOW() WHERE id = $2", params![label, id])
            .await?;
        return Ok(ok(json!({ "ok": true, "alreadyInFolder": true, "folder": dest })));
    }
    let Some(account) = load_account(&st.db, &msg.account_id).await? else { return Ok(err(404, "Message not found")) };
    st.mail.guard_move_uid(account.id(), &msg.folder, msg.uid);
    let r: Result<std::result::Result<Option<i64>, Response>> = async {
        let new_uid = match st.mail.move_message(&account, msg.uid, &msg.folder, dest).await {
            Ok(u) => u,
            Err(e) => {
                tracing::error!("IMAP move for /{label} failed: {e}");
                return Ok(Err(err(502, &format!("IMAP move failed: {e}"))));
            }
        };
        match new_uid {
            Some(n) => {
                st.db
                    .exec("DELETE FROM messages WHERE account_id = $1 AND uid = $2 AND folder = $3 AND id != $4", params![account.id(), n, dest, id])
                    .await?;
                st.db
                    .exec(
                        "UPDATE messages SET folder = $1, uid = $2, spam_user_override = $3, spam_verdict = $3, spam_analyzed_at = NOW() WHERE id = $4",
                        params![dest, n, label, id],
                    )
                    .await?;
            }
            None => {
                st.mail.guard_move_uid(account.id(), dest, msg.uid);
                st.db
                    .exec(
                        "UPDATE messages SET folder = $1, spam_user_override = $2, spam_verdict = $2, spam_analyzed_at = NOW() WHERE id = $3",
                        params![dest, label, id],
                    )
                    .await?;
                let (e, acct, d, uid) = (st.mail.clone(), account.id().to_owned(), dest.to_owned(), msg.uid);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    e.unguard_move_uid(&acct, &d, uid);
                });
            }
        }
        Ok(Ok(new_uid))
    }
    .await;
    st.mail.unguard_move_uid(account.id(), &msg.folder, msg.uid);
    let new_uid = match r? {
        Ok(n) => n,
        Err(resp) => return Ok(resp),
    };
    let unread = i64::from(!msg.is_read);
    adjust_folder_counts(&st.db, account.id(), &msg.folder, -1, -unread);
    adjust_folder_counts(&st.db, account.id(), dest, 1, unread);
    st.db
        .exec(
            "INSERT INTO spam_training_log (user_id, account_id, message_id_header, message_uid, folder, label, source) VALUES ($1, $2, $3, $4, $5, $6, 'manual')",
            params![user, account.id(), &mid, new_uid.unwrap_or(msg.uid), dest, label],
        )
        .await?;
    if label == "spam" && account.mapping("spam").is_none() {
        let r = st
            .db
            .exec(
                "UPDATE email_accounts SET folder_mappings = folder_mappings || jsonb_build_object('spam', $1::text) WHERE id = $2 AND NOT (folder_mappings ? 'spam')",
                params![dest, account.id()],
            )
            .await;
        if let Err(e) = r {
            tracing::warn!("Failed to auto-persist folder_mappings.spam: {e}");
        }
    }
    st.mail.broadcast(json!({ "type": "folder_updated", "folder": dest, "accountId": account.id() }), Some(user));
    Ok(ok(json!({ "ok": true, "folder": dest, "newUid": new_uid })))
}

pub async fn spam(State(st): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let Some(row) = st
        .db
        .json_opt(
            "SELECT m.account_id, a.folder_mappings FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.id = $1 AND a.user_id = $2",
            params![&id, &a.user_id],
        )
        .await?
    else {
        return Ok(err(404, "Message not found"));
    };
    let acct = row.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
    let Some(dest) = resolve_spam_folder(&st.db, &acct, row.get("folder_mappings")).await? else {
        return Ok(err(422, "No spam folder configured for this account"));
    };
    move_for_spam_label(&st, &id, &a.user_id, &dest, "spam").await
}

pub async fn ham(State(st): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let Some(row) = st
        .db
        .json_opt(
            "SELECT m.account_id, m.folder, a.folder_mappings FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.id = $1 AND a.user_id = $2",
            params![&id, &a.user_id],
        )
        .await?
    else {
        return Ok(err(404, "Message not found"));
    };
    let acct = row.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
    let folder = row.get("folder").and_then(Value::as_str).unwrap_or("");
    let all = resolve_all_spam_paths(&st.db, &acct, row.get("folder_mappings")).await?;
    if !all.contains(folder) {
        return Ok(err(400, "Message is not in the spam folder"));
    }
    let inbox = row
        .get("folder_mappings")
        .and_then(|m| m.get("inbox"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("INBOX")
        .to_owned();
    move_for_spam_label(&st, &id, &a.user_id, &inbox, "ham").await
}

const CATEGORIES: [&str; 5] = ["primary", "newsletter", "promotion", "automated", "social"];

pub async fn category_counts(State(st): State<AppState>, a: Auth, q: Qs) -> Result<Response> {
    let account_id = query_string(&q, "accountId");
    if let Some(id) = account_id.as_deref().filter(|s| !s.is_empty())
        && !is_uuid(id)
    {
        return Ok(err(400, "Invalid account id"));
    }
    let accounts: Vec<(String, Option<bool>)> = st
        .db
        .rows(
            "SELECT id, include_in_unified_inbox FROM email_accounts WHERE user_id = $1 AND enabled = true",
            params![&a.user_id],
        )
        .await?
        .iter()
        .map(|r| (r.get::<_, uuid::Uuid>(0).to_string(), r.get::<_, Option<bool>>(1)))
        .collect();
    let (ids, _) = resolve_account_scope(&accounts, account_id.as_deref());
    let mut counts = Map::new();
    if !ids.is_empty() {
        for r in st
            .db
            .rows(
                "SELECT COALESCE(m.category, 'primary') AS category, COUNT(*) FILTER (WHERE m.is_read = false)::int AS unread_count FROM messages m \
                 WHERE m.account_id = ANY($1) AND m.folder = 'INBOX' AND m.is_deleted = false GROUP BY COALESCE(m.category, 'primary')",
                params![ids],
            )
            .await?
        {
            counts.insert(r.get::<_, String>("category"), json!(r.get::<_, i32>("unread_count")));
        }
    } else {
        return Ok(ok(json!({ "counts": {} })));
    }
    let mut res = ok(json!({ "counts": counts }));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

pub async fn set_category(State(st): State<AppState>, a: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let Some(category) =
        body.get("category").and_then(Value::as_str).filter(|c| CATEGORIES.contains(c)).map(str::to_owned)
    else {
        return Ok(err(400, "Invalid category"));
    };
    let stored = (category != "primary").then_some(category.clone());
    let r = st
        .db
        .opt(
            "UPDATE messages SET category = $1 FROM email_accounts a WHERE messages.id = $2 AND messages.account_id = a.id AND a.user_id = $3 RETURNING messages.id",
            params![stored, &id, &a.user_id],
        )
        .await?;
    Ok(match r {
        Some(_) => ok(json!({ "ok": true, "category": category })),
        None => err(404, "Message not found"),
    })
}

pub async fn unsubscribe(State(st): State<AppState>, a: Auth, Path(id): Path<String>) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(400, "Invalid message id"));
    }
    let Some(row) = st
        .db
        .opt(
            "SELECT m.list_unsubscribe, m.list_unsubscribe_post FROM messages m JOIN email_accounts a ON m.account_id = a.id \
             WHERE m.id = $1 AND a.user_id = $2 AND m.is_deleted = false",
            params![&id, &a.user_id],
        )
        .await?
    else {
        return Ok(err(404, "Message not found"));
    };
    let Some(raw) = row.get::<_, Option<String>>(0).filter(|s| !s.is_empty()) else {
        return Ok(err(400, "No unsubscribe header"));
    };
    let unsub = inboxora_mail::headers::decode_mime_words(&raw);
    let post = row
        .get::<_, Option<String>>(1)
        .filter(|s| !s.is_empty())
        .map(|p| inboxora_mail::headers::decode_mime_words(&p));
    static REFS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"<([^>]+)>").expect("re"));
    let refs: Vec<String> =
        REFS.captures_iter(&unsub).map(|c| inboxora_mail::text::js_trim(&c[1]).to_owned()).collect();
    let https = refs.iter().find(|r| r.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))).cloned();
    let mailto = refs.iter().find(|r| r.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("mailto:"))).cloned();
    let one_click = post.as_deref().is_some_and(|p| p.to_lowercase().contains("list-unsubscribe=one-click"));
    if one_click && let Some(url) = &https {
        let Ok(parsed) = url::Url::parse(url) else { return Ok(err(400, "Invalid unsubscribe URL")) };
        let host = parsed.host_str().unwrap_or("").to_owned();
        if inboxora_net::validate_host(&host, false).await.is_some() {
            return Ok(err(400, "Unsubscribe URL not allowed"));
        }
        let req = inboxora_net::safe_request(inboxora_net::reqwest::Method::POST, url, inboxora_net::Guard::default());
        match req {
            Ok(r) => {
                let res = r
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .header("User-Agent", "Inboxora/4.0.0")
                    .body("List-Unsubscribe=One-Click")
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await;
                match res {
                    Ok(resp) if resp.status().is_success() => {
                        st.db.exec("UPDATE messages SET unsubscribed_at = NOW() WHERE id = $1", params![&id]).await?;
                        return Ok(ok(json!({ "ok": true, "type": "one-click" })));
                    }
                    Ok(resp) => tracing::warn!("One-click unsubscribe returned {} for {url}", resp.status().as_u16()),
                    Err(e) => tracing::warn!("One-click unsubscribe failed: {e}"),
                }
            }
            Err(e) => tracing::warn!("One-click unsubscribe failed: {e}"),
        }
    }
    st.db.exec("UPDATE messages SET unsubscribed_at = NOW() WHERE id = $1", params![&id]).await?;
    Ok(ok(
        json!({ "ok": true, "type": if https.is_some() { "url" } else { "mailto" }, "url": https, "mailto": mailto }),
    ))
}
