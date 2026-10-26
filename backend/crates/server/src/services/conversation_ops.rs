//! Conversation mutations: user overrides (`conversationOverrides.ts`), copy-scoped actions
//! (`conversationActions.ts`), the rebuild worker and jobs (`conversationRebuild*.ts`) and the
//! ingest-failure retry (`conversationIngestRetry.ts`).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use inboxora_core::pg::{Db, Q, Row, Tx};
use inboxora_core::{Error, params};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::conversation::{
    CopyFacts, assert_no_alias_cycle, lock_conversations, provider_identity_for_copy, refresh_aggregates,
    resolve_alias, resolve_own_identities, upsert_copy_tx,
};
use super::mail_utils::{
    adjust_folder_counts, resolve_all_drafts_paths, resolve_all_trash_paths, resolve_trash_folder,
};
use crate::engine::EngineRef;
use crate::engine::cache::Location;
use crate::engine::connect::Account;

/// A failure with Node's optional `statusCode`.
#[derive(Debug, Clone)]
pub struct OpError {
    pub status: Option<u16>,
    pub message: String,
}

impl OpError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { status: None, message: message.into() }
    }
    pub fn status(status: u16, message: impl Into<String>) -> Self {
        Self { status: Some(status), message: message.into() }
    }
}

impl From<Error> for OpError {
    fn from(e: Error) -> Self {
        match &e {
            Error::Http { status, body } => OpError {
                status: Some(status.as_u16()),
                message: body.get("error").and_then(Value::as_str).unwrap_or("").to_owned(),
            },
            _ => OpError::new(e.to_string()),
        }
    }
}

impl From<OpError> for Error {
    fn from(e: OpError) -> Self {
        match e.status {
            Some(s) => Error::status(s, e.message),
            None => Error::internal(e.message),
        }
    }
}

type OpResult<T> = Result<T, OpError>;

fn uuid(s: &str) -> OpResult<Uuid> {
    Uuid::parse_str(s).map_err(|_| OpError::new(format!("invalid input syntax for type uuid: \"{s}\"")))
}

/// `assertConversationOwner`.
async fn assert_owner(tx: &Tx<'_>, user: &str, conversation: Uuid, account: &str) -> OpResult<Row> {
    tx.opt(
        "SELECT id, user_id, manually_locked FROM conversations WHERE id = $1 AND user_id = $2 AND ($3::uuid IS NULL OR account_id = $3) FOR UPDATE",
        params![conversation, user, account],
    )
    .await?
    .ok_or_else(|| OpError::status(404, "Conversation not found"))
}

// ── overrides ───────────────────────────────────────────────────────────────────────────────

const OVERRIDE_TYPES: [&str; 7] = [
    "force-include",
    "force-exclude",
    "manual-split",
    "manual-merge",
    "lock-conversation",
    "unlock-conversation",
    "manual-move",
];

#[derive(Debug, Clone, Default)]
pub struct OverrideRequest {
    pub conversation_id: String,
    pub logical_message_id: Option<String>,
    pub scope: String,
    pub override_type: String,
    pub target_id: Option<String>,
    pub target_conversation_id: Option<String>,
    pub reason: Option<String>,
}

const DESCENDANTS: &str = "WITH RECURSIVE descendants(id, path) AS ( \
    SELECT id, ARRAY[id] FROM logical_messages WHERE id = $3 AND user_id = $2 AND account_id = $4 \
    UNION ALL \
    SELECT lm.id, d.path || lm.id FROM logical_messages lm JOIN descendants d ON lm.parent_logical_message_id = d.id \
     WHERE lm.user_id = $2 AND lm.account_id = $4 AND NOT lm.id = ANY(d.path)) SELECT id FROM descendants";

