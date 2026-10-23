//! Sync triggers, folder management and bulk read state.

use std::collections::HashSet;
use std::sync::LazyLock;

use axum::extract::State;
use axum::response::Response;
use inboxora_core::pg::Q;
use inboxora_core::{Result, params};
use serde_json::{Value, json};

use super::{account_for_user, is_uuid, valid_folder_name};
use crate::http::{Auth, Body};
use crate::routes::util::{err, json as json_status, ok};
use crate::services::mail_utils::adjust_folder_counts;
use crate::state::AppState;

fn s<'a>(b: &'a Body, k: &str) -> Option<&'a str> {
    b.get(k).and_then(Value::as_str)
}

fn truthy(b: &Body, k: &str) -> bool {
    inboxora_core::json::truthy(b.get(k))
}

async fn optional_owned_account(
    st: &AppState,
    a: &Auth,
    body: &Body,
) -> Result<std::result::Result<Option<String>, Response>> {
    if !truthy(body, "accountId") {
        return Ok(Ok(None));
    }
    let id = body.get("accountId").map(inboxora_mail::text::js_string).unwrap_or_default();
    if !is_uuid(&id) {
        return Ok(Err(err(400, "Invalid account id")));
    }
    if st
        .db
        .opt("SELECT id FROM email_accounts WHERE id = $1 AND user_id = $2", params![&id, &a.user_id])
        .await?
        .is_none()
    {
        return Ok(Err(err(404, "Account not found")));
    }
    Ok(Ok(Some(id)))
}

