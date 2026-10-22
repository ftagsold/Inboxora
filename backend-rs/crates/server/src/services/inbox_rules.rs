//! Inbox rules and the block list applied to newly arrived mail (port of `inboxRules.ts`), and
//! the idempotent rule forwarder (`ruleForwarder.ts`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use inboxora_core::params;
use inboxora_core::pg::{Db, Q};
use inboxora_mail::rules::{compile_js_regex, is_dangerous_regex};
use serde_json::{Map, Value};
use uuid::Uuid;

use super::mail_compose::{Compose, Part, account_transport};
use super::mail_utils::{
    DeleteStrategy, adjust_folder_counts, delete_strategy, is_all_mail_folder, resolve_all_trash_paths,
    resolve_archive_folder, resolve_trash_folder,
};
use crate::engine::connect::Account;
use crate::engine::sync::NewMessage;
use crate::engine::{EResult, Engine, EngineError};

#[derive(Debug, Clone, Default)]
pub struct RuleMessage {
    pub id: Uuid,
    pub uid: i64,
    pub folder: String,
    pub subject: String,
    pub from_email: String,
    pub from_name: String,
    pub is_read: bool,
    pub has_attachments: bool,
    pub to: Vec<(String, String)>,
    pub headers: HashMap<String, String>,
    pub body_text: Option<String>,
}

impl RuleMessage {
    pub fn from_new(m: &NewMessage) -> Self {
        Self {
            id: m.id,
            uid: i64::from(m.parsed.uid),
            folder: m.folder.clone(),
            subject: m.parsed.subject.clone(),
            from_email: m.parsed.from_email.clone(),
            from_name: m.parsed.from_name.clone(),
            is_read: m.parsed.is_read,
            has_attachments: m.parsed.has_attachments,
            to: m.parsed.to.iter().map(|a| (a.email.clone(), a.name.clone())).collect(),
            headers: m.parsed.headers.0.iter().cloned().collect(),
            body_text: None,
        }
    }
}

fn norm(v: &str) -> String {
    v.to_lowercase().trim().to_owned()
}

