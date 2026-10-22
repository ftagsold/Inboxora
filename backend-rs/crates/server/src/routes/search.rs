//! `/api/search` (port of routes/search.ts).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::response::Response;
use axum::routing::get;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Result, params};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};

use crate::http::{Auth, Qs};
use crate::routes::contacts::{query_int, query_string};
use crate::routes::util::{err, fail, json, ok};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(search)).route("/contacts", get(suggest_contacts))
}

/// 20 searches per minute per user (shared by both endpoints, in-memory like Node).
static BUCKETS: LazyLock<Mutex<HashMap<String, (u32, Instant)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn limited(user_id: &str) -> Option<Response> {
    let now = Instant::now();
    let mut b = BUCKETS.lock();
    if b.len() > 10_000 {
        b.retain(|_, (_, reset)| *reset > now);
    }
    match b.get_mut(user_id) {
        Some((count, reset)) if now <= *reset => {
            if *count >= 20 {
                let secs = (*reset - now).as_secs_f64().ceil() as u64;
                let mut r = json(429, json!({ "error": "Too many search requests. Try again shortly." }));
                if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                    r.headers_mut().insert("retry-after", v);
                }
                return Some(r);
            }
            *count += 1;
            None
        }
        _ => {
            b.insert(user_id.to_owned(), (1, now + Duration::from_secs(60)));
            None
        }
    }
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

const FTS_BODY_CHAR_CAP: usize = 600_000;

fn negate_cond(sql: &str) -> String {
    format!("NOT COALESCE(({sql}), false)")
}

fn free_text_condition(like: usize, fts: usize) -> String {
    format!(
        "(m.from_name ILIKE ${like} OR m.from_email ILIKE ${like} OR m.subject ILIKE ${like} \
         OR m.search_vector @@ plainto_tsquery('english', ${fts}) \
         OR to_tsvector('english', LEFT(coalesce(m.body_text,''), {FTS_BODY_CHAR_CAP})) @@ plainto_tsquery('english', ${fts}))"
    )
}