async fn apply_override_tx(tx: &Tx<'_>, user: &str, r: &OverrideRequest) -> OpResult<Value> {
    if !OVERRIDE_TYPES.contains(&r.override_type.as_str()) {
        return Err(OpError::new("Unsupported conversation override type"));
    }
    let ty = r.override_type.as_str();
    let row = tx
        .opt(
            "SELECT account_id FROM conversations WHERE id = $1 AND user_id = $2 FOR UPDATE",
            params![&r.conversation_id, user],
        )
        .await?
        .ok_or_else(|| OpError::status(404, "Conversation not found"))?;
    let account: Uuid = row.get("account_id");
    let account = account.to_string();
    let acct = Some(account.as_str());
    let conversation = resolve_alias(tx, user, acct, uuid(&r.conversation_id)?).await?;
    let owner = if ty == "manual-merge" { None } else { Some(assert_owner(tx, user, conversation, &account).await?) };
    let message_level = ["force-include", "force-exclude", "manual-split", "manual-move"].contains(&ty);
    let conversation_level = ["lock-conversation", "unlock-conversation", "manual-merge"].contains(&ty);
    if message_level && r.logical_message_id.is_none() {
        return Err(OpError::new(format!("{ty} is a message-level override and requires logicalMessageId")));
    }
    if conversation_level && r.logical_message_id.is_some() {
        return Err(OpError::new(format!(
            "{ty} is a conversation-level override and must not specify logicalMessageId"
        )));
    }
    let logical = r.logical_message_id.as_deref().map(uuid).transpose()?;
    if let Some(lm) = logical {
        let found = if ty == "force-include" {
            tx.opt(
                "SELECT id FROM logical_messages WHERE id = $1 AND user_id = $2 AND account_id = $4 AND conversation_id IS NULL FOR UPDATE",
                params![lm, user, Option::<Uuid>::None, &account],
            )
            .await?
        } else {
            tx.opt(
                "SELECT id FROM logical_messages WHERE id = $1 AND user_id = $2 AND account_id = $4 AND conversation_id = $3 FOR UPDATE",
                params![lm, user, conversation, &account],
            )
            .await?
        };
        if found.is_none() {
            return Err(OpError::status(404, "Logical message not found in eligible conversation"));
        }
    }
    let mut target: Option<Uuid> = r.target_id.as_deref().filter(|s| !s.is_empty()).map(uuid).transpose()?;
    match ty {
        "force-include" => {
            let effective = r
                .target_conversation_id
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| r.target_id.clone())
                .ok_or_else(|| OpError::new("force-include requires a targetConversationId"))?;
            let canonical = resolve_alias(tx, user, acct, uuid(&effective)?).await?;
            assert_owner(tx, user, canonical, &account).await?;
            target = Some(canonical);
        }
        "manual-merge" => {
            let t = target
                .filter(|t| *t != conversation)
                .ok_or_else(|| OpError::new("manual-merge requires a different target conversation"))?;
            let source = resolve_alias(tx, user, acct, conversation).await?;
            let tgt = resolve_alias(tx, user, acct, t).await?;
            if source == tgt {
                return Err(OpError::new("manual-merge would create an alias cycle"));
            }
            lock_conversations(tx, user, &[source, tgt]).await?;
            assert_owner(tx, user, source, &account).await?;
            assert_owner(tx, user, tgt, &account).await?;
            assert_no_alias_cycle(tx, user, &account, source, tgt).await?;
            tx.exec(
                "INSERT INTO conversation_aliases (user_id, account_id, alias_conversation_id, canonical_conversation_id, reason) VALUES ($1,$2,$3,$4,$5) \
                 ON CONFLICT (user_id, account_id, alias_conversation_id) DO UPDATE SET canonical_conversation_id = EXCLUDED.canonical_conversation_id, reason = EXCLUDED.reason",
                params![user, &account, source, tgt, r.reason.clone().unwrap_or_else(|| "manual-merge".into())],
            )
            .await?;
            for sql in [
                "UPDATE messages SET conversation_id = $1, conversation_user_id = $2 WHERE conversation_id = $3 AND conversation_user_id = $2 AND account_id = $4",
                "UPDATE logical_messages SET conversation_id = $1 WHERE conversation_id = $3 AND user_id = $2 AND account_id = $4",
                "UPDATE conversation_evidence SET conversation_id = $1 WHERE conversation_id = $3 AND user_id = $2 AND account_id = $4",
                "UPDATE provider_thread_mappings SET conversation_id = $1, last_seen_at = NOW() WHERE conversation_id = $3 AND user_id = $2 AND account_id = $4",
                "UPDATE conversation_overrides SET conversation_id = $1 WHERE conversation_id = $3 AND user_id = $2 AND account_id = $4",
            ] {
                tx.exec(sql, params![tgt, user, source, &account]).await?;
            }
            refresh_aggregates(tx, user, tgt).await?;
            refresh_aggregates(tx, user, source).await?;
            target = Some(tgt);
        }
        "manual-split" => {
            let lm = logical.ok_or_else(|| OpError::new("manual-split requires logicalMessageId"))?;
            if !["message-only", "message-with-descendants"].contains(&r.scope.as_str()) {
                return Err(OpError::new("Unsupported manual-split scope"));
            }
            let created: Uuid = tx
                .opt(
                    "INSERT INTO conversations (user_id, account_id, kind, first_message_at, last_message_at, segment_number) \
                     SELECT user_id, account_id, 'manual_conversation', first_message_at, last_message_at, segment_number + 1 \
                     FROM conversations WHERE id = $1 RETURNING id",
                    params![conversation],
                )
                .await?
                .ok_or_else(|| OpError::new("split insert failed"))?
                .get("id");
            let with_desc = r.scope == "message-with-descendants";
            if with_desc {
                tx.exec(
                    "WITH RECURSIVE descendants(id, path) AS ( \
                       SELECT id, ARRAY[id] FROM logical_messages WHERE id = $2 AND user_id = $3 AND account_id = $4 \
                       UNION ALL \
                       SELECT lm.id, d.path || lm.id FROM logical_messages lm JOIN descendants d ON lm.parent_logical_message_id = d.id \
                        WHERE lm.user_id = $3 AND NOT lm.id = ANY(d.path)) \
                     UPDATE logical_messages lm SET conversation_id = $1, parent_logical_message_id = CASE WHEN lm.id = $2 THEN NULL ELSE lm.parent_logical_message_id END \
                     WHERE lm.id IN (SELECT id FROM descendants)",
                    params![created, lm, user, &account],
                )
                .await?;
            } else {
                tx.exec(
                    "UPDATE logical_messages SET conversation_id = $1, parent_logical_message_id = NULL WHERE id = $2 AND user_id = $3 AND account_id = $4",
                    params![created, lm, user, &account],
                )
                .await?;
            }
            let moved_sql = if with_desc {
                DESCENDANTS.to_owned()
            } else {
                "SELECT id FROM logical_messages WHERE id = $3 AND user_id = $2 AND account_id = $4".to_owned()
            };
            tx.exec(
                &format!("UPDATE messages SET conversation_id = $1, conversation_user_id = $2 WHERE logical_message_id IN ({moved_sql}) AND conversation_user_id = $2 AND account_id = $4"),
                params![created, user, lm, &account],
            )
            .await?;
            tx.exec(
                &format!("UPDATE conversation_evidence SET conversation_id = $1 WHERE logical_message_id IN ({moved_sql}) AND user_id = $2 AND account_id = $4"),
                params![created, user, lm, &account],
            )
            .await?;
            tx.exec(
                &format!(
                    "UPDATE conversation_overrides SET conversation_id = $1, target_id = CASE WHEN target_id = $4 THEN $1 ELSE target_id END \
                     WHERE logical_message_id IN ({}) AND user_id = $2 AND account_id = $5",
                    moved_sql.replace("$4", "$5")
                ),
                params![created, user, lm, conversation, &account],
            )
            .await?;
            tx.exec(
                "UPDATE logical_messages child SET parent_logical_message_id = NULL WHERE child.user_id = $1 AND child.account_id = $3 AND child.conversation_id <> $2 \
                 AND child.parent_logical_message_id IN (SELECT id FROM logical_messages WHERE conversation_id = $2 AND account_id = $3)",
                params![user, created, &account],
            )
            .await?;
            if tx
                .opt(
                    "SELECT 1 FROM logical_messages child JOIN logical_messages parent ON parent.id = child.parent_logical_message_id \
                     WHERE child.user_id = $1 AND child.account_id = $2 AND (child.conversation_id IS DISTINCT FROM parent.conversation_id) LIMIT 1",
                    params![user, &account],
                )
                .await?
                .is_some()
            {
                return Err(OpError::new("conversation split left a cross-conversation parent edge"));
            }
            refresh_aggregates(tx, user, conversation).await?;
            refresh_aggregates(tx, user, created).await?;
            target = Some(created);
        }
        "manual-move" => {
            let lm = logical.ok_or_else(|| OpError::new("manual-move requires logicalMessageId"))?;
            let t = target
                .filter(|t| *t != conversation)
                .ok_or_else(|| OpError::new("manual-move requires a different target conversation"))?;
            let tgt = resolve_alias(tx, user, acct, t).await?;
            if tgt == conversation {
                return Err(OpError::new("manual-move target is the same conversation (alias)"));
            }
            lock_conversations(tx, user, &[conversation, tgt]).await?;
            assert_owner(tx, user, tgt, &account).await?;
            let moved: Vec<String> = tx
                .rows(
                    "WITH RECURSIVE descendants(id, path) AS ( \
                       SELECT id, ARRAY[id] FROM logical_messages WHERE id = $1 AND user_id = $2 AND conversation_id = $3 AND account_id = $4 \
                       UNION ALL \
                       SELECT lm.id, d.path || lm.id FROM logical_messages lm JOIN descendants d ON lm.parent_logical_message_id = d.id \
                        WHERE lm.user_id = $2 AND lm.account_id = $4 AND NOT lm.id = ANY(d.path)) \
                     SELECT lm.id FROM logical_messages lm JOIN descendants d ON d.id = lm.id",
                    params![lm, user, conversation, &account],
                )
                .await?
                .iter()
                .map(|r| r.get::<_, Uuid>("id").to_string())
                .collect();
            tx.exec(
                "UPDATE logical_messages SET conversation_id = $1, parent_logical_message_id = CASE WHEN id = $2 THEN NULL ELSE parent_logical_message_id END \
                 WHERE id = ANY($3::uuid[]) AND user_id = $4 AND account_id = $5",
                params![tgt, lm, moved.clone(), user, &account],
            )
            .await?;
            tx.exec(
                "UPDATE messages SET conversation_id = $1, conversation_user_id = $2 WHERE logical_message_id = ANY($3::uuid[]) AND conversation_user_id = $2 AND account_id = $4",
                params![tgt, user, moved.clone(), &account],
            )
            .await?;
            tx.exec(
                "UPDATE conversation_evidence SET conversation_id = $1 WHERE logical_message_id = ANY($2::uuid[]) AND user_id = $3 AND account_id = $4",
                params![tgt, moved, user, &account],
            )
            .await?;
            tx.exec(
                "UPDATE logical_messages child SET parent_logical_message_id = NULL WHERE child.user_id = $1 AND child.account_id = $3 AND child.conversation_id <> $2 \
                 AND child.parent_logical_message_id IN (SELECT id FROM logical_messages WHERE conversation_id = $2 AND account_id = $3)",
                params![user, tgt, &account],
            )
            .await?;
            if tx
                .opt(
                    "SELECT 1 FROM logical_messages child JOIN logical_messages parent ON parent.id = child.parent_logical_message_id \
                     WHERE child.user_id = $1 AND child.account_id = $2 AND child.conversation_id IS DISTINCT FROM parent.conversation_id LIMIT 1",
                    params![user, &account],
                )
                .await?
                .is_some()
            {
                return Err(OpError::new("manual-move left a cross-conversation parent edge"));
            }
            refresh_aggregates(tx, user, conversation).await?;
            refresh_aggregates(tx, user, tgt).await?;
            target = Some(tgt);
        }
        "lock-conversation" => {
            tx.exec(
                "UPDATE conversations SET manually_locked = true, updated_at = NOW() WHERE id = $1",
                params![conversation],
            )
            .await?;
        }
        "unlock-conversation" => {
            tx.exec(
                "UPDATE conversations SET manually_locked = false, updated_at = NOW() WHERE id = $1",
                params![conversation],
            )
            .await?;
        }
        _ => {}
    }
    tx.exec(
        "INSERT INTO conversation_overrides (user_id, account_id, conversation_id, logical_message_id, override_type, target_id, target_user_id, reason) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        params![user, &account, conversation, logical, ty, target, target.map(|_| user.to_owned()), &r.reason],
    )
    .await?;
    let locked = ty == "lock-conversation"
        || owner.is_some_and(|o| o.get::<_, Option<bool>>("manually_locked").unwrap_or(false));
    Ok(
        json!({ "conversationId": conversation.to_string(), "overrideType": ty, "targetId": target.map(|t| t.to_string()), "manuallyLocked": locked }),
    )
}