fn match_operator(op: &str, field: &str, rule: &str) -> bool {
    let (f, r) = (norm(field), norm(rule));
    if r.is_empty() {
        return false;
    }
    match op {
        "contains" => f.contains(&r),
        "not_contains" => !f.contains(&r),
        "equals" => f == r,
        "starts_with" => f.starts_with(&r),
        "ends_with" => f.ends_with(&r),
        "regex" => {
            if is_dangerous_regex(rule) {
                return false;
            }
            compile_js_regex(&r, true).is_some_and(|re| re.is_match(&f).unwrap_or(false))
        }
        _ => false,
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn evaluate_condition(c: &Value, m: &RuleMessage) -> bool {
    let Some(field) = c.get("field").and_then(Value::as_str) else { return false };
    let (op, value) = (s(c, "operator"), s(c, "value"));
    match field {
        "from" => {
            if op == "not_contains" {
                match_operator("not_contains", &m.from_email, value)
                    && match_operator("not_contains", &m.from_name, value)
            } else {
                match_operator(op, &m.from_email, value) || match_operator(op, &m.from_name, value)
            }
        }
        "to" => {
            if m.to.is_empty() {
                return false;
            }
            if op == "not_contains" {
                m.to.iter()
                    .all(|(e, n)| match_operator("not_contains", e, value) && match_operator("not_contains", n, value))
            } else {
                m.to.iter().any(|(e, n)| match_operator(op, e, value) || match_operator(op, n, value))
            }
        }
        "subject" => match_operator(op, &m.subject, value),
        "has_attachment" => m.has_attachments,
        "read_status" => {
            if value == "read" {
                m.is_read
            } else {
                !m.is_read
            }
        }
        "body" => match_operator(op, m.body_text.as_deref().unwrap_or(""), value),
        "header" => {
            let name = s(c, "headerName").to_lowercase().trim().to_owned();
            if name.is_empty() {
                return false;
            }
            match_operator(op, m.headers.get(&name).map_or("", String::as_str), value)
        }
        _ => false,
    }
}

fn evaluate_rule(rule: &Map<String, Value>, m: &RuleMessage) -> bool {
    let conditions = rule.get("conditions").and_then(Value::as_array).cloned().unwrap_or_default();
    if conditions.is_empty() {
        return false;
    }
    if rule.get("condition_logic").and_then(Value::as_str) == Some("OR") {
        conditions.iter().any(|c| evaluate_condition(c, m))
    } else {
        conditions.iter().all(|c| evaluate_condition(c, m))
    }
}

#[derive(Default)]
struct ResolverCache {
    archive: Option<(Option<String>, bool)>,
    trash: Option<(Option<String>, HashSet<String>)>,
}

/// Move one message and record the new location (shared by move/archive/delete/block).
async fn move_one(
    engine: &Arc<Engine>,
    account: &Account,
    m: &mut RuleMessage,
    dest: &str,
    all_mail: bool,
) -> EResult<()> {
    let (src_folder, src_uid) = (m.folder.clone(), m.uid);
    engine.guard_move_uid(account.id(), &src_folder, src_uid);
    let r = async {
        let res = engine.bulk_move_messages(account, &[src_uid as u32], &src_folder, dest).await;
        if !res.failed.is_empty() {
            return Err(EngineError(format!("IMAP move to {dest} failed for uid {src_uid}")));
        }
        let new_uid = res.uid_map.get(&(src_uid as u32)).copied().map(i64::from);
        if all_mail {
            engine.db.exec("DELETE FROM messages WHERE id = $1", params![m.id]).await?;
        } else if let Some(n) = new_uid {
            engine.db.exec("UPDATE messages SET folder = $1, uid = $2 WHERE id = $3", params![dest, n, m.id]).await?;
        } else {
            engine.guard_move_uid(account.id(), dest, src_uid);
            engine.db.exec("UPDATE messages SET folder = $1 WHERE id = $2", params![dest, m.id]).await?;
            let (e, a, d) = (engine.clone(), account.id().to_owned(), dest.to_owned());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                e.unguard_move_uid(&a, &d, src_uid);
            });
        }
        let unread = if m.is_read { 0 } else { 1 };
        adjust_folder_counts(&engine.db, account.id(), &src_folder, -1, -unread);
        if !all_mail {
            adjust_folder_counts(&engine.db, account.id(), dest, 1, unread);
        }
        m.folder = dest.to_owned();
        m.uid = new_uid.unwrap_or(src_uid);
        Ok(())
    }
    .await;
    engine.unguard_move_uid(account.id(), &src_folder, src_uid);
    r
}