const TRASH_EXCLUSION: &str = "NOT EXISTS (SELECT 1 FROM folders f WHERE f.account_id = m.account_id AND f.path = m.folder \
    AND (f.special_use = '\\Trash' OR lower(f.name) LIKE '%trash%' OR lower(f.name) LIKE '%deleted%'))";

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
    let mut p: Vec<Val> = vec![Val::from(target_ids)];
    let next = |p: &Vec<Val>| p.len() + 1;
    for f in &filters {
        let cond = match f.key.as_str() {
            "in" => None,
            "from" => {
                let n = next(&p);
                p.push(Val::from(format!("%{}%", f.value)));
                Some(format!("(m.from_email ILIKE ${n} OR m.from_name ILIKE ${n})"))
            }
            "subject" => {
                let n = next(&p);
                p.push(Val::from(format!("%{}%", f.value)));
                Some(format!("m.subject ILIKE ${n}"))
            }
            "to" => {
                let n = next(&p);
                p.push(Val::from(format!("%{}%", f.value)));
                Some(format!("(m.to_addresses::text ILIKE ${n} OR m.cc_addresses::text ILIKE ${n})"))
            }
            "has" if f.value == "attachment" || f.value == "attachments" => Some("m.has_attachments = true".into()),
            "is" => match f.value.as_str() {
                "unread" => Some("m.is_read = false".into()),
                "read" => Some("m.is_read = true".into()),
                "starred" => Some("m.is_starred = true".into()),
                _ => None,
            },
            // Node built `m.date >= 3` (missing `$`), so these failed with 500; fixed here.
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
    for t in terms.iter().take(10) {
        if inboxora_mail::text::js_len(&t.value) < 2 {
            continue;
        }
        let like = next(&p);
        p.push(Val::from(format!("%{}%", t.value)));
        let fts = next(&p);
        p.push(Val::from(t.value.clone()));
        let c = free_text_condition(like, fts);
        conditions.push(if t.negate { negate_cond(&c) } else { c });
    }
    if conditions.is_empty() {
        return Ok(ok(json!({ "messages": [], "query": raw })));
    }
    // `in:` wins over the folder param; `in:all` searches every folder.
    let mut scope: Option<Option<String>> = None;
    let mut fuzzy = false;
    for f in filters.iter().filter(|f| f.key == "in") {
        if f.value == "all" {
            scope = Some(None);
        } else {
            scope = Some(Some(f.value.clone()));
            fuzzy = true;
        }
    }
    let scope = match scope {
        Some(s) => s,
        None => {
            fuzzy = false;
            query_string(&q, "folder").map(|f| f.trim().to_owned()).filter(|f| !f.is_empty())
        }
    };
    match scope {
        Some(folder) if fuzzy => {
            let n = next(&p);
            p.push(Val::from(folder.clone()));
            p.push(Val::from(format!("%/{folder}")));
            conditions.push(format!("(m.folder ILIKE ${n} OR m.folder ILIKE ${})", n + 1));
        }
        Some(folder) => {
            let n = next(&p);
            p.push(Val::from(folder));
            conditions.push(format!("m.folder = ${n}"));
        }
        None => conditions.push(TRASH_EXCLUSION.into()),
    }
    let n = next(&p);
    p.push(Val::Int(cap));
    p.push(Val::Int(offset.max(0)));
    let sql = format!(
        "SELECT m.id, m.uid, m.folder, m.subject, m.from_name, m.from_email, m.date, m.snippet, m.is_read, m.is_starred, \
         m.has_attachments, m.account_id, a.name as account_name, a.email_address as account_email, a.color as account_color \
         FROM messages m JOIN email_accounts a ON m.account_id = a.id WHERE m.account_id = ANY($1) AND m.is_deleted = false \
         AND {} ORDER BY m.date DESC LIMIT ${n} OFFSET ${}",
        conditions.join(" AND "),
        n + 1
    );
    let rows = s.db.json_rows(&sql, &p).await.map_err(fail("Search error", "Search failed"))?;
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
    let accounts: Vec<String> =
        s.db.rows("SELECT id FROM email_accounts WHERE user_id = $1 AND enabled = true", params![&auth.user_id])
            .await?
            .iter()
            .map(|r| r.get::<_, uuid::Uuid>(0).to_string())
            .collect();
    if accounts.is_empty() {
        return Ok(ok(json!({ "contacts": [] })));
    }
    let rows = s
        .db
        .json_rows(
            "WITH known AS (SELECT primary_email AS email, display_name AS name, send_count, last_sent FROM contacts \
               WHERE user_id = $1 AND is_auto = false AND primary_email IS NOT NULL AND (display_name ILIKE $2 OR primary_email ILIKE $2)), \
             auto AS (SELECT primary_email AS email, display_name AS name, 0 AS send_count, last_sent FROM contacts \
               WHERE user_id = $1 AND is_auto = true AND primary_email IS NOT NULL AND (display_name ILIKE $2 OR primary_email ILIKE $2) \
               AND lower(primary_email) NOT IN (SELECT lower(email) FROM known)), \
             inbound AS (SELECT email, name, send_count, last_sent FROM (SELECT DISTINCT ON (from_email) from_email AS email, \
               from_name AS name, 0 AS send_count, date AS last_sent, is_bulk FROM messages WHERE account_id = ANY($3) \
               AND is_deleted = false AND from_email IS NOT NULL AND from_email != '' AND (from_email ILIKE $2 OR from_name ILIKE $2) \
               AND lower(from_email) NOT IN (SELECT lower(primary_email) FROM contacts WHERE user_id = $1 AND primary_email IS NOT NULL) \
               AND from_email !~* '^(noreply|no-reply|donotreply|mailer-daemon|notifications?|bounce[^@]*)@' \
               ORDER BY from_email, date DESC) latest WHERE is_bulk IS NOT TRUE) \
             SELECT email, name FROM (SELECT email, name, 1 AS priority, send_count, last_sent FROM known UNION ALL \
               SELECT email, name, 2 AS priority, 0, last_sent FROM auto UNION ALL SELECT email, name, 3 AS priority, 0, last_sent FROM inbound) combined \
             ORDER BY priority, send_count DESC, last_sent DESC NULLS LAST LIMIT 10",
            params![&auth.user_id, format!("%{trimmed}%"), accounts],
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
