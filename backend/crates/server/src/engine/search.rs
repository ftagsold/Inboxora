//! Server-side search. Message text lives on the IMAP server, so matching runs there (`UID
//! SEARCH`) and the hits are mapped back to header-index rows. Servers with word-based
//! full-text indexes (Gmail, Dovecot FTS) only match whole words, so the cached display fields
//! (subject, sender, recipients, snippet) are also scanned for substrings.

use std::sync::Arc;
use std::time::Duration;

use inboxora_core::params;
use inboxora_core::pg::Q;
use uuid::Uuid;

use super::cache::{Meta, meta_key};
use super::connect::load_account;
use super::parse::Addr;
use super::{EResult, Engine, EngineError};

/// Each account's search must finish within this budget; slower accounts contribute no hits.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(25);

const TRASH: &str = "(f.special_use = '\\Trash' OR lower(f.name) LIKE '%trash%' OR lower(f.name) LIKE '%deleted%')";

/// Newest rows per search whose cached display fields are scanned for substrings.
const CACHED_SCAN_ROWS: i64 = 20_000;
/// Keys per Redis MGET while scanning.
const CACHED_SCAN_BATCH: usize = 1000;

/// Which part of a message a text condition looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextField {
    From,
    To,
    Subject,
    /// Free text: every cached field.
    Any,
}

/// One substring condition of a query (`from:`, `to:`, `subject:` or a bare word).
#[derive(Debug, Clone)]
pub struct TextCond {
    pub field: TextField,
    /// Lowercased needle.
    pub needle: String,
    pub negate: bool,
}

impl TextCond {
    pub fn new(field: TextField, value: &str, negate: bool) -> Self {
        Self { field, needle: value.to_lowercase(), negate }
    }
}

fn contains(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(needle)
}

fn addrs_contain(list: &[Addr], needle: &str) -> bool {
    list.iter().any(|a| contains(&a.name, needle) || contains(&a.email, needle))
}

fn from_contains(m: &Meta, needle: &str) -> bool {
    contains(&m.from_name, needle)
        || contains(&m.from_email, needle)
        || m.sender_name.as_deref().is_some_and(|v| contains(v, needle))
        || m.sender_email.as_deref().is_some_and(|v| contains(v, needle))
}

/// Whether cached display fields satisfy every condition.
pub fn meta_matches(m: &Meta, conds: &[TextCond]) -> bool {
    conds.iter().all(|c| {
        let hit = match c.field {
            TextField::From => from_contains(m, &c.needle),
            TextField::To => addrs_contain(&m.to, &c.needle) || addrs_contain(&m.cc, &c.needle),
            TextField::Subject => contains(&m.subject, &c.needle),
            TextField::Any => {
                contains(&m.subject, &c.needle)
                    || from_contains(m, &c.needle)
                    || addrs_contain(&m.to, &c.needle)
                    || addrs_contain(&m.cc, &c.needle)
                    || contains(&m.snippet, &c.needle)
            }
        };
        hit != c.negate
    })
}

/// Which folders a search covers.
#[derive(Debug, Clone)]
pub enum FolderScope {
    /// Exactly this path.
    Exact(String),
    /// `in:name`: the path, or any path ending in `/name` (case-insensitive).
    Named(String),
    /// Every folder with indexed mail except Trash.
    AllButTrash,
}

impl Engine {
    /// Ids of rows whose server copy matches `criteria` (IMAP search syntax), at most `per_folder`
    /// of the newest hits per folder.
    pub async fn search_ids(
        self: &Arc<Self>,
        account_ids: &[String],
        scope: &FolderScope,
        criteria: &str,
        per_folder: usize,
    ) -> Vec<Uuid> {
        let runs = account_ids.iter().map(|account_id| {
            let engine = self.clone();
            async move {
                match tokio::time::timeout(
                    SEARCH_TIMEOUT,
                    engine.search_account(account_id, scope, criteria, per_folder),
                )
                .await
                {
                    Ok(Ok(ids)) => ids,
                    Ok(Err(e)) => {
                        tracing::warn!("Search failed for account {account_id}: {e}");
                        Vec::new()
                    }
                    Err(_) => {
                        tracing::warn!("Search timed out for account {account_id}");
                        Vec::new()
                    }
                }
            }
        });
        futures_util::future::join_all(runs).await.into_iter().flatten().collect()
    }