/// `applyConversationOverride` (serializable, retried like Node's `withTransaction`).
pub async fn apply_override(db: &Db, user: &str, r: OverrideRequest) -> OpResult<Value> {
    let result: Arc<Mutex<Option<OpError>>> = Arc::default();
    let (user, r2, slot) = (user.to_owned(), r, result.clone());
    let out = db
        .transaction(true, 2, move |tx| {
            let (user, r, slot) = (user.clone(), r2.clone(), slot.clone());
            Box::pin(async move {
                match apply_override_tx(tx, &user, &r).await {
                    Ok(v) => Ok(v),
                    Err(e) => {
                        let err: Error = match e.status {
                            Some(_) => e.clone().into(),
                            None => Error::internal(e.message.clone()),
                        };
                        *slot.lock() = Some(e);
                        Err(err)
                    }
                }
            })
        })
        .await;
    out.map_err(|e| result.lock().take().unwrap_or_else(|| OpError::from(e)))
}

pub async fn list_overrides(db: &Db, user: &str, conversation: &str) -> inboxora_core::Result<Vec<Value>> {
    let Some(row) = db
        .opt("SELECT account_id FROM conversations WHERE id = $1 AND user_id = $2", params![conversation, user])
        .await?
    else {
        return Ok(vec![]);
    };
    let account: Uuid = row.get("account_id");
    let account = account.to_string();
    let canonical = resolve_alias(
        db,
        user,
        Some(&account),
        Uuid::parse_str(conversation).map_err(|e| Error::internal(e.to_string()))?,
    )
    .await?;
    db.json_rows(
        "SELECT * FROM conversation_overrides WHERE user_id = $1 AND account_id = $3 AND conversation_id = $2 ORDER BY created_at DESC",
        params![user, canonical, &account],
    )
    .await
}

// ── copy-scoped actions ─────────────────────────────────────────────────────────────────────

pub const COPY_SCOPES: [&str; 4] =
    ["THIS_COPY", "ALL_COPIES_OF_LOGICAL_MESSAGE", "COPIES_ON_THIS_ACCOUNT", "WHOLE_CONVERSATION"];

#[derive(Debug, Clone)]
struct Copy {
    id: String,
    account_id: String,
    folder: String,
    uid: Option<i64>,
    is_read: bool,
    extra: Map<String, Value>,
}

impl Copy {
    fn json(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("account_id".into(), json!(self.account_id));
        m.insert("folder".into(), json!(self.folder));
        m.insert("uid".into(), json!(self.uid));
        m.insert("is_read".into(), json!(self.is_read));
        m.extend(self.extra.clone());
        m
    }
}

fn assert_scope(scope: &str) -> OpResult<()> {
    if COPY_SCOPES.contains(&scope) {
        Ok(())
    } else {
        Err(OpError::status(400, format!("Unsupported copy scope: {scope}")))
    }
}

async fn resolve_physical(
    tx: &Tx<'_>,
    user: &str,
    conversation: &str,
    scope: &str,
    copy_id: Option<&str>,
    logical: Option<&str>,
) -> OpResult<(Uuid, String, Vec<Copy>)> {
    assert_scope(scope)?;
    let row = tx
        .opt(
            "SELECT account_id FROM conversations WHERE id = $1 AND user_id = $2 FOR UPDATE",
            params![conversation, user],
        )
        .await?
        .ok_or_else(|| OpError::status(404, "Conversation not found or not owned by user"))?;
    let account: Uuid = row.get("account_id");
    let account = account.to_string();
    let canonical = resolve_alias(tx, user, Some(&account), uuid(conversation)?).await?;
    let (selector, extra) = match (copy_id, logical) {
        (Some(c), _) => ("m.id = $4", Some(c.to_owned())),
        (None, Some(l)) => ("m.logical_message_id = $4", Some(l.to_owned())),
        _ => (
            "m.id = (SELECT latest.id FROM messages latest JOIN email_accounts latest_account ON latest_account.id = latest.account_id \
              WHERE latest.conversation_id = $2 AND latest.account_id = $3 AND latest_account.user_id = $1 AND latest.is_deleted = false \
              ORDER BY latest.date DESC NULLS LAST, latest.id DESC LIMIT 1)",
            None,
        ),
    };
    let sql = format!(
        "SELECT m.id, m.account_id, m.logical_message_id, m.conversation_id FROM messages m JOIN email_accounts a ON a.id = m.account_id AND a.user_id = $1 \
         WHERE m.conversation_id = $2 AND m.account_id = $3 AND m.is_deleted = false AND {selector} FOR UPDATE"
    );
    let selected = match &extra {
        Some(e) => tx.rows(&sql, params![user, canonical, &account, uuid(e)?]).await?,
        None => tx.rows(&sql, params![user, canonical, &account]).await?,
    };
    let sel = selected.first().ok_or_else(|| OpError::status(404, "Selected copy not found or not owned by user"))?;
    let sel_id: Uuid = sel.get("id");
    let sel_logical: Option<Uuid> = sel.get("logical_message_id");
    let (where_, key): (&str, Uuid) = match scope {
        "THIS_COPY" => ("m.account_id = $2 AND m.id = $3", sel_id),
        "ALL_COPIES_OF_LOGICAL_MESSAGE" | "COPIES_ON_THIS_ACCOUNT" => match sel_logical {
            Some(l) => ("m.account_id = $2 AND m.logical_message_id = $3", l),
            None => ("m.account_id = $2 AND m.id = $3", sel_id),
        },
        _ => ("m.account_id = $2 AND m.conversation_id = $3", canonical),
    };
    let rows = tx
        .rows(
            &format!(
                "SELECT m.id, m.account_id, m.logical_message_id, m.conversation_id, m.folder, m.uid, m.is_read FROM messages m \
                 JOIN email_accounts a ON a.id = m.account_id AND a.user_id = $1 WHERE m.is_deleted = false AND {where_} FOR UPDATE"
            ),
            params![user, &account, key],
        )
        .await?;
    let copies = rows
        .iter()
        .map(|r| {
            let mut extra = Map::new();
            extra.insert(
                "logical_message_id".into(),
                json!(r.get::<_, Option<Uuid>>("logical_message_id").map(|u| u.to_string())),
            );
            extra.insert(
                "conversation_id".into(),
                json!(r.get::<_, Option<Uuid>>("conversation_id").map(|u| u.to_string())),
            );
            Copy {
                id: r.get::<_, Uuid>("id").to_string(),
                account_id: r.get::<_, Uuid>("account_id").to_string(),
                folder: r.get("folder"),
                uid: r.get("uid"),
                is_read: r.get::<_, Option<bool>>("is_read").unwrap_or(false),
                extra,
            }
        })
        .collect();
    Ok((canonical, sel_id.to_string(), copies))
}

async fn folder_row(tx: &Tx<'_>, account: &str, path: &str) -> OpResult<Option<(String, Option<String>)>> {
    Ok(tx
        .opt(
            "SELECT path, special_use FROM folders WHERE account_id = $1 AND path = $2 AND no_select = false LIMIT 1",
            params![account, path],
        )
        .await?
        .map(|r| (r.get("path"), r.get("special_use"))))
}

async fn archive_destination(
    tx: &Tx<'_>,
    account: &str,
    mappings: Option<&Value>,
) -> OpResult<Option<(String, Option<String>)>> {
    if let Some(m) = mappings.and_then(|m| m.get("archive")).and_then(Value::as_str).filter(|s| !s.is_empty())
        && let Some(r) = folder_row(tx, account, m).await?
    {
        return Ok(Some(r));
    }
    Ok(tx
        .opt(
            "SELECT path, special_use FROM folders WHERE account_id = $1 AND no_select = false \
             AND (special_use IN ('\\Archive','\\All') OR lower(name) LIKE '%archive%') \
             ORDER BY CASE WHEN special_use = '\\Archive' THEN 0 WHEN lower(name) LIKE '%archive%' THEN 1 ELSE 2 END LIMIT 1",
            params![account],
        )
        .await?
        .map(|r| (r.get("path"), r.get("special_use"))))
}

