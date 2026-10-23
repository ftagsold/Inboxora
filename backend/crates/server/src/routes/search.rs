//! `/api/search` (port of routes/search.ts).

use std::sync::LazyLock;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::response::Response;
use axum::routing::get;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Result, params};
use regex::Regex;
use serde_json::json;
use uuid::Uuid;

use crate::engine::search::FolderScope;
use crate::http::limit::Windows;
use crate::http::{Auth, Qs};
use crate::routes::contacts::{query_int, query_string};
use crate::routes::util::{err, fail, json, ok};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(search)).route("/contacts", get(suggest_contacts))
}

/// 20 searches per minute per user (shared by both endpoints, in-memory like Node).
static BUCKETS: LazyLock<Windows> = LazyLock::new(|| Windows::new(10_000));

fn limited(user_id: &str) -> Option<Response> {
    let (hits, left) = BUCKETS.hit(user_id, Duration::from_secs(60));
    (hits > 20).then(|| {
        let mut r = json(429, json!({ "error": "Too many search requests. Try again shortly." }));
        if let Ok(v) = HeaderValue::from_str(&left.as_secs_f64().ceil().to_string()) {
            r.headers_mut().insert("retry-after", v);
        }
        r
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub key: String,
    pub value: String,
    pub negate: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub value: String,
    pub negate: bool,
}

/// `parseSearchQuery`: operator filters (`from:`, `to:`, … with quotes and `-` negation) and
/// free-text terms.
pub fn parse_search_query(raw: &str) -> (Vec<Filter>, Vec<Term>) {
    static OP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)(-?)(?-u:\b)(from|to|subject|has|is|after|before|in):("([^"]*)"|(\S+))"#).expect("regex")
    });
    let mut filters = Vec::new();
    let remaining = OP.replace_all(raw, |c: &regex::Captures| {
        let key = c[2].to_lowercase();
        let v = c.get(4).or_else(|| c.get(5)).map_or("", |m| m.as_str());
        let v = inboxora_mail::text::js_trim(&v.to_lowercase()).to_owned();
        if !v.is_empty() {
            filters.push(Filter { key, value: v, negate: &c[1] == "-" });
        }
        " ".to_owned()
    });
    let remaining = inboxora_mail::text::js_trim(&remaining).to_owned();
    let mut terms = Vec::new();
    for word in remaining.split(inboxora_mail::text::is_js_space) {
        let mut w = inboxora_mail::text::js_trim(word).to_owned();
        if w.is_empty() || w == "-" {
            continue;
        }
        let mut negate = false;
        if w.starts_with('-') && w.chars().count() > 1 {
            negate = true;
            w = w[1..].to_owned();
        }
        terms.push(Term { value: w, negate });
    }
    (filters, terms)
}

/// `new Date(value)` for the formats people type in `after:`/`before:` (the process runs in UTC).
pub fn js_date_parse(v: &str) -> Option<DateTime<Utc>> {
    let v = v.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(v) {
        return Some(t.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y/%m/%d %H:%M:%S", "%Y/%m/%d %H:%M"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(v, fmt) {
            return Some(t.and_utc());
        }
    }
    for fmt in ["%Y-%m-%d", "%Y/%m/%d", "%m/%d/%Y", "%Y-%m", "%b %d %Y", "%B %d %Y", "%d %b %Y", "%d %B %Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(v, fmt) {
            return Some(d.and_hms_opt(0, 0, 0)?.and_utc());
        }
    }
    if v.len() == 4 && v.bytes().all(|b| b.is_ascii_digit()) {
        return NaiveDate::from_ymd_opt(v.parse().ok()?, 1, 1)?.and_hms_opt(0, 0, 0).map(|t| t.and_utc());
    }
    if let Some(month) = v.get(..7).filter(|_| v.len() == 7)
        && let Ok(d) = NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d")
    {
        return d.and_hms_opt(0, 0, 0).map(|t| t.and_utc());
    }
    None
}

fn negate_cond(sql: &str) -> String {
    format!("NOT COALESCE(({sql}), false)")
}

const TRASH_EXCLUSION: &str = "NOT EXISTS (SELECT 1 FROM folders f WHERE f.account_id = m.account_id AND f.path = m.folder \
    AND (f.special_use = '\\Trash' OR lower(f.name) LIKE '%trash%' OR lower(f.name) LIKE '%deleted%'))";

/// Newest server hits kept per folder before the result page is cut.
const HITS_PER_FOLDER: usize = 1000;