async fn apply_action(
    engine: &Arc<Engine>,
    action: &Value,
    m: &mut RuleMessage,
    account: &Account,
    rule_id: &str,
    cache: &mut ResolverCache,
) -> EResult<bool> {
    let mappings = account.0.get("folder_mappings");
    match s(action, "type") {
        "forward" => {
            let Some(recipient) = action.get("value").and_then(Value::as_str) else { return Ok(false) };
            forward_rule_message(engine, rule_id, m.id, account, recipient).await.map(|_| true)
        }
        "mark_read" => {
            engine
                .db
                .exec("UPDATE messages SET is_read = true, read_changed_at = NOW() WHERE id = $1", params![m.id])
                .await?;
            let (e, a, id, uid, folder) = (engine.clone(), account.clone(), m.id, m.uid, m.folder.clone());
            tokio::spawn(async move {
                if let Err(err) = e.set_flag(&a, uid, &folder, "\\Seen", true).await {
                    tracing::error!("inboxRules: setFlag \\Seen failed: {err}");
                    e.enqueue_flag_push(a.id(), &id.to_string(), "\\Seen", true);
                }
            });
            if !m.is_read {
                adjust_folder_counts(&engine.db, account.id(), &m.folder, 0, -1);
            }
            m.is_read = true;
            Ok(false)
        }
        "star" => {
            engine
                .db
                .exec("UPDATE messages SET is_starred = true, star_changed_at = NOW() WHERE id = $1", params![m.id])
                .await?;
            let (e, a, id, uid, folder) = (engine.clone(), account.clone(), m.id, m.uid, m.folder.clone());
            tokio::spawn(async move {
                if let Err(err) = e.set_flag(&a, uid, &folder, "\\Flagged", true).await {
                    tracing::error!("inboxRules: setFlag \\Flagged failed: {err}");
                    e.enqueue_flag_push(a.id(), &id.to_string(), "\\Flagged", true);
                }
            });
            Ok(false)
        }
        "move" => {
            let dest = s(action, "value").to_owned();
            if dest.is_empty() {
                return Ok(false);
            }
            move_one(engine, account, m, &dest, false).await?;
            Ok(true)
        }
        "archive" => {
            if cache.archive.is_none() {
                let folder = resolve_archive_folder(&engine.db, account.id(), mappings).await?;
                let all = match &folder {
                    Some(f) => is_all_mail_folder(&engine.db, account.id(), Some(f)).await?,
                    None => false,
                };
                cache.archive = Some((folder, all));
            }
            let (Some(folder), all) = cache.archive.clone().unwrap_or_default() else { return Ok(false) };
            move_one(engine, account, m, &folder, all).await.map_err(|e| {
                if e.0.starts_with("IMAP move") {
                    EngineError(format!("IMAP archive failed for uid {}", m.uid))
                } else {
                    e
                }
            })?;
            Ok(true)
        }
        "delete" => {
            if cache.trash.is_none() {
                let (t, all) = tokio::join!(
                    resolve_trash_folder(&engine.db, account.id(), mappings),
                    resolve_all_trash_paths(&engine.db, account.id(), mappings)
                );
                cache.trash = Some((t?, all?));
            }
            let (trash, all) = cache.trash.clone().unwrap_or_default();
            match delete_strategy(&m.folder, trash.as_deref(), Some(&all)) {
                DeleteStrategy::NoTrash => Ok(false),
                DeleteStrategy::Move(dest) => {
                    move_one(engine, account, m, &dest, false)
                        .await
                        .map_err(|_| EngineError(format!("IMAP delete-move failed for uid {}", m.uid)))?;
                    Ok(true)
                }
                DeleteStrategy::Expunge => {
                    engine.set_flag(account, m.uid, &m.folder, "\\Deleted", true).await?;
                    engine.db.exec("UPDATE messages SET is_deleted = true WHERE id = $1", params![m.id]).await?;
                    adjust_folder_counts(&engine.db, account.id(), &m.folder, -1, if m.is_read { 0 } else { -1 });
                    Ok(true)
                }
            }
        }
        _ => Ok(false),
    }
}

