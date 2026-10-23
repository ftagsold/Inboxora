//! Server-side search. Message text lives on the IMAP server, so matching runs there (`UID
//! SEARCH`) and the hits are mapped back to header-index rows.

use std::sync::Arc;
use std::time::Duration;

use inboxora_core::params;
use inboxora_core::pg::Q;
use uuid::Uuid;

use super::connect::load_account;
use super::{EResult, Engine, EngineError};

/// Each account's search must finish within this budget; slower accounts contribute no hits.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(25);

const TRASH: &str = "(f.special_use = '\\Trash' OR lower(f.name) LIKE '%trash%' OR lower(f.name) LIKE '%deleted%')";

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