/// IMAP `SEARCH` keys for the text parts of a query (sender, recipients, subject, free text).
pub fn imap_criteria(filters: &[Filter], terms: &[Term]) -> Vec<String> {
    use inboxora_imap::quote;
    let key = |negate: bool, k: String| if negate { format!("NOT ({k})") } else { k };
    let mut out = Vec::new();
    for f in filters {
        let k = match f.key.as_str() {
            "from" => format!("FROM {}", quote(&f.value)),
            "subject" => format!("SUBJECT {}", quote(&f.value)),
            "to" => format!("OR TO {q} CC {q}", q = quote(&f.value)),
            _ => continue,
        };
        out.push(key(f.negate, k));
    }
    for t in terms.iter().take(10) {
        if inboxora_mail::text::js_len(&t.value) < 2 {
            continue;
        }
        out.push(key(t.negate, format!("TEXT {}", quote(&t.value))));
    }
    out
}

/// `resolveAccountScope`: one owned account, or every account in the unified inbox.
pub fn resolve_account_scope(
    accounts: &[(String, Option<bool>)],
    requested: Option<&str>,
) -> (Vec<String>, Option<String>) {
    if let Some(r) = requested.filter(|r| !r.is_empty())
        && accounts.iter().any(|(id, _)| id == r)
    {
        return (vec![r.to_owned()], Some(r.to_owned()));
    }
    (accounts.iter().filter(|(_, inc)| *inc != Some(false)).map(|(id, _)| id.clone()).collect(), None)
}

async fn search(State(s): State<AppState>, auth: Auth, q: Qs) -> Result<Response> {
    if let Some(r) = limited(&auth.user_id) {
        return Ok(r);
    }
    let raw = query_string(&q, "q").unwrap_or_default();
    let account_id = query_string(&q, "accountId");
    let limit = query_int(&q, "limit", 50);
    let offset = query_int(&q, "offset", 0);
    let trimmed = inboxora_mail::text::js_trim(&raw).to_owned();
    if trimmed.is_empty() {
        return Ok(ok(json!({ "messages": [] })));
    }
    if inboxora_mail::text::js_len(&trimmed) > 500 {
        return Ok(err(400, "Search query too long"));
    }
    let accounts: Vec<(String, Option<bool>)> =
        s.db.rows(
            "SELECT id, include_in_unified_inbox FROM email_accounts WHERE user_id = $1 AND enabled = true",
            params![&auth.user_id],
        )
        .await?
        .iter()
        .map(|r| (r.get::<_, uuid::Uuid>(0).to_string(), r.get::<_, Option<bool>>(1)))
        .collect();
    let (target_ids, _) = resolve_account_scope(&accounts, account_id.as_deref());
    if target_ids.is_empty() {
        return Ok(ok(json!({ "messages": [] })));
    }
    let cap = limit.clamp(1, 200);
    let (filters, terms) = parse_search_query(&trimmed);
    let mut conditions: Vec<String> = Vec::new();
    let mut p: Vec<Val> = vec![Val::from(target_ids.clone())];
    let next = |p: &Vec<Val>| p.len() + 1;
    for f in &filters {
        let cond = match f.key.as_str() {
            "has" if f.value == "attachment" || f.value == "attachments" => Some("m.has_attachments = true".into()),
            "is" => match f.value.as_str() {
                "unread" => Some("m.is_read = false".into()),
                "read" => Some("m.is_read = true".into()),
                "starred" => Some("m.is_starred = true".into()),
                _ => None,
            },
            "after" | "before" => js_date_parse(&f.value).map(|d| {
                let n = next(&p);
                p.push(Val::Time(d));
                format!("m.date {} ${n}", if f.key == "after" { ">=" } else { "<" })
            }),
            _ => None,
        };
        if let Some(c) = cond {
            conditions.push(if f.negate { negate_cond(&c) } else { c });
        }
    }
    let criteria = imap_criteria(&filters, &terms);
    if conditions.is_empty() && criteria.is_empty() {
        return Ok(ok(json!({ "messages": [], "query": raw })));
    }
    // `in:` wins over the folder param; `in:all` searches every folder.
    let mut scope: Option<FolderScope> = None;
    for f in filters.iter().filter(|f| f.key == "in") {
        scope = Some(if f.value == "all" { FolderScope::AllButTrash } else { FolderScope::Named(f.value.clone()) });
    }
    let scope = scope.unwrap_or_else(|| {
        match query_string(&q, "folder").map(|f| f.trim().to_owned()).filter(|f| !f.is_empty()) {
            Some(f) => FolderScope::Exact(f),
            None => FolderScope::AllButTrash,
        }
    });
    if criteria.is_empty() {
        match &scope {
            FolderScope::Named(folder) => {
                let n = next(&p);
                p.push(Val::from(folder.clone()));
                p.push(Val::from(format!("%/{folder}")));
                conditions.push(format!("(m.folder ILIKE ${n} OR m.folder ILIKE ${})", n + 1));
            }
            FolderScope::Exact(folder) => {
                let n = next(&p);
                p.push(Val::from(folder.clone()));
                conditions.push(format!("m.folder = ${n}"));
            }
            FolderScope::AllButTrash => conditions.push(TRASH_EXCLUSION.into()),
        }
    } else {
        let ids = s.mail.search_ids(&target_ids, &scope, &criteria.join(" "), HITS_PER_FOLDER).await;
        if ids.is_empty() {
            return Ok(ok(json!({ "messages": [], "query": raw })));
        }
        let n = next(&p);
        p.push(Val::from(ids.iter().map(Uuid::to_string).collect::<Vec<_>>()));
        conditions.push(format!("m.id = ANY(${n}::uuid[])"));
    }
    let n = next(&p);
    p.push(Val::Int(cap));
    p.push(Val::Int(offset.max(0)));
    let sql = format!(
        "SELECT m.id, m.uid, m.folder, m.date, m.is_read, m.is_starred, \
         m.has_attachments, m.account_id, a.name as account_name, a.email_address as account_email, a.color as account_color \
         FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.account_id = ANY($1) AND m.is_deleted = false \
         AND {} ORDER BY m.date DESC LIMIT ${n} OFFSET ${}",
        conditions.join(" AND "),
        n + 1
    );
    let mut rows = s.db.json_rows(&sql, &p).await.map_err(fail("Search error", "Search failed"))?;
    s.mail.hydrate_values(&mut rows).await;
    Ok(ok(json!({ "messages": rows, "query": raw })))
}