pub async fn sync(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let account = match optional_owned_account(&st, &a, &body).await? {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let (m, user) = (st.mail.clone(), a.user_id.clone());
    tokio::spawn(async move { m.sync_now(&user, account.as_deref()).await });
    Ok(ok(json!({ "ok": true })))
}

pub async fn sync_folders(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let account = match optional_owned_account(&st, &a, &body).await? {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let (m, user) = (st.mail.clone(), a.user_id.clone());
    tokio::spawn(async move { m.sync_folders_now(&user, account.as_deref()).await });
    Ok(ok(json!({ "ok": true })))
}

pub async fn sync_folder(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    if !truthy(&body, "accountId") || !truthy(&body, "folder") {
        return Ok(err(400, "accountId and folder required"));
    }
    let account_id = body.get("accountId").map(inboxora_mail::text::js_string).unwrap_or_default();
    if !is_uuid(&account_id) {
        return Ok(err(400, "Invalid account id"));
    }
    let folder = s(&body, "folder").unwrap_or_default().to_owned();
    if !valid_folder_name(&folder) {
        return Ok(err(400, "Invalid folder name"));
    }
    let Some(account) = account_for_user(&st, &account_id, &a.user_id).await? else {
        return Ok(err(404, "Account not found"));
    };
    let m = st.mail.clone();
    tokio::spawn(async move { m.sync_folder_on_demand(&account, &folder).await });
    Ok(ok(json!({ "ok": true })))
}

pub async fn mark_all_read(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let account_id = body.get("accountId").map(inboxora_mail::text::js_string).unwrap_or_default();
    if !truthy(&body, "accountId") || !is_uuid(&account_id) {
        return Ok(err(400, "Invalid account id"));
    }
    let folder = body.get("folder").map_or_else(|| "INBOX".to_owned(), inboxora_mail::text::js_string);
    if !valid_folder_name(&folder) {
        return Ok(err(400, "Invalid folder name"));
    }
    let Some(account) = account_for_user(&st, &account_id, &a.user_id).await? else {
        return Ok(err(404, "Account not found"));
    };
    st.db
        .exec(
            "UPDATE messages SET is_read = true, read_changed_at = NOW() WHERE account_id = $1 AND folder = $2",
            params![&account_id, &folder],
        )
        .await?;
    if let Err(e) = st
        .db
        .exec("UPDATE folders SET unread_count = 0 WHERE account_id = $1 AND path = $2", params![&account_id, &folder])
        .await
    {
        tracing::error!("Folder count update failed: {e}");
    }
    let (m, acc, f) = (st.mail.clone(), account.clone(), folder.clone());
    tokio::spawn(async move {
        if let Err(e) = m.mark_all_read_imap(&acc, &f).await {
            tracing::warn!("markAllReadImap failed: {e}");
        }
    });
    st.mail.broadcast(json!({ "type": "sync_complete", "accountId": account_id }), Some(account.user_id()));
    Ok(ok(json!({ "ok": true })))
}

pub async fn create_folder(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let name = s(&body, "name").map(|n| inboxora_mail::text::js_trim(n).to_owned()).unwrap_or_default();
    if !truthy(&body, "accountId") || name.is_empty() {
        return Ok(err(400, "accountId and name required"));
    }
    if !valid_folder_name(&name) {
        return Ok(err(400, "Invalid folder name"));
    }
    let parent = s(&body, "parentPath").filter(|p| !p.is_empty()).map(str::to_owned);
    if let Some(p) = &parent
        && !valid_folder_name(p)
    {
        return Ok(err(400, "Invalid parent path"));
    }
    let account_id = body.get("accountId").map(inboxora_mail::text::js_string).unwrap_or_default();
    let Some(account) = account_for_user(&st, &account_id, &a.user_id).await? else {
        return Ok(err(404, "Account not found"));
    };
    let mut path = name.clone();
    if let Some(p) = &parent {
        let delim = st
            .db
            .opt("SELECT delimiter FROM folders WHERE account_id = $1 LIMIT 1", params![&account_id])
            .await?
            .and_then(|r| r.get::<_, Option<String>>("delimiter"))
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "/".into());
        path = format!("{p}{delim}{name}");
    }
    let run = async {
        st.mail.create_folder(&account, &path).await?;
        st.db
            .exec(
                "INSERT INTO folders (account_id, path, name) VALUES ($1, $2, $3) ON CONFLICT (account_id, path) DO NOTHING",
                params![&account_id, &path, &name],
            )
            .await?;
        Ok::<_, crate::engine::EngineError>(())
    };
    Ok(match run.await {
        Ok(()) => ok(json!({ "ok": true, "path": path })),
        Err(e) => {
            tracing::error!("Create folder error: {e}");
            err(500, "Failed to create folder")
        }
    })
}

pub async fn delete_folder(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    if !truthy(&body, "accountId") || !truthy(&body, "path") {
        return Ok(err(400, "accountId and path required"));
    }
    let path = s(&body, "path").unwrap_or_default().to_owned();
    if !valid_folder_name(&path) {
        return Ok(err(400, "Invalid folder path"));
    }
    let account_id = body.get("accountId").map(inboxora_mail::text::js_string).unwrap_or_default();
    let Some(account) = account_for_user(&st, &account_id, &a.user_id).await? else {
        return Ok(err(404, "Account not found"));
    };
    if let Err(e) = st.mail.delete_folder(&account, &path).await {
        tracing::error!("IMAP deleteFolder failed for {path}: {e}");
        return Ok(err(500, "Failed to delete folder on server"));
    }
    st.db.exec("DELETE FROM folders WHERE account_id = $1 AND path = $2", params![&account_id, &path]).await?;
    st.db.exec("DELETE FROM messages WHERE account_id = $1 AND folder = $2", params![&account_id, &path]).await?;
    Ok(ok(json!({ "ok": true })))
}

pub async fn rename_folder(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let new_name = s(&body, "newName").map(|n| inboxora_mail::text::js_trim(n).to_owned()).unwrap_or_default();
    if !truthy(&body, "accountId") || !truthy(&body, "oldPath") || new_name.is_empty() {
        return Ok(err(400, "Missing required fields"));
    }
    if !valid_folder_name(&new_name) {
        return Ok(err(400, "Invalid folder name"));
    }
    let old = s(&body, "oldPath").unwrap_or_default().to_owned();
    if !valid_folder_name(&old) {
        return Ok(err(400, "Invalid folder path"));
    }
    let account_id = body.get("accountId").map(inboxora_mail::text::js_string).unwrap_or_default();
    let Some(account) = account_for_user(&st, &account_id, &a.user_id).await? else {
        return Ok(err(404, "Account not found"));
    };
    let delim = st
        .db
        .opt("SELECT delimiter FROM folders WHERE account_id = $1 AND path = $2", params![&account_id, &old])
        .await?
        .and_then(|r| r.get::<_, Option<String>>("delimiter"))
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/".into());
    let mut parts: Vec<&str> = old.split(delim.as_str()).collect();
    let last = parts.len() - 1;
    parts[last] = &new_name;
    let new_path = parts.join(&delim);
    let run = async {
        st.mail.rename_folder(&account, &old, &new_path).await?;
        let child = format!("{old}{delim}");
        let p = params![&account_id, &old, &child, &new_path];
        st.db
            .exec(
                "DELETE FROM folders old WHERE old.account_id = $1 AND (old.path = $2 OR substr(old.path, 1, length($3)) = $3) \
                 AND EXISTS (SELECT 1 FROM folders n WHERE n.account_id = $1 AND n.path = $4 || substr(old.path, length($2) + 1))",
                p,
            )
            .await?;
        st.db
            .exec(
                "UPDATE folders SET path = $4 || substr(path, length($2) + 1), updated_at = NOW() \
                 WHERE account_id = $1 AND (path = $2 OR substr(path, 1, length($3)) = $3)",
                p,
            )
            .await?;
        st.db
            .exec(
                "UPDATE folders SET name = $1, updated_at = NOW() WHERE account_id = $2 AND path = $3",
                params![&new_name, &account_id, &new_path],
            )
            .await?;
        st.db
            .exec(
                "DELETE FROM messages old WHERE old.account_id = $1 AND (old.folder = $2 OR substr(old.folder, 1, length($3)) = $3) \
                 AND EXISTS (SELECT 1 FROM messages n WHERE n.account_id = $1 AND n.folder = $4 || substr(old.folder, length($2) + 1) AND n.uid = old.uid)",
                p,
            )
            .await?;
        st.db
            .exec(
                "UPDATE messages SET folder = $4 || substr(folder, length($2) + 1) WHERE account_id = $1 AND (folder = $2 OR substr(folder, 1, length($3)) = $3)",
                p,
            )
            .await?;
        Ok::<_, crate::engine::EngineError>(())
    };
    Ok(match run.await {
        Ok(()) => ok(json!({ "ok": true, "newPath": new_path })),
        Err(e) => {
            tracing::error!("Rename folder error: {e}");
            err(500, "Failed to rename folder")
        }
    })
}

static EMPTY_IN_FLIGHT: LazyLock<parking_lot::Mutex<HashSet<String>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashSet::new()));