/// `applyInboxRules`: evaluate the account's rules in priority order; destination actions
/// wait until every forward rule has run. Returns (remaining ids, muted ids).
pub async fn run_rules(
    engine: &Arc<Engine>,
    account: &Account,
    messages: &mut [RuleMessage],
) -> EResult<(HashSet<Uuid>, HashSet<Uuid>)> {
    let mut removed: HashSet<Uuid> = HashSet::new();
    let mut muted: HashSet<Uuid> = HashSet::new();
    if messages.is_empty() {
        return Ok((removed, muted));
    }
    let rules: Vec<Map<String, Value>> = match engine
        .db
        .json_rows(
            "SELECT * FROM inbox_rules WHERE user_id = $1 AND enabled = true AND (account_id IS NULL OR account_id = $2) ORDER BY priority ASC, created_at ASC",
            params![account.user_id(), account.id()],
        )
        .await
    {
        Ok(r) => r.into_iter().filter_map(|v| if let Value::Object(m) = v { Some(m) } else { None }).collect(),
        Err(e) => {
            tracing::error!("inboxRules: failed to load rules: {e}");
            return Ok((removed, muted));
        }
    };
    if rules.is_empty() {
        return Ok((removed, muted));
    }
    let needs_body = rules.iter().any(|r| {
        r.get("conditions").and_then(Value::as_array).is_some_and(|c| c.iter().any(|c| s(c, "field") == "body"))
    });
    if needs_body {
        let ids: Vec<String> = messages.iter().map(|m| m.id.to_string()).collect();
        match engine.db.rows("SELECT id, body_text FROM messages WHERE id = ANY($1::uuid[])", params![ids]).await {
            Ok(rows) => {
                let by: HashMap<Uuid, Option<String>> =
                    rows.iter().map(|r| (r.get("id"), r.get("body_text"))).collect();
                for m in messages.iter_mut() {
                    m.body_text = Some(by.get(&m.id).cloned().flatten().unwrap_or_default());
                    if m.body_text.as_deref() == Some("") {
                        tracing::warn!(
                            "inboxRules: body_text not yet available for message {} — body rules will not match (account uses lazy body fetch)",
                            m.id
                        );
                    }
                }
            }
            Err(e) => tracing::error!("inboxRules: failed to fetch body_text for rules: {e}"),
        }
    }
    let last_forward = rules.iter().rposition(|r| {
        r.get("actions").and_then(Value::as_array).is_some_and(|a| a.iter().any(|a| s(a, "type") == "forward"))
    });
    let mut cache = ResolverCache::default();
    let mut blocked: HashSet<Uuid> = HashSet::new();
    for m in messages.iter_mut() {
        let mut deferred: Vec<(Value, String)> = Vec::new();
        let mut barrier_passed = last_forward.is_none();
        for (index, rule) in rules.iter().enumerate() {
            let rule_id = rule.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
            let matches = evaluate_rule(rule, m);
            if !matches {
                if !barrier_passed && Some(index) == last_forward {
                    barrier_passed = true;
                    flush(engine, account, m, &mut deferred, &mut removed, &blocked, &mut muted, &mut cache).await;
                }
                continue;
            }
            let actions = rule.get("actions").and_then(Value::as_array).cloned().unwrap_or_default();
            for action in actions.iter().filter(|a| s(a, "type") == "forward") {
                if apply_action(engine, action, m, account, &rule_id, &mut cache).await.is_err() {
                    blocked.insert(m.id);
                    tracing::error!("inboxRules: forward action failed; destination actions suppressed");
                }
            }
            let stop = rule.get("stop_processing").and_then(Value::as_bool).unwrap_or(false);
            if !barrier_passed && (Some(index) == last_forward || stop) {
                barrier_passed = true;
                flush(engine, account, m, &mut deferred, &mut removed, &blocked, &mut muted, &mut cache).await;
            }
            let mut dest_seen = false;
            for action in actions.iter().filter(|a| s(a, "type") != "forward") {
                let ty = s(action, "type");
                let is_dest = matches!(ty, "move" | "archive" | "delete");
                if is_dest && (dest_seen || removed.contains(&m.id) || blocked.contains(&m.id)) {
                    continue;
                }
                if is_dest {
                    dest_seen = true;
                    if !barrier_passed {
                        deferred.push((action.clone(), rule_id.clone()));
                        continue;
                    }
                }
                execute(engine, account, m, action, &rule_id, is_dest, &mut removed, &mut muted, &mut cache).await;
            }
            if stop {
                break;
            }
        }
        flush(engine, account, m, &mut deferred, &mut removed, &blocked, &mut muted, &mut cache).await;
    }
    Ok((removed, muted))
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    engine: &Arc<Engine>,
    account: &Account,
    m: &mut RuleMessage,
    action: &Value,
    rule_id: &str,
    is_dest: bool,
    removed: &mut HashSet<Uuid>,
    muted: &mut HashSet<Uuid>,
    cache: &mut ResolverCache,
) {
    match apply_action(engine, action, m, account, rule_id, cache).await {
        Ok(acted) => {
            if is_dest && acted {
                removed.insert(m.id);
            }
            if s(action, "type") == "mark_read" {
                muted.insert(m.id);
            }
        }
        Err(e) => tracing::error!("inboxRules: action {} failed for msg {}: {e}", s(action, "type"), m.id),
    }
}

#[allow(clippy::too_many_arguments)]
async fn flush(
    engine: &Arc<Engine>,
    account: &Account,
    m: &mut RuleMessage,
    deferred: &mut Vec<(Value, String)>,
    removed: &mut HashSet<Uuid>,
    blocked: &HashSet<Uuid>,
    muted: &mut HashSet<Uuid>,
    cache: &mut ResolverCache,
) {
    while !deferred.is_empty() {
        let (action, rule_id) = deferred.remove(0);
        if removed.contains(&m.id) || blocked.contains(&m.id) {
            continue;
        }
        execute(engine, account, m, &action, &rule_id, true, removed, muted, cache).await;
    }
}

/// Engine hook: rules over newly arrived messages → (remaining, muted ids).
pub async fn apply_inbox_rules(
    engine: &Arc<Engine>,
    account: &Account,
    messages: Vec<NewMessage>,
) -> EResult<(Vec<NewMessage>, HashSet<Uuid>)> {
    let mut rm: Vec<RuleMessage> = messages.iter().map(RuleMessage::from_new).collect();
    let (removed, muted) = run_rules(engine, account, &mut rm).await?;
    let read: HashMap<Uuid, bool> = rm.iter().map(|m| (m.id, m.is_read)).collect();
    Ok((
        messages
            .into_iter()
            .filter(|m| !removed.contains(&m.id))
            .map(|mut m| {
                m.parsed.is_read = read.get(&m.id).copied().unwrap_or(m.parsed.is_read);
                m
            })
            .collect(),
        muted,
    ))
}