async fn suggest_contacts(State(s): State<AppState>, auth: Auth, q: Qs) -> Result<Response> {
    if let Some(r) = limited(&auth.user_id) {
        return Ok(r);
    }
    let raw = query_string(&q, "q").unwrap_or_default();
    let trimmed = inboxora_mail::text::js_trim(&raw).to_owned();
    let len = inboxora_mail::text::js_len(&trimmed);
    if trimmed.is_empty() || len < 2 {
        return Ok(ok(json!({ "contacts": [] })));
    }
    if len > 100 {
        return Ok(err(400, "Query too long"));
    }
    // Senders of incoming mail are learned into auto contacts as it arrives, so the contacts
    // table is the whole suggestion source.
    let (relation, live) = crate::services::remote_dav::contacts_relation(&s.db, &auth.user_id, 3).await;
    let rows = s
        .db
        .json_rows(
            &format!(
                "WITH known AS (SELECT primary_email AS email, display_name AS name, send_count, last_sent FROM {relation} c \
                   WHERE user_id = $1 AND is_auto = false AND primary_email IS NOT NULL AND (display_name ILIKE $2 OR primary_email ILIKE $2)), \
                 auto AS (SELECT primary_email AS email, display_name AS name, 0 AS send_count, last_sent FROM contacts \
                   WHERE user_id = $1 AND is_auto = true AND primary_email IS NOT NULL AND (display_name ILIKE $2 OR primary_email ILIKE $2) \
                   AND lower(primary_email) NOT IN (SELECT lower(email) FROM known)) \
                 SELECT email, name FROM (SELECT email, name, 1 AS priority, send_count, last_sent FROM known UNION ALL \
                   SELECT email, name, 2 AS priority, 0, last_sent FROM auto) combined \
                 ORDER BY priority, send_count DESC, last_sent DESC NULLS LAST LIMIT 10"
            ),
            &[Val::from(&auth.user_id), Val::from(format!("%{trimmed}%")), live],
        )
        .await
        .map_err(fail("Contact suggest error", "Failed to fetch contacts"))?;
    Ok(ok(json!({ "contacts": rows })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operators_like_node() {
        let (f, t) = parse_search_query(r#"from:"John Smith" -to:bob invoice -draft has:attachment in:Sent x - "#);
        assert_eq!(f.len(), 4);
        assert_eq!(f[0], Filter { key: "from".into(), value: "john smith".into(), negate: false });
        assert_eq!(f[1], Filter { key: "to".into(), value: "bob".into(), negate: true });
        assert_eq!(
            t,
            vec![
                Term { value: "invoice".into(), negate: false },
                Term { value: "draft".into(), negate: true },
                Term { value: "x".into(), negate: false }
            ]
        );
        let (f, _) = parse_search_query("xfrom:a");
        assert!(f.is_empty());
    }
}
