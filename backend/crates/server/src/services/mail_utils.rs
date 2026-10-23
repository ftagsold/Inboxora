//! Folder-role resolution, cached folder counts and sibling flag fan-out (port of
//! `utils/mailUtils.ts` and `utils/relocateColumns.ts`).

use std::collections::HashSet;

use inboxora_core::Result;
use inboxora_core::params;
use inboxora_core::pg::{Db, Q};
use serde_json::Value;

use crate::services::diagnostics_ring::record_sync_signal;

/// Columns a relocated/copied sibling inherits from its source row.
pub const RELOCATE_COPY_COLS: [&str; 50] = [
    "message_id",
    "subject",
    "from_name",
    "from_email",
    "to_addresses",
    "cc_addresses",
    "reply_to",
    "in_reply_to",
    "date",
    "snippet",
    "is_read",
    "is_starred",
    "has_attachments",
    "flags",
    "body_html",
    "body_text",
    "attachments",
    "thread_references",
    "thread_id",
    "is_bulk",
    "read_changed_at",
    "star_changed_at",
    "spam_score_sa",
    "spam_score_ml",
    "spam_verdict",
    "spam_analyzed_at",
    "spam_details",
    "spam_user_override",
    "category",
    "list_unsubscribe",
    "list_unsubscribe_post",
    "unsubscribed_at",
    "delivery_addresses",
    "plugin_annotations",
    "sender_name",
    "sender_email",
    "logical_message_id",
    "conversation_id",
    "conversation_user_id",
    "canonical_message_id",
    "provider_message_id",
    "provider_thread_id",
    "provider_namespace",
    "threading_reason",
    "threading_confidence",
    "threading_algorithm_version",
    "conversation_raw_headers",
    "conversation_thread_index",
    "conversation_thread_topic",
    "automated_series_mode",
];

pub fn relocate_insert_cols() -> String {
    ["account_id", "uid", "folder"].iter().copied().chain(RELOCATE_COPY_COLS).collect::<Vec<_>>().join(", ")
}