/// Provider moves grouped by (account, source, destination); every copy must succeed.
async fn move_with_provider(
    tx: &Tx<'_>,
    engine: &EngineRef,
    rows: &[Copy],
    dest: &HashMap<String, String>,
) -> OpResult<(Vec<(Copy, String, Option<i64>)>, Vec<(Account, String)>)> {
    let mut groups: Vec<((String, String, String), Vec<&Copy>)> = Vec::new();
    for r in rows {
        let Some(d) = dest.get(&r.id) else { continue };
        let key = (r.account_id.clone(), r.folder.clone(), d.clone());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(r),
            None => groups.push((key, vec![r])),
        }
    }
    let mut moved = Vec::new();
    let mut resync = Vec::new();
    for ((account_id, from, to), group) in groups {
        let account = tx
            .json_opt("SELECT * FROM email_accounts WHERE id = $1", params![&account_id])
            .await?
            .map(Account::from_map)
            .ok_or_else(|| OpError::status(404, "Account not found"))?;
        let uids: Vec<u32> = group.iter().filter_map(|r| r.uid).map(|u| u as u32).collect();
        let res = engine.bulk_move_messages(&account, &uids, &from, &to).await;
        for r in group {
            let uid = r.uid.unwrap_or(0) as u32;
            if !res.succeeded.contains(&uid) {
                return Err(OpError::status(502, format!("Provider move failed for copy {}", r.id)));
            }
            let new_uid = res.uid_map.get(&uid).map(|n| i64::from(*n));
            if new_uid.is_none() {
                resync.push((account.clone(), to.clone()));
            }
            moved.push((r.clone(), to.clone(), new_uid));
        }
    }
    Ok((moved, resync))
}

fn resync(engine: &EngineRef, items: Vec<(Account, String)>) {
    for (account, folder) in items {
        let e = engine.clone();
        tokio::spawn(async move { e.sync_folder_on_demand(&account, &folder).await });
    }
}

async fn archive_rows(tx: &Tx<'_>, engine: &EngineRef, rows: &[Copy], user: &str) -> OpResult<Vec<Map<String, Value>>> {
    let mut dest: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut mappings: HashMap<String, Value> = HashMap::new();
    for r in rows {
        if !mappings.contains_key(&r.account_id) {
            let m = tx
                .opt(
                    "SELECT folder_mappings FROM email_accounts WHERE id = $1 AND user_id = $2",
                    params![&r.account_id, user],
                )
                .await?
                .and_then(|row| row.get::<_, Option<Value>>("folder_mappings"))
                .unwrap_or_else(|| json!({}));
            mappings.insert(r.account_id.clone(), m);
        }
        let d = archive_destination(tx, &r.account_id, mappings.get(&r.account_id))
            .await?
            .ok_or_else(|| OpError::status(409, "No archive folder configured for account"))?;
        dest.insert(r.id.clone(), d);
    }
    let paths: HashMap<String, String> = dest.iter().map(|(k, v)| (k.clone(), v.0.clone())).collect();
    let (moved, resyncs) = move_with_provider(tx, engine, rows, &paths).await?;
    let mut changed = Vec::new();
    for (r, _, new_uid) in moved {
        let (path, special) = dest.get(&r.id).cloned().unwrap_or_default();
        let mut m = r.json();
        m.insert("destinationFolder".into(), json!(path));
        m.insert("special_use".into(), json!(special));
        if special.as_deref() == Some("\\All") {
            if tx.opt("DELETE FROM messages WHERE id = $1 RETURNING id, folder", params![&r.id]).await?.is_some() {
                changed.push(m);
            }
        } else if let Some(n) = new_uid {
            if let Some(u) = tx
                .opt(
                    "UPDATE messages SET folder = $1, uid = $2 WHERE id = $3 RETURNING id, folder",
                    params![&path, n, &r.id],
                )
                .await?
            {
                m.insert("folder".into(), json!(u.get::<_, String>("folder")));
                changed.push(m);
            }
        } else {
            tx.exec("DELETE FROM messages WHERE id = $1", params![&r.id]).await?;
            m.insert("needsResync".into(), json!(true));
            changed.push(m);
        }
    }
    resync(engine, resyncs);
    Ok(changed)
}

async fn delete_rows(tx: &Tx<'_>, engine: &EngineRef, rows: &[Copy]) -> OpResult<Vec<Map<String, Value>>> {
    let mut changed = Vec::new();
    let mut by_account: Vec<(String, Vec<&Copy>)> = Vec::new();
    for r in rows {
        match by_account.iter_mut().find(|(a, _)| *a == r.account_id) {
            Some((_, v)) => v.push(r),
            None => by_account.push((r.account_id.clone(), vec![r])),
        }
    }
    for (account_id, account_rows) in by_account {
        let account = tx
            .json_opt("SELECT * FROM email_accounts WHERE id = $1", params![&account_id])
            .await?
            .map(Account::from_map)
            .ok_or_else(|| OpError::status(404, "Account not found"))?;
        let mappings = account.0.get("folder_mappings");
        let trash = resolve_trash_folder(tx, &account_id, mappings)
            .await?
            .ok_or_else(|| OpError::status(422, "No Trash folder configured for this account"))?;
        let trash_paths = resolve_all_trash_paths(tx, &account_id, mappings).await?;
        let drafts = resolve_all_drafts_paths(tx, &account_id, mappings).await?;
        let (permanent, to_trash): (Vec<&Copy>, Vec<&Copy>) =
            account_rows.into_iter().partition(|r| trash_paths.contains(&r.folder) || drafts.contains(&r.folder));
        let mut by_folder: Vec<(String, Vec<&Copy>)> = Vec::new();
        for r in permanent {
            match by_folder.iter_mut().find(|(f, _)| *f == r.folder) {
                Some((_, v)) => v.push(r),
                None => by_folder.push((r.folder.clone(), vec![r])),
            }
        }
        for (folder, fr) in by_folder {
            let uids: Vec<u32> = fr.iter().filter_map(|r| r.uid).map(|u| u as u32).collect();
            let (succeeded, _) = engine.bulk_permanent_delete(&account, &uids, &folder).await;
            for r in fr {
                if !succeeded.contains(&(r.uid.unwrap_or(0) as u32)) {
                    return Err(OpError::status(502, format!("Provider delete failed for copy {}", r.id)));
                }
                tx.exec("DELETE FROM messages WHERE id = $1", params![&r.id]).await?;
                changed.push(r.json());
            }
        }
        if !to_trash.is_empty() {
            let dest: HashMap<String, String> = to_trash.iter().map(|r| (r.id.clone(), trash.clone())).collect();
            let owned: Vec<Copy> = to_trash.into_iter().cloned().collect();
            let (moved, resyncs) = move_with_provider(tx, engine, &owned, &dest).await?;
            for (r, _, new_uid) in moved {
                match new_uid {
                    None => {
                        tx.exec("DELETE FROM messages WHERE id = $1", params![&r.id]).await?;
                    }
                    Some(n) => {
                        tx.exec(
                            "DELETE FROM messages WHERE account_id = $1 AND uid = $2 AND folder = $3 AND id <> $4",
                            params![&r.account_id, n, &trash, &r.id],
                        )
                        .await?;
                        tx.exec("UPDATE messages SET folder = $1, uid = $2 WHERE id = $3", params![&trash, n, &r.id])
                            .await?;
                    }
                }
                let mut m = r.json();
                m.insert("destinationFolder".into(), json!(trash));
                changed.push(m);
            }
            resync(engine, resyncs);
        }
    }
    Ok(changed)
}

async fn move_rows(
    tx: &Tx<'_>,
    engine: &EngineRef,
    rows: &[Copy],
    target: &str,
) -> OpResult<(Vec<Map<String, Value>>, usize)> {
    let mut dest = HashMap::new();
    for r in rows {
        let d = folder_row(tx, &r.account_id, target)
            .await?
            .ok_or_else(|| OpError::status(409, format!("Destination folder not found for account: {target}")))?;
        dest.insert(r.id.clone(), d.0);
    }
    let (moved, resyncs) = move_with_provider(tx, engine, rows, &dest).await?;
    let mut out = Vec::new();
    for (r, _, new_uid) in moved {
        let mut m = Map::new();
        m.insert("id".into(), json!(r.id));
        match new_uid {
            None => {
                tx.exec("DELETE FROM messages WHERE id = $1", params![&r.id]).await?;
                m.insert("folder".into(), json!(target));
                m.insert("needsResync".into(), json!(true));
                out.push(m);
            }
            Some(n) => {
                if tx
                    .opt(
                        "UPDATE messages SET folder = $1, uid = $2 WHERE id = $3 RETURNING id, folder",
                        params![target, n, &r.id],
                    )
                    .await?
                    .is_some()
                {
                    m.insert("folder".into(), json!(target));
                    out.push(m);
                }
            }
        }
    }
    resync(engine, resyncs);
    let n = out.len();
    Ok((out, n))
}