/// `applyBlockList`: senders on the user's block list go to Trash (or are expunged there).
pub async fn apply_block_list(
    engine: &Arc<Engine>,
    account: &Account,
    messages: Vec<NewMessage>,
) -> EResult<Vec<NewMessage>> {
    if messages.is_empty() {
        return Ok(messages);
    }
    let blocked: HashSet<String> = match engine
        .db
        .rows("SELECT email_address FROM block_list WHERE user_id = $1", params![account.user_id()])
        .await
    {
        Ok(rows) => rows.iter().map(|r| r.get::<_, String>("email_address").to_lowercase()).collect(),
        Err(e) => {
            tracing::error!("blockList: failed to load: {e}");
            return Ok(messages);
        }
    };
    if blocked.is_empty() {
        return Ok(messages);
    }
    let mappings = account.0.get("folder_mappings");
    let (trash, all) = tokio::join!(
        resolve_trash_folder(&engine.db, account.id(), mappings),
        resolve_all_trash_paths(&engine.db, account.id(), mappings)
    );
    let (trash, all) = match (trash, all) {
        (Ok(t), Ok(a)) => (t, a),
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!("blockList: failed to resolve trash folders: {e}");
            return Ok(messages);
        }
    };
    let mut remaining = Vec::new();
    for msg in messages {
        if !blocked.contains(&msg.parsed.from_email.to_lowercase()) {
            remaining.push(msg);
            continue;
        }
        let mut rm = RuleMessage::from_new(&msg);
        let r: EResult<bool> = async {
            match delete_strategy(&msg.folder, trash.as_deref(), Some(&all)) {
                DeleteStrategy::Move(dest) => Ok(move_one(engine, account, &mut rm, &dest, false).await.is_ok()),
                DeleteStrategy::Expunge => {
                    engine.set_flag(account, rm.uid, &rm.folder, "\\Deleted", true).await?;
                    engine.db.exec("UPDATE messages SET is_deleted = true WHERE id = $1", params![rm.id]).await?;
                    adjust_folder_counts(&engine.db, account.id(), &rm.folder, -1, if rm.is_read { 0 } else { -1 });
                    Ok(true)
                }
                DeleteStrategy::NoTrash => Ok(false),
            }
        }
        .await;
        match r {
            Ok(true) => {}
            Ok(false) => remaining.push(msg),
            Err(e) => {
                tracing::error!("blockList: failed to move msg {}: {e}", msg.id);
                remaining.push(msg);
            }
        }
    }
    Ok(remaining)
}

// ── forwarder ───────────────────────────────────────────────────────────────────────────

const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

fn escape_html(v: &str) -> String {
    v.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn format_address(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => {
            let email = o.get("address").or_else(|| o.get("email")).and_then(Value::as_str).unwrap_or("");
            match o.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
                Some(n) => format!("{n} <{email}>"),
                None => email.to_owned(),
            }
        }
        _ => String::new(),
    }
}

fn parse_list(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) if !s.trim().is_empty() => serde_json::from_str::<Vec<Value>>(s).unwrap_or_default(),
        _ => vec![],
    }
}