pub fn relocate_select_cols() -> String {
    ["d.account_id".to_owned(), "u.new_uid".to_owned(), "$4".to_owned()]
        .into_iter()
        .chain(RELOCATE_COPY_COLS.iter().map(|c| format!("d.{c}")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A mapping is used only when the folder exists and is selectable.
pub async fn mapped_folder_usable(db: &impl Q, account_id: &str, path: Option<&str>) -> Result<Option<String>> {
    let Some(path) = path.filter(|p| !p.is_empty()) else { return Ok(None) };
    let row = db
        .opt(
            "SELECT 1 FROM folders WHERE account_id = $1 AND path = $2 AND no_select = false LIMIT 1",
            params![account_id, path],
        )
        .await?;
    Ok(row.map(|_| path.to_owned()))
}

fn mapping<'a>(mappings: Option<&'a Value>, role: &str) -> Option<&'a str> {
    mappings.and_then(|m| m.get(role)).and_then(Value::as_str)
}

async fn first_path(db: &impl Q, sql: &str, account_id: &str) -> Result<Option<String>> {
    Ok(db.opt(sql, params![account_id]).await?.map(|r| r.get::<_, String>("path")))
}

async fn all_paths(db: &impl Q, sql: &str, account_id: &str) -> Result<HashSet<String>> {
    Ok(db.rows(sql, params![account_id]).await?.iter().map(|r| r.get::<_, String>("path")).collect())
}

const TRASH_WHERE: &str = "(special_use = '\\Trash' OR lower(name) LIKE '%trash%' OR lower(name) LIKE '%deleted%')";
const SPAM_WHERE: &str = "(special_use = '\\Junk' OR lower(name) ~ '(spam|junk|bulk|indesiderata|spamverdacht|courrier ind|posta indesiderata)')";

pub async fn resolve_trash_folder(db: &impl Q, account_id: &str, mappings: Option<&Value>) -> Result<Option<String>> {
    if let Some(m) = mapped_folder_usable(db, account_id, mapping(mappings, "trash")).await? {
        return Ok(Some(m));
    }
    first_path(
        db,
        &format!(
            "SELECT path FROM folders WHERE account_id = $1 AND {TRASH_WHERE} ORDER BY (CASE WHEN special_use = '\\Trash' THEN 0 ELSE 1 END) LIMIT 1"
        ),
        account_id,
    )
    .await
}

pub async fn resolve_all_trash_paths(
    db: &impl Q,
    account_id: &str,
    mappings: Option<&Value>,
) -> Result<HashSet<String>> {
    if let Some(m) = mapped_folder_usable(db, account_id, mapping(mappings, "trash")).await? {
        return Ok([m].into());
    }
    all_paths(db, &format!("SELECT path FROM folders WHERE account_id = $1 AND {TRASH_WHERE}"), account_id).await
}

pub async fn resolve_all_drafts_paths(
    db: &impl Q,
    account_id: &str,
    mappings: Option<&Value>,
) -> Result<HashSet<String>> {
    if let Some(m) = mapped_folder_usable(db, account_id, mapping(mappings, "drafts")).await? {
        return Ok([m].into());
    }
    all_paths(
        db,
        "SELECT path FROM folders WHERE account_id = $1 AND (special_use = '\\Drafts' OR lower(name) LIKE '%draft%')",
        account_id,
    )
    .await
}

pub async fn resolve_archive_folder(db: &impl Q, account_id: &str, mappings: Option<&Value>) -> Result<Option<String>> {
    if let Some(m) = mapped_folder_usable(db, account_id, mapping(mappings, "archive")).await? {
        return Ok(Some(m));
    }
    first_path(
        db,
        "SELECT path FROM folders WHERE account_id = $1 AND (special_use = '\\Archive' OR lower(name) LIKE '%archive%' OR special_use = '\\All') \
         ORDER BY (CASE WHEN special_use = '\\Archive' THEN 0 WHEN lower(name) LIKE '%archive%' THEN 1 ELSE 2 END) LIMIT 1",
        account_id,
    )
    .await
}

pub async fn is_all_mail_folder(db: &impl Q, account_id: &str, path: Option<&str>) -> Result<bool> {
    let Some(path) = path.filter(|p| !p.is_empty()) else { return Ok(false) };
    Ok(db
        .opt(
            "SELECT 1 FROM folders WHERE account_id = $1 AND path = $2 AND special_use = '\\All'",
            params![account_id, path],
        )
        .await?
        .is_some())
}

pub async fn resolve_spam_folder(db: &impl Q, account_id: &str, mappings: Option<&Value>) -> Result<Option<String>> {
    if let Some(m) = mapped_folder_usable(db, account_id, mapping(mappings, "spam")).await? {
        return Ok(Some(m));
    }
    first_path(
        db,
        &format!(
            "SELECT path FROM folders WHERE account_id = $1 AND {SPAM_WHERE} ORDER BY (CASE WHEN special_use = '\\Junk' THEN 0 ELSE 1 END) LIMIT 1"
        ),
        account_id,
    )
    .await
}

pub async fn resolve_all_spam_paths(
    db: &impl Q,
    account_id: &str,
    mappings: Option<&Value>,
) -> Result<HashSet<String>> {
    if let Some(m) = mapped_folder_usable(db, account_id, mapping(mappings, "spam")).await? {
        return Ok([m].into());
    }
    all_paths(db, &format!("SELECT path FROM folders WHERE account_id = $1 AND {SPAM_WHERE}"), account_id).await
}

pub async fn resolve_sent_folder(db: &impl Q, account_id: &str, mappings: Option<&Value>) -> Result<Option<String>> {
    if let Some(m) = mapped_folder_usable(db, account_id, mapping(mappings, "sent")).await? {
        return Ok(Some(m));
    }
    first_path(db, "SELECT path FROM folders WHERE account_id = $1 AND special_use = '\\Sent' LIMIT 1", account_id)
        .await
}

/// Adjust cached folder counts (fire-and-forget, like Node).
pub fn adjust_folder_counts(db: &Db, account_id: &str, path: &str, total: i64, unread: i64) {
    if total == 0 && unread == 0 {
        return;
    }
    let (db, account_id, path) = (db.clone(), account_id.to_owned(), path.to_owned());
    tokio::spawn(async move {
        let r = db
            .opt(
                "WITH prev AS (SELECT total_count AS ot, unread_count AS ou FROM folders WHERE account_id = $3 AND path = $4) \
                 UPDATE folders f SET total_count = GREATEST(0, f.total_count + $1), unread_count = GREATEST(0, f.unread_count + $2) \
                 FROM prev WHERE f.account_id = $3 AND f.path = $4 \
                 RETURNING (prev.ou + $2 < 0) AS unread_clamped, (prev.ot + $1 < 0) AS total_clamped",
                params![total, unread, &account_id, &path],
            )
            .await;
        match r {
            Ok(Some(row)) => {
                if row.get::<_, Option<bool>>("unread_clamped").unwrap_or(false)
                    || row.get::<_, Option<bool>>("total_clamped").unwrap_or(false)
                {
                    record_sync_signal("badge_count_clamp", Some(&account_id), None);
                }
            }
            Ok(None) => {}
            Err(e) => tracing::error!("Folder count adjust failed: {e}"),
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteStrategy {
    NoTrash,
    Expunge,
    Move(String),
}

/// `getDeleteStrategy`.
pub fn delete_strategy(folder: &str, trash: Option<&str>, all_trash: Option<&HashSet<String>>) -> DeleteStrategy {
    let Some(trash) = trash.filter(|t| !t.is_empty()) else { return DeleteStrategy::NoTrash };
    let in_trash = all_trash.map_or(folder == trash, |s| s.contains(folder));
    if in_trash { DeleteStrategy::Expunge } else { DeleteStrategy::Move(trash.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategies() {
        assert_eq!(delete_strategy("INBOX", None, None), DeleteStrategy::NoTrash);
        assert_eq!(delete_strategy("Trash", Some("Trash"), None), DeleteStrategy::Expunge);
        let all: HashSet<String> = ["Trash".into(), "Deleted Items".into()].into();
        assert_eq!(delete_strategy("Deleted Items", Some("Trash"), Some(&all)), DeleteStrategy::Expunge);
        assert_eq!(delete_strategy("INBOX", Some("Trash"), Some(&all)), DeleteStrategy::Move("Trash".into()));
        assert_eq!(RELOCATE_COPY_COLS.len(), 50);
        assert!(relocate_select_cols().starts_with("d.account_id, u.new_uid, $4, d.message_id"));
    }
}