fn folder_counts(engine: &EngineRef, rows: &[Map<String, Value>], action: &str, user: &str) {
    if rows.is_empty() || !["archive", "move", "delete"].contains(&action) {
        return;
    }
    let mut deltas: Vec<((String, String), i64, i64)> = Vec::new();
    let mut add = |a: &str, f: &str, t: i64, u: i64| {
        let k = (a.to_owned(), f.to_owned());
        match deltas.iter_mut().find(|(x, ..)| *x == k) {
            Some(e) => {
                e.1 += t;
                e.2 += u;
            }
            None => deltas.push((k, t, u)),
        }
    };
    for r in rows {
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("");
        let unread = i64::from(!r.get("is_read").and_then(Value::as_bool).unwrap_or(false));
        add(s("account_id"), s("folder"), -1, -unread);
        let dest = s("destinationFolder");
        if !dest.is_empty() && dest != s("folder") && !(action == "archive" && s("special_use") == "\\All") {
            add(s("account_id"), dest, 1, unread);
        }
    }
    for ((acct, folder), t, u) in deltas {
        adjust_folder_counts(&engine.db, &acct, &folder, t, u);
        engine.broadcast(json!({ "type": "folder_updated", "folder": folder, "accountId": acct }), Some(user));
    }
}

fn broadcast_flags(engine: &EngineRef, rows: &[Map<String, Value>], action: &str, user: &str) {
    if rows.is_empty() || !["read", "star"].contains(&action) {
        return;
    }
    let field = if action == "read" { "is_read" } else { "is_starred" };
    let mut by: Vec<(String, Vec<Value>)> = Vec::new();
    for r in rows {
        let Some(v) = r.get(field).filter(|v| v.is_boolean()) else { continue };
        let acct = r.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
        let change = json!({ "id": r.get("id"), field: v });
        match by.iter_mut().find(|(a, _)| *a == acct) {
            Some((_, c)) => c.push(change),
            None => by.push((acct, vec![change])),
        }
    }
    for (acct, changes) in by {
        engine.broadcast(json!({ "type": "message_flags", "accountId": acct, "changes": changes }), Some(user));
    }
}

async fn push_flags(engine: &EngineRef, rows: &[Map<String, Value>], flag: &str, value: bool) {
    let mut accounts: HashMap<String, Account> = HashMap::new();
    for r in rows {
        let a = r.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
        if !accounts.contains_key(&a)
            && let Ok(Some(acc)) = crate::engine::connect::load_account(&engine.db, &a).await
        {
            accounts.insert(a, acc);
        }
    }
    futures_util::future::join_all(rows.iter().map(|r| {
        let a = r.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
        let account = accounts.get(&a).cloned();
        let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
        let folder = r.get("folder").and_then(Value::as_str).unwrap_or("").to_owned();
        let uid = inboxora_core::json::int(r.get("uid"));
        async move {
            let (Some(account), Some(uid)) = (account, uid) else { return };
            match engine.set_flag(&account, uid, &folder, flag, value).await {
                Ok(()) => engine.resolve_flag_push(&a, &id, flag),
                Err(e) => {
                    tracing::error!("CE {flag} push failed: {e}");
                    engine.enqueue_flag_push(&a, &id, flag, value);
                }
            }
        }
    }))
    .await;
}

const REFRESH_CONVERSATION: &str = "UPDATE conversations c SET \
    logical_message_count = COALESCE((SELECT COUNT(DISTINCT m.logical_message_id) FROM messages m WHERE m.conversation_id = c.id AND m.is_deleted = false), 0), \
    copy_count = COALESCE((SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id AND m.is_deleted = false), 0), \
    unread_count = COALESCE((SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id AND m.is_deleted = false AND NOT m.is_read), 0), \
    last_message_at = (SELECT MAX(m.date) FROM messages m WHERE m.conversation_id = c.id AND m.is_deleted = false), updated_at = NOW() \
    WHERE c.id = $1 AND c.user_id = $2";

#[derive(Debug, Clone, Default)]
pub struct ActionRequest {
    pub conversation_id: String,
    pub scope: String,
    pub copy_id: Option<String>,
    pub logical_message_id: Option<String>,
    pub action: String,
    pub is_read: bool,
    pub is_starred: bool,
    pub target_folder: Option<String>,
}

const ACTIONS: [&str; 5] = ["archive", "move", "delete", "read", "star"];

/// Apply one action to the resolved copies (shared by single and bulk).
async fn act(
    tx: &Tx<'_>,
    engine: &EngineRef,
    user: &str,
    r: &ActionRequest,
    rows: &[Copy],
) -> OpResult<(Vec<Map<String, Value>>, usize)> {
    let ids: Vec<String> = rows.iter().map(|c| c.id.clone()).collect();
    let merge = |changed: Vec<(String, Map<String, Value>)>| -> Vec<Map<String, Value>> {
        changed
            .into_iter()
            .map(|(id, extra)| {
                let mut m = rows.iter().find(|c| c.id == id).map(Copy::json).unwrap_or_default();
                m.extend(extra);
                m
            })
            .collect()
    };
    Ok(match r.action.as_str() {
        "read" | "star" => {
            let (col, marker, v) = if r.action == "read" {
                ("is_read", "read_changed_at", r.is_read)
            } else {
                ("is_starred", "star_changed_at", r.is_starred)
            };
            let updated = tx
                .rows(&format!("UPDATE messages SET {col} = $1, {marker} = NOW() WHERE id = ANY($2::uuid[]) RETURNING id, {col}"), params![v, ids])
                .await?;
            let n = updated.len();
            (
                merge(
                    updated
                        .iter()
                        .map(|row| {
                            let mut m = Map::new();
                            m.insert(col.into(), json!(row.get::<_, bool>(1)));
                            (row.get::<_, Uuid>(0).to_string(), m)
                        })
                        .collect(),
                ),
                n,
            )
        }
        "delete" => {
            let v = delete_rows(tx, engine, rows).await?;
            let n = v.len();
            (v, n)
        }
        "archive" => {
            let v = archive_rows(tx, engine, rows, user).await?;
            let n = v.len();
            (v, n)
        }
        "move" => {
            let target = r
                .target_folder
                .clone()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| OpError::status(400, "targetFolder required"))?;
            let (v, n) = move_rows(tx, engine, rows, &target).await?;
            let ids: Vec<(String, Map<String, Value>)> =
                v.into_iter().map(|m| (m.get("id").and_then(Value::as_str).unwrap_or("").to_owned(), m)).collect();
            (merge(ids), n)
        }
        other => return Err(OpError::status(400, format!("Unsupported conversation action: {other}"))),
    })
}

async fn run_tx<T: Send + 'static>(
    db: &Db,
    f: impl for<'t> Fn(&'t Tx<'t>) -> futures_util::future::BoxFuture<'t, OpResult<T>> + Send + Sync + 'static,
) -> OpResult<T> {
    let f = Arc::new(f);
    let slot: Arc<Mutex<Option<OpError>>> = Arc::default();
    let s2 = slot.clone();
    // No retry: provider moves are not idempotent across a rolled-back attempt.
    db.transaction(true, 0, move |tx| {
        let (f, slot) = (f.clone(), s2.clone());
        Box::pin(async move {
            match f(tx).await {
                Ok(v) => Ok(v),
                Err(e) => {
                    *slot.lock() = Some(e.clone());
                    Err(Error::internal(e.message))
                }
            }
        })
    })
    .await
    .map_err(|e| slot.lock().take().unwrap_or_else(|| OpError::from(e)))
}