/// `htmlToPlainText` (the forwarder's lightweight variant).
pub fn forward_plain_text(html: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;
    static BR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>").expect("re"));
    static BLOCK_END: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)</(?:blockquote|div|h[1-6]|li|p|pre|tr)\s*>").expect("re"));
    static LI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<li(?:\s[^>]*)?>").expect("re"));
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("re"));
    static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)&#x([0-9a-f]+);").expect("re"));
    static DEC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#([0-9]+);").expect("re"));
    static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)&([a-z]+);").expect("re"));
    static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\S\r\n]+").expect("re"));
    static NL_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" *\n *").expect("re"));
    static MANY_NL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").expect("re"));
    let cp = |n: Option<u32>| {
        n.filter(|c| *c <= 0x10FFFF).and_then(char::from_u32).map_or(" ".to_owned(), |c| c.to_string())
    };
    let t = BR.replace_all(html, "\n");
    let t = BLOCK_END.replace_all(&t, "\n");
    let t = LI.replace_all(&t, "- ");
    let t = TAG.replace_all(&t, "");
    let t = HEX.replace_all(&t, |c: &regex::Captures<'_>| cp(u32::from_str_radix(&c[1], 16).ok()));
    let t = DEC.replace_all(&t, |c: &regex::Captures<'_>| cp(c[1].parse().ok()));
    let t = NAMED.replace_all(&t, |c: &regex::Captures<'_>| {
        match c[1].to_lowercase().as_str() {
            "amp" => "&",
            "apos" => "'",
            "gt" => ">",
            "lt" => "<",
            "nbsp" => " ",
            "quot" => "\"",
            _ => " ",
        }
        .to_owned()
    });
    let t = SPACES.replace_all(&t, " ");
    let t = NL_SPACES.replace_all(&t, "\n");
    MANY_NL.replace_all(&t, "\n\n").trim().to_owned()
}

/// `buildForwardMessage`.
pub fn build_forward(
    row: &Map<String, Value>,
    account: &Account,
    recipient: &str,
    text: Option<&str>,
    html: Option<&str>,
    attachments: Vec<Part>,
) -> Compose {
    let st = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let from = format_address(&serde_json::json!({ "name": st("from_name"), "address": st("from_email") }));
    let date = row
        .get("date")
        .and_then(Value::as_str)
        .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
        .map(|d| d.with_timezone(&chrono::Utc).format("%a, %d %b %Y %H:%M:%S GMT").to_string())
        .unwrap_or_default();
    let fmt_list = |k: &str| {
        parse_list(row.get(k)).iter().map(format_address).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ")
    };
    let headers: Vec<(&str, String)> = [
        ("From", from),
        ("Date", date),
        ("Subject", st("subject")),
        ("To", fmt_list("to_addresses")),
        ("Cc", fmt_list("cc_addresses")),
    ]
    .into_iter()
    .filter(|(_, v)| !v.is_empty())
    .collect();
    let header_text = std::iter::once("---------- Forwarded message ----------".to_owned())
        .chain(headers.iter().map(|(l, v)| format!("{l}: {v}")))
        .collect::<Vec<_>>()
        .join("\n");
    let header_html = format!(
        "<div>---------- Forwarded message ----------<br>{}</div><br>",
        headers
            .iter()
            .map(|(l, v)| format!("<strong>{}:</strong> {}<br>", escape_html(l), escape_html(v)))
            .collect::<String>()
    );
    let safe_html = html.filter(|h| !h.is_empty()).map(inboxora_mail::sanitize::sanitize_email);
    let plain = text
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| forward_plain_text(safe_html.as_deref().unwrap_or("")));
    let subject = st("subject");
    let lower = subject.to_lowercase();
    Compose {
        from: format!(
            "{} <{}>",
            account.str("sender_name").or(account.str("name")).unwrap_or(""),
            account.str("email_address").unwrap_or("")
        ),
        to: vec![recipient.to_owned()],
        subject: if lower.starts_with("fwd:") { subject } else { format!("Fwd: {subject}") },
        text: Some(format!("{header_text}\n\n{plain}")),
        html: safe_html.filter(|h| !h.is_empty()).map(|h| format!("{header_html}{h}")),
        attachments,
        ..Default::default()
    }
}