pub async fn empty_folder(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    if !truthy(&body, "accountId") || !truthy(&body, "path") {
        return Ok(err(400, "accountId and path required"));
    }
    let path = s(&body, "path").unwrap_or_default().to_owned();
    if !valid_folder_name(&path) {
        return Ok(err(400, "Invalid folder path"));
    }
    let account_id = body.get("accountId").map(inboxora_mail::text::js_string).unwrap_or_default();
    let Some(account) = account_for_user(&st, &account_id, &a.user_id).await? else {
        return Ok(err(404, "Account not found"));
    };
    let key = format!("{account_id}:{path}");
    if !EMPTY_IN_FLIGHT.lock().insert(key.clone()) {
        return Ok(err(409, "This folder is already being emptied"));
    }
    let (m, db) = (st.mail.clone(), st.db.clone());
    tokio::spawn(async move {
        let user = account.user_id().to_owned();
        let r = async {
            m.empty_folder(&account, &path).await?;
            db.exec("DELETE FROM messages WHERE account_id = $1 AND folder = $2", params![&account_id, &path]).await?;
            db.exec(
                "UPDATE folders SET total_count = 0, unread_count = 0 WHERE account_id = $1 AND path = $2",
                params![&account_id, &path],
            )
            .await?;
            Ok::<_, crate::engine::EngineError>(())
        }
        .await;
        match r {
            Ok(()) => {
                m.broadcast(
                    json!({ "type": "folder_emptied", "accountId": account_id, "folder": path, "ok": true }),
                    Some(&user),
                );
                m.broadcast(json!({ "type": "sync_complete", "accountId": account_id }), Some(&user));
            }
            Err(e) => {
                tracing::error!("Async emptyFolder failed for {path}: {e}");
                m.broadcast(
                    json!({ "type": "folder_emptied", "accountId": account_id, "folder": path, "ok": false }),
                    Some(&user),
                );
            }
        }
        EMPTY_IN_FLIGHT.lock().remove(&key);
    });
    Ok(json_status(202, json!({ "ok": true, "started": true })))
}