/// `applyConversationAction`.
pub async fn apply_action(engine: &EngineRef, user: &str, r: ActionRequest) -> OpResult<Value> {
    if r.conversation_id.is_empty() {
        return Err(OpError::status(400, "conversationId is required"));
    }
    assert_scope(&r.scope)?;
    if !ACTIONS.contains(&r.action.as_str()) {
        return Err(OpError::status(400, format!("Unsupported conversation action: {}", r.action)));
    }
    if r.action == "move" && r.target_folder.as_deref().is_none_or(str::is_empty) {
        return Err(OpError::status(400, "targetFolder required"));
    }
    let (e, u, req) = (engine.clone(), user.to_owned(), r.clone());
    let (mutated, response) = run_tx(&engine.db, move |tx| {
        let (e, u, r) = (e.clone(), u.clone(), req.clone());
        Box::pin(async move {
            let (canonical, selected, rows) = resolve_physical(
                tx,
                &u,
                &r.conversation_id,
                &r.scope,
                r.copy_id.as_deref(),
                r.logical_message_id.as_deref(),
            )
            .await?;
            let (mutated, count) = act(tx, &e, &u, &r, &rows).await?;
            tx.exec(REFRESH_CONVERSATION, params![canonical, &u]).await?;
            folder_counts(&e, &mutated, &r.action, &u);
            broadcast_flags(&e, &mutated, &r.action, &u);
            let affected: Vec<Value> = mutated.iter().map(|m| m.get("id").cloned().unwrap_or(Value::Null)).collect();
            Ok((
                mutated,
                json!({ "ok": true, "action": r.action, "scope": r.scope, "conversationId": canonical.to_string(),
                        "selectedCopyId": selected, "affectedIds": affected, "affectedCount": count }),
            ))
        })
    })
    .await?;
    if r.action == "read" {
        push_flags(engine, &mutated, "\\Seen", r.is_read).await;
    }
    if r.action == "star" {
        push_flags(engine, &mutated, "\\Flagged", r.is_starred).await;
    }
    Ok(response)
}

#[derive(Debug, Clone, Default)]
pub struct BulkItem {
    pub conversation_id: String,
    pub copy_id: Option<String>,
    pub logical_message_id: Option<String>,
}

/// `applyBulkConversationAction`.
pub async fn apply_bulk_action(
    engine: &EngineRef,
    user: &str,
    items: Vec<BulkItem>,
    base: ActionRequest,
) -> OpResult<Value> {
    if items.is_empty() {
        return Err(OpError::status(400, "conversationIds or items required"));
    }
    assert_scope(&base.scope)?;
    if base.scope != "WHOLE_CONVERSATION" && items.iter().any(|i| i.copy_id.is_none() && i.logical_message_id.is_none())
    {
        return Err(OpError::status(
            400,
            format!("Bulk scope {} requires copyId or logicalMessageId selectors", base.scope),
        ));
    }
    if !ACTIONS.contains(&base.action.as_str()) {
        return Err(OpError::status(400, format!("Unsupported conversation action: {}", base.action)));
    }
    let (e, u, b, its) = (engine.clone(), user.to_owned(), base.clone(), items.clone());
    let (flag_rows, response) = run_tx(&engine.db, move |tx| {
        let (e, u, b, its) = (e.clone(), u.clone(), b.clone(), its.clone());
        Box::pin(async move {
            let mut sorted = its.clone();
            sorted.sort_by(|a, b| a.conversation_id.cmp(&b.conversation_id));
            let mut all_ids: Vec<Value> = Vec::new();
            let mut count = 0usize;
            let mut flag_rows: Vec<Map<String, Value>> = Vec::new();
            for item in sorted {
                let r = ActionRequest {
                    conversation_id: item.conversation_id.clone(),
                    copy_id: item.copy_id.clone().or_else(|| b.copy_id.clone()),
                    logical_message_id: item.logical_message_id.clone().or_else(|| b.logical_message_id.clone()),
                    ..b.clone()
                };
                let (canonical, _, rows) = resolve_physical(
                    tx,
                    &u,
                    &r.conversation_id,
                    &r.scope,
                    r.copy_id.as_deref(),
                    r.logical_message_id.as_deref(),
                )
                .await?;
                let (mutated, _) = act(tx, &e, &u, &r, &rows).await?;
                tx.exec(REFRESH_CONVERSATION, params![canonical, &u]).await?;
                folder_counts(&e, &mutated, &r.action, &u);
                if r.action == "read" || r.action == "star" {
                    let (field, v) =
                        if r.action == "read" { ("is_read", r.is_read) } else { ("is_starred", r.is_starred) };
                    let flagged: Vec<Map<String, Value>> = mutated
                        .iter()
                        .map(|m| {
                            let mut m = m.clone();
                            m.insert(field.into(), json!(v));
                            m
                        })
                        .collect();
                    broadcast_flags(&e, &flagged, &r.action, &u);
                    flag_rows.extend(flagged);
                }
                count += mutated.len();
                all_ids.extend(mutated.iter().map(|m| m.get("id").cloned().unwrap_or(Value::Null)));
            }
            Ok((
                flag_rows,
                json!({ "ok": true, "action": b.action, "scope": b.scope,
                        "conversationIds": its.iter().map(|i| i.conversation_id.clone()).collect::<Vec<_>>(),
                        "affectedIds": all_ids, "affectedCount": count }),
            ))
        })
    })
    .await?;
    if base.action == "read" {
        push_flags(engine, &flag_rows, "\\Seen", base.is_read).await;
    }
    if base.action == "star" {
        push_flags(engine, &flag_rows, "\\Flagged", base.is_starred).await;
    }
    Ok(response)
}

// ── rebuild ─────────────────────────────────────────────────────────────────────────────────

const ALL_ACCOUNTS_SCOPE: &str = "00000000-0000-0000-0000-000000000000";
const SNAPSHOT_COLS: &str = "conversation_id, logical_message_id, canonical_message_id, provider_message_id, provider_thread_id, threading_reason, threading_confidence, threading_algorithm_version";