/// `forwardRuleMessage`: reserve (rule, message), forward with attachments, mark sent.
pub async fn forward_rule_message(
    engine: &Arc<Engine>,
    rule_id: &str,
    message_id: Uuid,
    account: &Account,
    recipient: &str,
) -> EResult<&'static str> {
    let db: &Db = &engine.db;
    let reserved = db
        .opt(
            "INSERT INTO inbox_rule_forwards (rule_id, message_id) VALUES ($1, $2) ON CONFLICT (rule_id, message_id) DO NOTHING RETURNING id",
            params![rule_id, message_id],
        )
        .await?;
    let Some(reserved) = reserved else {
        let existing = db
            .opt(
                "SELECT status FROM inbox_rule_forwards WHERE rule_id = $1 AND message_id = $2",
                params![rule_id, message_id],
            )
            .await?;
        if existing.and_then(|r| r.get::<_, Option<String>>("status")).as_deref() == Some("sent") {
            return Ok("duplicate");
        }
        return Err(EngineError("Forward delivery pending".into()));
    };
    let reservation: Uuid = reserved.get("id");
    let mut delivered = false;
    let r: EResult<&'static str> = async {
        let row = db
            .json_opt(
                "SELECT id, account_id, uid, folder, subject, from_name, from_email, to_addresses, cc_addresses, date, body_text, body_html, attachments \
                 FROM messages WHERE id = $1 AND account_id = $2",
                params![message_id, account.id()],
            )
            .await?
            .ok_or_else(|| EngineError("Forward source message not found".into()))?;
        let uid = row.get("uid").and_then(Value::as_i64).unwrap_or(0);
        let folder = row.get("folder").and_then(Value::as_str).unwrap_or("").to_owned();
        let mut text = row.get("body_text").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        let mut html = row.get("body_html").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        let mut fetched_parts: Vec<Value> = Vec::new();
        if text.is_none() && html.is_none() {
            let body = engine.fetch_message_body(account, uid, &folder).await?;
            text = body.text;
            html = body.html;
            fetched_parts = body.attachments.iter().map(|a| a.to_json()).collect();
        }
        let mut stored: Vec<Value> = Vec::new();
        let mut seen = HashSet::new();
        for c in parse_list(row.get("attachments")).into_iter().chain(fetched_parts) {
            let Some(part) = c.get("part").and_then(Value::as_str).map(str::to_owned) else { continue };
            if seen.insert(part) {
                stored.push(c);
            }
        }
        let known: f64 = stored.iter().map(|a| a.get("size").and_then(Value::as_f64).unwrap_or(0.0)).sum();
        if known > MAX_ATTACHMENT_BYTES as f64 {
            return Err(EngineError("Total attachment size exceeds 25 MB".into()));
        }
        let mut parts: Vec<Part> = Vec::new();
        if !stored.is_empty() {
            let req: Vec<(String, Option<String>)> =
                stored.iter().map(|a| (s(a, "part").to_owned(), a.get("encoding").and_then(Value::as_str).map(str::to_owned))).collect();
            let buffers = engine.fetch_multiple_attachments(account, uid, &folder, &req).await?;
            for a in &stored {
                let content = buffers.get(s(a, "part")).cloned().ok_or_else(|| EngineError("Forward attachment unavailable".into()))?;
                parts.push(Part {
                    filename: Some(a.get("filename").and_then(Value::as_str).filter(|f| !f.is_empty()).unwrap_or("attachment").to_owned()),
                    content,
                    content_type: a.get("type").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or("application/octet-stream").to_owned(),
                    cid: None,
                });
            }
        }
        let safe = html.as_deref().map(inboxora_mail::sanitize::sanitize_email);
        let (embedded_html, inline) = match safe {
            Some(h) => {
                let (h, inl) = inboxora_mail::util::embed_inline_data_images(&h);
                (Some(h), inl)
            }
            None => (None, vec![]),
        };
        let mut all: Vec<Part> = inline
            .into_iter()
            .map(|i| Part { filename: Some(i.filename), content: i.content, content_type: i.content_type, cid: Some(i.cid) })
            .collect();
        all.extend(parts);
        if all.iter().map(|p| p.content.len()).sum::<usize>() > MAX_ATTACHMENT_BYTES {
            return Err(EngineError("Total attachment size exceeds 25 MB".into()));
        }
        let mail = build_forward(&row, account, recipient, text.as_deref(), embedded_html.as_deref(), all);
        let (transport, _) = account_transport(db, (*account.0).clone()).await.map_err(|(_, e)| EngineError(e))?;
        let built = mail.build().map_err(EngineError)?;
        transport
            .send_raw(&built.envelope_from, &built.recipients, &built.raw)
            .await
            .map_err(|_| EngineError("Forward delivery failed".into()))?;
        delivered = true;
        db.exec("UPDATE inbox_rule_forwards SET status = 'sent', sent_at = NOW() WHERE id = $1", params![reservation]).await?;
        Ok("sent")
    }
    .await;
    if r.is_err() && !delivered {
        let _ =
            db.exec("DELETE FROM inbox_rule_forwards WHERE id = $1 AND status = 'pending'", params![reservation]).await;
    }
    r
}