/// Validate a bulk `ids` array (≤ 500 UUIDs).
pub fn bulk_ids(body: &Body) -> std::result::Result<Vec<String>, Response> {
    let Some(ids) = body.get("ids").and_then(Value::as_array).filter(|a| !a.is_empty()) else {
        return Err(err(400, "ids array required"));
    };
    if ids.len() > 500 {
        return Err(err(400, "Too many ids — maximum 500 per request"));
    }
    let out: Vec<String> = ids.iter().filter_map(|v| v.as_str().filter(|s| is_uuid(s)).map(str::to_owned)).collect();
    if out.len() != ids.len() {
        return Err(err(400, "Invalid message IDs"));
    }
    Ok(out)
}

pub async fn bulk_read(State(st): State<AppState>, a: Auth, body: Body) -> Result<Response> {
    let ids = match bulk_ids(&body) {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let Some(read) = body.get("read").and_then(Value::as_bool) else {
        return Ok(err(400, "read must be a boolean"));
    };
    let run = async {
        let rows = st
            .db
            .rows(
                "SELECT m.id, m.uid, m.folder, m.is_read, m.account_id, m.message_id FROM messages m JOIN email_accounts a ON m.account_id = a.id \
                 WHERE m.id = ANY($2::uuid[]) AND a.user_id = $1",
                params![&a.user_id, ids.clone()],
            )
            .await?;
        let to_update: Vec<(String, i64, String, String)> = rows
            .iter()
            .filter(|r| r.get::<_, Option<bool>>("is_read").unwrap_or(false) != read)
            .map(|r| {
                (
                    r.get::<_, uuid::Uuid>("id").to_string(),
                    r.get::<_, i64>("uid"),
                    r.get::<_, String>("folder"),
                    r.get::<_, uuid::Uuid>("account_id").to_string(),
                )
            })
            .collect();
        if to_update.is_empty() {
            return Ok::<_, crate::engine::EngineError>(json!({ "ok": true, "updated": [] }));
        }
        let ids: Vec<String> = to_update.iter().map(|m| m.0.clone()).collect();
        st.db
            .exec(
                "UPDATE messages SET is_read = $1, read_changed_at = NOW() WHERE id = ANY($2::uuid[])",
                params![read, ids.clone()],
            )
            .await?;
        let mut deltas: Vec<((String, String), i64)> = Vec::new();
        for (_, _, folder, acct) in &to_update {
            let key = (acct.clone(), folder.clone());
            match deltas.iter_mut().find(|(k, _)| *k == key) {
                Some((_, d)) => *d += if read { -1 } else { 1 },
                None => deltas.push((key, if read { -1 } else { 1 })),
            }
        }
        for ((acct, folder), d) in deltas {
            adjust_folder_counts(&st.db, &acct, &folder, 0, d);
        }
        st.mail.broadcast(
            json!({ "type": "message_flags", "changes": to_update.iter().map(|m| json!({ "id": m.0, "is_read": read })).collect::<Vec<_>>() }),
            Some(&a.user_id),
        );
        let mut accounts: Vec<String> = Vec::new();
        for m in &to_update {
            if !accounts.contains(&m.3) {
                accounts.push(m.3.clone());
            }
        }
        for acct in accounts {
            let Some(account) = crate::engine::connect::load_account(&st.db, &acct).await? else { continue };
            let msgs: Vec<&(String, i64, String, String)> = to_update.iter().filter(|m| m.3 == acct).collect();
            for chunk in msgs.chunks(3) {
                let results = futures_util::future::join_all(
                    chunk.iter().map(|m| st.mail.set_flag(&account, m.1, &m.2, "\\Seen", read)),
                )
                .await;
                for (m, r) in chunk.iter().zip(results) {
                    match r {
                        Ok(()) => st.mail.resolve_flag_push(&acct, &m.0, "\\Seen"),
                        Err(e) => {
                            tracing::error!("bulk-read IMAP {}: {e}", m.0);
                            st.mail.enqueue_flag_push(&acct, &m.0, "\\Seen", read);
                        }
                    }
                }
            }
        }
        Ok(json!({ "ok": true, "updated": ids }))
    };
    Ok(match run.await {
        Ok(v) => ok(v),
        Err(e) => {
            tracing::error!("bulk-read error: {e}");
            err(500, "Failed to update messages")
        }
    })
}