    /// Ids of the newest rows in scope whose cached display fields contain every condition
    /// as a substring (partial words included).
    pub async fn search_cached(&self, account_ids: &[String], scope: &FolderScope, conds: &[TextCond]) -> Vec<Uuid> {
        if conds.iter().all(|c| c.negate) || self.cache.redis().is_none() {
            return Vec::new();
        }
        let folder_cond = match scope {
            FolderScope::Exact(_) => "m.folder = $2".to_owned(),
            FolderScope::Named(_) => "(m.folder ILIKE $2 OR m.folder ILIKE '%/' || $2)".to_owned(),
            FolderScope::AllButTrash => format!(
                "($2::text IS NULL AND NOT EXISTS (SELECT 1 FROM folders f WHERE f.account_id = m.account_id AND f.path = m.folder AND {TRASH}))"
            ),
        };
        let folder: Option<String> = match scope {
            FolderScope::Exact(p) | FolderScope::Named(p) => Some(p.clone()),
            FolderScope::AllButTrash => None,
        };
        let rows = match self
            .db
            .rows(
                &format!(
                    "SELECT m.id FROM messages m WHERE m.account_id = ANY($1::uuid[]) AND m.is_deleted = false AND {folder_cond} \
                     ORDER BY m.date DESC LIMIT $3"
                ),
                params![account_ids.to_vec(), folder, CACHED_SCAN_ROWS],
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Cached search failed: {e}");
                return Vec::new();
            }
        };
        let ids: Vec<Uuid> = rows.iter().map(|r| r.get::<_, Uuid>("id")).collect();
        let mut hits = Vec::new();
        for chunk in ids.chunks(CACHED_SCAN_BATCH) {
            let keys: Vec<String> = chunk.iter().map(meta_key).collect();
            let metas: Vec<Option<Meta>> = self.cache.get_many(&keys).await;
            hits.extend(
                chunk
                    .iter()
                    .zip(metas)
                    .filter(|(_, m)| m.as_ref().is_some_and(|m| meta_matches(m, conds)))
                    .map(|(id, _)| *id),
            );
        }
        hits
    }

    async fn search_account(
        self: &Arc<Self>,
        account_id: &str,
        scope: &FolderScope,
        criteria: &str,
        per_folder: usize,
    ) -> EResult<Vec<Uuid>> {
        let folders: Vec<String> = match scope {
            FolderScope::Exact(p) => vec![p.clone()],
            FolderScope::Named(name) => self
                .db
                .rows(
                    "SELECT path FROM folders WHERE account_id = $1 AND no_select = false AND (path ILIKE $2 OR path ILIKE $3)",
                    params![account_id, name, format!("%/{name}")],
                )
                .await?
                .iter()
                .map(|r| r.get("path"))
                .collect(),
            FolderScope::AllButTrash => self
                .db
                .rows(
                    &format!(
                        "SELECT DISTINCT m.folder FROM messages m WHERE m.account_id = $1 AND m.is_deleted = false \
                         AND NOT EXISTS (SELECT 1 FROM folders f WHERE f.account_id = m.account_id AND f.path = m.folder AND {TRASH})"
                    ),
                    params![account_id],
                )
                .await?
                .iter()
                .map(|r| r.get("folder"))
                .collect(),
        };
        if folders.is_empty() {
            return Ok(Vec::new());
        }
        let account =
            load_account(&self.db, account_id).await?.ok_or_else(|| EngineError("Account not found".into()))?;
        let mut hits: Vec<(String, Vec<i64>)> = Vec::new();
        {
            let mut lease = self.lease(&account).await?;
            let r: EResult<()> = async {
                let client = lease.client();
                for folder in &folders {
                    client.select(folder).await?;
                    let mut uids = client.search_uids(criteria).await?;
                    uids.sort_unstable_by(|a, b| b.cmp(a));
                    uids.truncate(per_folder);
                    if !uids.is_empty() {
                        hits.push((folder.clone(), uids.into_iter().map(i64::from).collect()));
                    }
                }
                Ok(())
            }
            .await;
            if r.is_err() {
                lease.evict();
            }
            r?;
        }
        let mut ids = Vec::new();
        for (folder, uids) in hits {
            let rows = self
                .db
                .rows(
                    "SELECT id FROM messages WHERE account_id = $1 AND folder = $2 AND uid = ANY($3::bigint[]) AND is_deleted = false",
                    params![account_id, folder, uids],
                )
                .await?;
            ids.extend(rows.iter().map(|r| r.get::<_, Uuid>("id")));
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Meta {
        Meta {
            subject: "Quarterly Reporting deadline".into(),
            from_name: "Maria Example".into(),
            from_email: "maria@example.test".into(),
            to: vec![Addr { name: "Ops Team".into(), email: "ops@example.test".into() }],
            snippet: "Please send the spreadsheets by Friday".into(),
            ..Meta::default()
        }
    }

    #[test]
    fn matches_partial_words_in_any_field() {
        let m = meta();
        assert!(meta_matches(&m, &[TextCond::new(TextField::Any, "report", false)]));
        assert!(meta_matches(&m, &[TextCond::new(TextField::Any, "SPREAD", false)]));
        assert!(meta_matches(&m, &[TextCond::new(TextField::Any, "ari", false)]));
        assert!(meta_matches(&m, &[TextCond::new(TextField::To, "ops@", false)]));
        assert!(!meta_matches(&m, &[TextCond::new(TextField::Subject, "maria", false)]));
    }

    #[test]
    fn all_conditions_must_hold() {
        let m = meta();
        let conds = [TextCond::new(TextField::From, "mar", false), TextCond::new(TextField::Any, "invoice", false)];
        assert!(!meta_matches(&m, &conds));
        assert!(!meta_matches(&m, &[TextCond::new(TextField::Any, "friday", true)]));
    }
}