#[derive(Debug, Clone, Default)]
pub struct Cursor {
    pub date: Option<String>,
    pub id: Option<String>,
    pub is_null: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BatchResult {
    pub scanned: usize,
    pub updated: usize,
    pub would_change: usize,
    pub complete: bool,
    pub next: Option<Cursor>,
}

async fn snapshot(tx: &Tx<'_>, id: Uuid) -> inboxora_core::Result<Option<Value>> {
    Ok(tx
        .json_opt(&format!("SELECT {SNAPSHOT_COLS} FROM messages WHERE id = $1"), params![id])
        .await?
        .map(Value::Object))
}

async fn rebuild_rows(
    tx: &Tx<'_>,
    rows: &[Map<String, Value>],
    user: &str,
    facts: &HashMap<Uuid, CopyFacts>,
) -> inboxora_core::Result<usize> {
    let mut changed = 0;
    for row in rows {
        let id = Uuid::parse_str(row.get("id").and_then(Value::as_str).unwrap_or_default())
            .map_err(|e| Error::internal(e.to_string()))?;
        // The server no longer has it (or it could not be fetched): leave its links as they are.
        let Some(facts) = facts.get(&id) else { continue };
        let account = row.get("account_id").and_then(Value::as_str).unwrap_or_default();
        let host = row.get("imap_host").and_then(Value::as_str).unwrap_or_default();
        let before = snapshot(tx, id).await?;
        let identities: Vec<Value> = resolve_own_identities(tx, account, Some(&facts.delivery_addresses), None)
            .await?
            .into_iter()
            .map(Value::String)
            .collect();
        let provider = provider_identity_for_copy(row, host);
        upsert_copy_tx(tx, id, facts, &identities, Some(&provider), user, true).await?;
        if before != snapshot(tx, id).await? {
            changed += 1;
        }
    }
    Ok(changed)
}

/// One batch of `rebuildConversationCopies` for one account.
pub async fn rebuild_batch(
    engine: &EngineRef,
    user: &str,
    account: &str,
    limit: i64,
    dry_run: bool,
    force: bool,
    cursor: Option<Cursor>,
) -> inboxora_core::Result<BatchResult> {
    let db = &engine.db;
    let limit = limit.clamp(1, 500);
    let scope = if account.is_empty() { ALL_ACCOUNTS_SCOPE } else { account };
    let checkpoint = db
        .json_opt(
            "SELECT * FROM conversation_rebuild_checkpoints WHERE user_id = $1 AND scope_account_id = $2",
            params![user, scope],
        )
        .await?;
    if !dry_run
        && checkpoint.as_ref().and_then(|c| c.get("status")).and_then(Value::as_str) == Some("complete")
        && !force
        && cursor.is_none()
    {
        return Ok(BatchResult { complete: true, ..Default::default() });
    }
    let effective = cursor.clone().or_else(|| {
        if force {
            return None;
        }
        checkpoint.as_ref().map(|c| Cursor {
            date: c.get("last_message_date").and_then(Value::as_str).map(str::to_owned),
            id: c.get("last_message_id").and_then(Value::as_str).map(str::to_owned),
            is_null: c.get("last_sort_is_null").and_then(Value::as_bool).unwrap_or(false),
        })
    });
    // Threading needs each copy's headers, which live on the IMAP server: fetch the page's
    // headers (through the cache) before the transaction, then rebuild exactly those rows.
    let (page_values, page_pred) = cursor_predicate(user, account, effective.as_ref(), limit);
    let page: Vec<Map<String, Value>> = db
        .json_rows(
            &format!(
                "SELECT m.id, m.account_id, m.folder, m.uid FROM messages m \
                 JOIN email_accounts a ON a.id = m.account_id AND a.user_id = $1 \
                 WHERE m.is_deleted = false AND m.account_id = $2 {page_pred} ORDER BY (m.date IS NULL), m.date ASC, m.id ASC LIMIT ${}",
                page_values.len()
            ),
            &page_values,
        )
        .await?
        .into_iter()
        .filter_map(|v| if let Value::Object(m) = v { Some(m) } else { None })
        .collect();
    let locations: Vec<Location> = page.iter().filter_map(Location::from_row).collect();
    let page_ids: Vec<String> = locations.iter().map(|l| l.id.to_string()).collect();
    let facts: HashMap<Uuid, CopyFacts> =
        engine.message_meta(&locations).await.iter().map(|(id, m)| (*id, CopyFacts::from_meta(m))).collect();
    let (user_s, account_s, lock_key) = (user.to_owned(), account.to_owned(), format!("conversation-rebuild:{user}"));
    let effective2 = effective.clone();
    // The advisory lock and the batch share one session.
    let result = db
        .transaction(true, 3, move |tx| {
            let (user, account, lock_key, effective) = (user_s.clone(), account_s.clone(), lock_key.clone(), effective2.clone());
            let (page_ids, facts, page_len) = (page_ids.clone(), facts.clone(), page.len());
            Box::pin(async move {
                tx.exec("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))", params![&lock_key, format!("{lock_key}:2")]).await?;
                let rows: Vec<Map<String, Value>> = tx
                    .json_rows(
                        "SELECT m.*, a.user_id, a.email_address, a.imap_host, a.protocol FROM messages m \
                         JOIN email_accounts a ON a.id = m.account_id AND a.user_id = $1 \
                         WHERE m.is_deleted = false AND m.account_id = $2 AND m.id = ANY($3::uuid[]) \
                         ORDER BY (m.date IS NULL), m.date ASC, m.id ASC",
                        params![&user, &account, page_ids],
                    )
                    .await?
                    .into_iter()
                    .filter_map(|v| if let Value::Object(m) = v { Some(m) } else { None })
                    .collect();
                let _ = &effective;
                let changed = rebuild_rows(tx, &rows, &user, &facts).await?;
                let last = rows.last();
                let complete = (page_len as i64) < limit;
                if !dry_run {
                    let last_date = last.and_then(|l| l.get("date")).and_then(Value::as_str).map(str::to_owned);
                    let is_null = match last {
                        Some(l) => Some(l.get("date").is_none_or(Value::is_null)),
                        None => effective.as_ref().map(|c| c.is_null),
                    };
                    tx.exec(
                        "INSERT INTO conversation_rebuild_checkpoints (user_id, scope_account_id, last_sort_is_null, last_message_date, last_message_id, status, dry_run, scanned_count, updated_count, diagnostics) \
                         VALUES ($1,$2,$3,$4,$5,$6,false,$7,$8,$9::jsonb) ON CONFLICT (user_id, scope_account_id) DO UPDATE SET \
                           last_sort_is_null = EXCLUDED.last_sort_is_null, last_message_date = EXCLUDED.last_message_date, last_message_id = EXCLUDED.last_message_id, \
                           status = EXCLUDED.status, scanned_count = conversation_rebuild_checkpoints.scanned_count + EXCLUDED.scanned_count, \
                           updated_count = conversation_rebuild_checkpoints.updated_count + EXCLUDED.updated_count, diagnostics = EXCLUDED.diagnostics, updated_at = NOW()",
                        params![
                            &user,
                            if account.is_empty() { ALL_ACCOUNTS_SCOPE } else { account.as_str() },
                            is_null,
                            last_date,
                            last.and_then(|l| l.get("id")).and_then(Value::as_str).map(str::to_owned),
                            if complete { "complete" } else { "paused" },
                            rows.len() as i64,
                            changed as i64,
                            json!({ "limit": limit, "complete": complete, "force": force }).to_string()
                        ],
                    )
                    .await?;
                } else {
                    // A dry run must leave nothing behind.
                    return Err(Error::Internal(format!("__dry_run__:{changed}:{page_len}:{}", serde_json::to_string(&last.cloned()).unwrap_or_default())));
                }
                Ok((page_len, changed, complete, last.cloned()))
            })
        })
        .await;
    let (scanned, changed, complete, last) = match result {
        Ok(v) => v,
        Err(Error::Internal(m)) if m.starts_with("__dry_run__:") => {
            let mut parts = m.splitn(4, ':').skip(1);
            let changed = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let scanned: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let last: Option<Map<String, Value>> = parts.next().and_then(|s| serde_json::from_str(s).ok()).flatten();
            (scanned, changed, (scanned as i64) < limit, last)
        }
        Err(e) => return Err(e),
    };
    let next = (!complete).then(|| Cursor {
        date: last.as_ref().and_then(|l| l.get("date")).and_then(Value::as_str).map(str::to_owned),
        id: last.as_ref().and_then(|l| l.get("id")).and_then(Value::as_str).map(str::to_owned),
        is_null: last.as_ref().is_some_and(|l| l.get("date").is_none_or(Value::is_null)),
    });
    Ok(BatchResult { scanned, updated: if dry_run { 0 } else { changed }, would_change: changed, complete, next })
}

/// The rebuild page predicate after `cursor` and its bind values (`$1` user, `$2` account,
/// then the cursor, then the limit last).
fn cursor_predicate(
    user: &str,
    account: &str,
    cursor: Option<&Cursor>,
    limit: i64,
) -> (Vec<inboxora_core::pg::Val>, String) {
    let mut values: Vec<inboxora_core::pg::Val> = vec![user.into(), account.into()];
    let mut pred = String::new();
    if let Some(c) = cursor.filter(|c| c.id.is_some()) {
        if c.is_null {
            values.push(c.id.clone().into());
            pred = format!("AND (m.date IS NULL AND m.id > ${}::uuid)", values.len());
        } else {
            values.push(c.date.clone().into());
            values.push(c.id.clone().into());
            let n = values.len();
            pred = format!(
                "AND ((m.date IS NOT NULL AND (m.date > ${d}::timestamptz OR (m.date = ${d}::timestamptz AND m.id > ${n}::uuid))) OR m.date IS NULL)",
                d = n - 1
            );
        }
    }
    values.push(limit.into());
    (values, pred)
}

#[derive(Debug, Clone)]
struct Job {
    user_id: String,
    status: String,
    created: Instant,
    result: Value,
    error: Option<String>,
}

static JOBS: LazyLock<Mutex<Vec<(String, Job)>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static RATE: LazyLock<crate::http::limit::Windows> = LazyLock::new(|| crate::http::limit::Windows::new(10_000));

/// `consumeConversationRebuildRateLimit`: 2 requests per minute per user.
pub fn consume_rebuild_rate_limit(user: &str) -> bool {
    RATE.hit(user, Duration::from_secs(60)).0 <= 2
}

pub async fn record_rebuild_audit(
    db: &Db,
    user: &str,
    job: &str,
    action: &str,
    details: Value,
) -> inboxora_core::Result<()> {
    db.exec(
        "INSERT INTO conversation_rebuild_audit (user_id, job_id, action, details) VALUES ($1,$2,$3,$4::jsonb)",
        params![user, job, action, details.to_string()],
    )
    .await?;
    Ok(())
}

/// `startConversationRebuildJob`.
pub fn start_rebuild_job(
    engine: &EngineRef,
    user: &str,
    account: Option<String>,
    limit: i64,
    dry_run: bool,
    force: bool,
) -> String {
    let id = Uuid::new_v4().to_string();
    {
        let mut jobs = JOBS.lock();
        jobs.push((
            id.clone(),
            Job {
                user_id: user.to_owned(),
                status: "queued".into(),
                created: Instant::now(),
                result: Value::Null,
                error: None,
            },
        ));
        while jobs.len() > 100 {
            jobs.remove(0);
        }
    }
    let (engine, user, job_id) = (engine.clone(), user.to_owned(), id.clone());
    let set = move |id: &str, f: &dyn Fn(&mut Job)| {
        if let Some((_, j)) = JOBS.lock().iter_mut().find(|(k, _)| k == id) {
            f(j);
        }
    };
    tokio::spawn(async move {
        set(&job_id, &|j| j.status = "running".into());
        let run = async {
            let scopes: Vec<String> = match &account {
                Some(a) => vec![a.clone()],
                None => engine
                    .db
                    .rows("SELECT id FROM email_accounts WHERE user_id = $1 ORDER BY id", params![&user])
                    .await?
                    .iter()
                    .map(|r| r.get::<_, Uuid>("id").to_string())
                    .collect(),
            };
            let (mut scanned, mut updated, mut would, mut batches) = (0usize, 0usize, 0usize, 0usize);
            let mut last: Option<BatchResult> = None;
            for scope in &scopes {
                let mut cursor: Option<Cursor> = None;
                loop {
                    let r =
                        rebuild_batch(&engine, &user, scope, limit, dry_run, force && cursor.is_none(), cursor.clone())
                            .await?;
                    scanned += r.scanned;
                    updated += r.updated;
                    would += r.would_change;
                    batches += 1;
                    cursor = r.next.clone();
                    let complete = r.complete;
                    let result = json!({ "scanned": scanned, "updated": updated, "would_change": would, "changed": updated, "batches": batches,
                                         "accounts": scopes.len(), "complete": complete, "dryRun": dry_run,
                                         "wouldChange": r.would_change, "totalScanned": r.scanned, "totalUpdated": r.updated });
                    set(&job_id, &|j| j.result = result.clone());
                    last = Some(r);
                    if complete {
                        break;
                    }
                }
            }
            if last.is_none() {
                let result = json!({ "scanned": 0, "updated": 0, "would_change": 0, "changed": 0, "batches": 0, "accounts": 0, "complete": true, "dryRun": dry_run });
                set(&job_id, &|j| j.result = result.clone());
            }
            Ok::<_, Error>(())
        };
        match run.await {
            Ok(()) => {
                set(&job_id, &|j| j.status = "complete".into());
                let result = JOBS
                    .lock()
                    .iter()
                    .find(|(k, _)| *k == job_id)
                    .map(|(_, j)| j.result.clone())
                    .unwrap_or(Value::Null);
                let _ = record_rebuild_audit(
                    &engine.db,
                    &user,
                    &job_id,
                    "completed",
                    json!({ "accountId": account, "dryRun": dry_run, "result": result }),
                )
                .await;
            }
            Err(e) => {
                let msg = e.to_string();
                set(&job_id, &|j| {
                    j.error = Some(msg.clone());
                    j.status = "failed".into();
                });
                let _ = record_rebuild_audit(
                    &engine.db,
                    &user,
                    &job_id,
                    "failed",
                    json!({ "accountId": account, "dryRun": dry_run, "error": msg }),
                )
                .await;
            }
        }
    });
    id
}

pub fn get_rebuild_job(user: &str, job: &str) -> Option<Value> {
    let jobs = JOBS.lock();
    let (_, j) = jobs.iter().find(|(k, _)| k == job)?;
    if j.user_id != user || j.created.elapsed() > Duration::from_secs(3600) {
        return None;
    }
    Some(json!({ "jobId": job, "status": j.status, "result": j.result, "error": j.error }))
}

// ── ingest retry ────────────────────────────────────────────────────────────────────────────

/// `retryConversationIngestFailures`: re-run persistence for recorded failures.
pub async fn retry_ingest_failures(engine: &EngineRef, limit: i64) -> inboxora_core::Result<Vec<Value>> {
    let db = &engine.db;
    let limit = limit.clamp(1, 100);
    let failures = db
        .transaction(false, 0, move |tx| {
            Box::pin(async move {
                let rows = tx
                    .json_rows(
                        "SELECT * FROM conversation_ingest_failures WHERE resolved_at IS NULL AND next_attempt_at <= NOW() \
                         ORDER BY next_attempt_at ASC, created_at ASC LIMIT $1 FOR UPDATE SKIP LOCKED",
                        params![limit],
                    )
                    .await?;
                for r in &rows {
                    tx.exec(
                        "UPDATE conversation_ingest_failures SET attempts = attempts + 1, next_attempt_at = NOW() + INTERVAL '5 minutes', updated_at = NOW() WHERE id = $1",
                        params![r.get("id").cloned().unwrap_or(Value::Null)],
                    )
                    .await?;
                }
                Ok(rows)
            })
        })
        .await?;
    let mut out = Vec::new();
    for f in failures {
        let id = f.get("id").cloned().unwrap_or(Value::Null);
        let (row_id, user) = (
            f.get("message_row_id").and_then(Value::as_str).unwrap_or("").to_owned(),
            f.get("user_id").and_then(Value::as_str).unwrap_or("").to_owned(),
        );
        let location = db
            .json_opt("SELECT id, account_id, folder, uid FROM messages WHERE id = $1", params![&row_id])
            .await?
            .as_ref()
            .and_then(Location::from_row);
        let facts = match &location {
            Some(l) => engine.message_meta(std::slice::from_ref(l)).await.get(&l.id).map(CopyFacts::from_meta),
            None => None,
        };
        let r = db
            .transaction(true, 2, move |tx| {
                let (row_id, user, facts) = (row_id.clone(), user.clone(), facts.clone());
                Box::pin(async move {
                    let facts = facts.ok_or_else(|| Error::internal("Message is no longer on the server"))?;
                    let row = tx
                        .json_opt(
                            "SELECT m.*, a.user_id, a.email_address, a.imap_host, a.protocol FROM messages m JOIN email_accounts a ON a.id = m.account_id \
                             WHERE m.id = $1 AND a.user_id = $2 FOR UPDATE",
                            params![&row_id, &user],
                        )
                        .await?
                        .ok_or_else(|| Error::internal("Message row no longer exists"))?;
                    rebuild_rows_single(tx, &row, &user, &facts).await
                })
            })
            .await;
        match r {
            Ok(v) => {
                db.exec(
                    "UPDATE conversation_ingest_failures SET resolved_at = NOW(), updated_at = NOW() WHERE id = $1",
                    params![&id],
                )
                .await?;
                out.push(json!({ "id": id, "resolved": true, "logicalMessageId": v.0, "conversationId": v.1 }));
            }
            Err(e) => out.push(json!({ "id": id, "resolved": false, "error": e.to_string() })),
        }
    }
    Ok(out)
}

async fn rebuild_rows_single(
    tx: &Tx<'_>,
    row: &Map<String, Value>,
    user: &str,
    facts: &CopyFacts,
) -> inboxora_core::Result<(String, Option<String>)> {
    let id = Uuid::parse_str(row.get("id").and_then(Value::as_str).unwrap_or_default())
        .map_err(|e| Error::internal(e.to_string()))?;
    let account = row.get("account_id").and_then(Value::as_str).unwrap_or_default();
    let host = row.get("imap_host").and_then(Value::as_str).unwrap_or_default();
    let identities: Vec<Value> = resolve_own_identities(tx, account, Some(&facts.delivery_addresses), None)
        .await?
        .into_iter()
        .map(Value::String)
        .collect();
    let provider = provider_identity_for_copy(row, host);
    let r = upsert_copy_tx(tx, id, facts, &identities, Some(&provider), user, false).await?;
    Ok((r.logical_message_id.to_string(), r.conversation_id.map(|c| c.to_string())))
}

/// Run the ingest retry every five minutes.
pub fn start_ingest_retry(engine: EngineRef) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_secs(300));
        iv.tick().await;
        loop {
            iv.tick().await;
            super::tasks::tick("ingest retry", async {
                if let Err(e) = retry_ingest_failures(&engine, 25).await {
                    tracing::warn!("Conversation ingest retry failed: {e}");
                }
            })
            .await;
        }
    });
}
