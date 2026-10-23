//! Secret calendar feeds (port of `routes/calendarFeed.ts` and `services/calendarFeed.ts`):
//! an anonymous `GET /calendar/feeds/:token(.ics)` protected by a 256-bit bearer token, plus
//! feed management under `/api/calendar/feeds`.

use std::time::Duration;

use axum::Router;
use axum::body::Body as AxumBody;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, SecondsFormat, Utc};
use inboxora_core::pg::Q;
use inboxora_core::{Result, crypto, params};
use inboxora_mail::conversation::js_date_to_string;
use inboxora_mail::util::description_content_lines;
use serde_json::{Value, json};
use uuid::Uuid;

use super::dav::{decode_uri_component, tagged};
use super::util::{empty, err, json, ok};
use crate::http::{Auth, Body, ClientIp};
use crate::state::AppState;

const PUBLIC_LIMIT: u64 = 30;
const PUBLIC_WINDOW: Duration = Duration::from_secs(60);
const TOKEN_BYTES: usize = 32;

/// The anonymous feed endpoint (mounted at the root, outside `/api`).
pub fn public_router() -> Router<AppState> {
    Router::new().route("/calendar/feeds/{token}", get(feed)).route("/calendar/feeds/{token}/", get(feed))
}

/// Feed management, nested in the authenticated `/api/calendar` group.
pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/feeds", get(list_feeds).post(create_feed))
        .route("/feeds/{feed_id}", axum::routing::delete(revoke_feed))
        .route("/feeds/{feed_id}/rotate", post(rotate_feed))
}

/// `issueCalendarFeedToken`: (token, sha256 hex).
fn issue_token() -> (String, String) {
    let token = crypto::random_base64url(TOKEN_BYTES);
    let hash = crypto::sha256_hex(&token);
    (token, hash)
}

/// `hashCalendarFeedToken`: only well-formed 43-character base64url tokens hash.
fn hash_token(token: &str) -> Option<String> {
    (token.len() == 43 && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        .then(|| crypto::sha256_hex(token))
}

fn not_found_text() -> Response {
    let mut res = Response::new(AxumBody::from("Not found"));
    *res.status_mut() = StatusCode::NOT_FOUND;
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    res
}

/// The `:token` of `['/calendar/feeds/:token.ics', '/calendar/feeds/:token']`.
fn token_param(segment: &str) -> &str {
    let n = segment.len();
    if n > 4 && segment.is_char_boundary(n - 4) && segment[n - 4..].eq_ignore_ascii_case(".ics") {
        &segment[..n - 4]
    } else {
        segment
    }
}

fn escape_text(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\\n")
        .replace(';', "\\;")
        .replace(',', "\\,")
}

/// Fold a content line at 75 UTF-8 octets without splitting characters.
fn fold(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + line.len() / 60 * 3);
    let mut current_len = 0;
    for c in line.chars() {
        if current_len + c.len_utf8() > 75 && current_len > 0 {
            out.push_str("\r\n ");
            current_len = 0;
        }
        out.push(c);
        current_len += c.len_utf8();
    }
    out
}

fn date_value(t: &DateTime<Utc>, all_day: bool) -> String {
    let iso = t.to_rfc3339_opts(SecondsFormat::Millis, true);
    if all_day {
        return iso[..10].replace('-', "");
    }
    let compact = iso.replace(['-', ':'], "");
    match compact.strip_suffix('Z').and_then(|s| s.rsplit_once('.')) {
        Some((head, ms)) if ms.len() == 3 && ms.bytes().all(|b| b.is_ascii_digit()) => format!("{head}Z"),
        _ => compact,
    }
}

struct FeedEvent {
    calendar_id: Uuid,
    id: Uuid,
    uid: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    url: Option<String>,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    all_day: bool,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

/// `serializeCalendarFeed(events, calendarName)`.
fn serialize(events: &[FeedEvent], name: &str) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".into(),
        "PRODID:-//Inboxora//Calendar Feed//EN".into(),
        format!("X-WR-CALNAME:{}", escape_text(name)),
    ];
    for e in events {
        let all_day = e.all_day;
        let date_param = if all_day { ";VALUE=DATE" } else { "" };
        let uid = e.uid.clone().filter(|u| !u.is_empty()).unwrap_or_else(|| e.id.to_string());
        let stamp = e.updated_at.or(e.created_at).unwrap_or(e.starts_at);
        lines.push("BEGIN:VEVENT".into());
        lines.push(format!("UID:{}", escape_text(&uid)));
        lines.push(format!("DTSTAMP:{}", date_value(&stamp, false)));
        lines.push(format!("DTSTART{date_param}:{}", date_value(&e.starts_at, all_day)));
        lines.push(format!("DTEND{date_param}:{}", date_value(&e.ends_at, all_day)));
        if let Some(s) = e.summary.as_deref().filter(|s| !s.is_empty()) {
            lines.push(format!("SUMMARY:{}", escape_text(s)));
        }
        lines.extend(description_content_lines(e.description.as_deref(), escape_text));
        if let Some(l) = e.location.as_deref().filter(|s| !s.is_empty()) {
            lines.push(format!("LOCATION:{}", escape_text(l)));
        }
        if let Some(u) = e.url.as_deref().filter(|s| !s.is_empty()) {
            lines.push(format!("URL:{}", escape_text(u)));
        }
        lines.push("END:VEVENT".into());
    }
    lines.push("END:VCALENDAR".into());
    lines.push(String::new());
    lines.iter().map(|l| fold(l)).collect::<Vec<_>>().join("\r\n")
}

/// JS default sort order (UTF-16 code units).
fn js_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

async fn feed(State(s): State<AppState>, ClientIp(ip): ClientIp, req: Request) -> Response {
    let method = req.method().clone();
    let pattern = "/calendar/feeds/:token.ics";
    let segment = req.uri().path().trim_end_matches('/').rsplit('/').next().unwrap_or("").to_owned();
    let Some(token) = decode_uri_component(token_param(&segment)) else {
        let e = inboxora_core::Error::Other(anyhow::anyhow!("Failed to decode param '{segment}'"));
        return tagged(e.into_response(), &method, "unmatched");
    };
    let (limited, reset_ms) =
        crate::services::auth::rate_consume(&s.redis, &format!("calendar-feed:{ip}"), PUBLIC_LIMIT, PUBLIC_WINDOW)
            .await;
    if limited {
        let mut res = not_found_text();
        res.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(reset_ms.div_ceil(1000)));
        return tagged(res, &method, pattern);
    }
    let res = match serve_feed(&s, &token, req.headers()).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    tagged(res, &method, pattern)
}

async fn serve_feed(s: &AppState, token: &str, headers: &axum::http::HeaderMap) -> Result<Response> {
    let Some(hash) = hash_token(token) else { return Ok(not_found_text()) };
    let rows = s
        .db
        .rows(
            "SELECT f.calendar_ids, f.owner_user_id, c.id AS calendar_id, c.name AS calendar_name, e.id, e.uid, e.summary, e.description, \
             e.location, e.url, e.starts_at, e.ends_at, e.all_day, e.created_at, e.updated_at \
             FROM calendar_secret_feeds f \
             JOIN calendars c ON c.id = ANY(f.calendar_ids) AND c.owner_user_id = f.owner_user_id \
             LEFT JOIN calendar_events e ON e.calendar_id = c.id AND e.user_id = f.owner_user_id \
             WHERE f.token_hash = $1 AND f.revoked_at IS NULL \
             ORDER BY e.starts_at ASC NULLS LAST, c.id ASC, e.id ASC",
            params![hash],
        )
        .await?;
    if rows.is_empty() {
        return Ok(not_found_text());
    }
    let mut names: Vec<String> =
        rows.iter().filter_map(|r| r.get::<_, Option<String>>("calendar_name")).filter(|n| !n.is_empty()).collect();
    names.sort_by(|a, b| js_cmp(a, b));
    names.dedup();
    let mut events: Vec<FeedEvent> = rows
        .iter()
        .filter_map(|r| {
            Some(FeedEvent {
                id: r.get::<_, Option<Uuid>>("id")?,
                calendar_id: r.get("calendar_id"),
                uid: r.get("uid"),
                summary: r.get("summary"),
                description: r.get("description"),
                location: r.get("location"),
                url: r.get("url"),
                starts_at: r.get::<_, Option<DateTime<Utc>>>("starts_at")?,
                ends_at: r.get::<_, Option<DateTime<Utc>>>("ends_at")?,
                all_day: r.get::<_, Option<bool>>("all_day").unwrap_or(false),
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
            })
        })
        .collect();
    // Read-only remote calendars are not stored: add their events from the live snapshot.
    if let Some(owner) = rows.first().map(|r| r.get::<_, Uuid>("owner_user_id").to_string()) {
        let mut ids: Vec<String> = rows.iter().map(|r| r.get::<_, Uuid>("calendar_id").to_string()).collect();
        ids.sort();
        ids.dedup();
        let time = |m: &serde_json::Map<String, serde_json::Value>, k: &str| {
            m.get(k)
                .and_then(serde_json::Value::as_str)
                .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
                .map(|d| d.with_timezone(&Utc))
        };
        let text = |m: &serde_json::Map<String, serde_json::Value>, k: &str| {
            m.get(k).and_then(serde_json::Value::as_str).map(str::to_owned)
        };
        for m in crate::services::remote_dav::event_rows(&s.db, &owner, Some(&ids)).await {
            let (Some(id), Some(calendar_id), Some(starts_at), Some(ends_at)) = (
                text(&m, "id").and_then(|v| v.parse().ok()),
                text(&m, "calendar_id").and_then(|v| v.parse().ok()),
                time(&m, "starts_at"),
                time(&m, "ends_at"),
            ) else {
                continue;
            };
            events.push(FeedEvent {
                calendar_id,
                id,
                uid: text(&m, "uid"),
                summary: text(&m, "summary"),
                description: text(&m, "description"),
                location: text(&m, "location"),
                url: text(&m, "url"),
                starts_at,
                ends_at,
                all_day: m.get("all_day").and_then(serde_json::Value::as_bool).unwrap_or(false),
                created_at: None,
                updated_at: None,
            });
        }
    }
    // Node re-sorts by `String(value)`, which for dates is `Date#toString()` (weekday first).
    // Reproduced so both backends serve byte-identical feeds (and the same ETags).
    events.sort_by(|a, b| {
        js_cmp(&js_date_to_string(&a.starts_at), &js_date_to_string(&b.starts_at))
            .then_with(|| a.calendar_id.to_string().cmp(&b.calendar_id.to_string()))
            .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
    });
    let name = if names.is_empty() { "Inboxora".to_owned() } else { names.join(", ") };
    let body = serialize(&events, &name);
    let etag = format!("\"{}\"", crypto::sha256_hex(&body));
    let validators: Vec<String> = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|v| v.trim().to_owned())
        .collect();
    let fresh = validators.iter().any(|v| v == "*" || *v == etag || *v == format!("W/{etag}"));
    let mut res = if fresh { empty(304) } else { Response::new(AxumBody::from(body)) };
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/calendar; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache, must-revalidate"));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    Ok(res)
}

fn feed_json(feed: &serde_json::Map<String, Value>, token: &str) -> Value {
    json!({
        "id": feed.get("id"),
        "calendarIds": feed.get("calendar_ids"),
        "createdAt": feed.get("created_at"),
        "url": format!("/calendar/feeds/{token}.ics"),
    })
}

async fn create_feed(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let mut requested: Vec<Value> = Vec::new();
    if let Some(Value::Array(ids)) = body.get("calendarIds") {
        for id in ids {
            // `new Set(...)` (SameValueZero; objects never compare equal).
            if id.is_object() || id.is_array() || !requested.contains(id) {
                requested.push(id.clone());
            }
        }
    }
    let ids: Option<Vec<String>> = requested.iter().map(|v| v.as_str().map(str::to_owned)).collect();
    let Some(ids) = ids.filter(|i| !i.is_empty()) else {
        return Ok(err(400, "calendarIds must contain at least one calendar"));
    };
    let owned =
        s.db.rows(
            "SELECT id, name FROM calendars WHERE id = ANY($1) AND owner_user_id = $2 ORDER BY created_at",
            params![ids.clone(), &auth.user_id],
        )
        .await?;
    if owned.len() != ids.len() {
        return Ok(err(404, "Calendar not found"));
    }
    let (token, hash) = issue_token();
    let feed = s
        .db
        .json_opt(
            "INSERT INTO calendar_secret_feeds (owner_user_id, token_hash, calendar_ids) VALUES ($1, $2, $3) RETURNING id, calendar_ids, created_at",
            params![&auth.user_id, hash, ids],
        )
        .await?
        .unwrap_or_default();
    Ok(json(201, json!({ "feed": feed_json(&feed, &token), "secret": token })))
}

async fn list_feeds(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let rows = s
        .db
        .json_rows(
            "SELECT id, calendar_ids, created_at, revoked_at FROM calendar_secret_feeds WHERE owner_user_id = $1 ORDER BY created_at DESC",
            params![&auth.user_id],
        )
        .await?;
    let feeds: Vec<Value> = rows
        .iter()
        .map(|f| {
            json!({
                "id": f.get("id"),
                "calendarIds": f.get("calendar_ids"),
                "createdAt": f.get("created_at"),
                "revokedAt": f.get("revoked_at"),
            })
        })
        .collect();
    Ok(ok(json!({ "feeds": feeds })))
}

async fn revoke_feed(State(s): State<AppState>, auth: Auth, Path(feed_id): Path<String>) -> Result<Response> {
    let row = s
        .db
        .opt(
            "UPDATE calendar_secret_feeds SET revoked_at = COALESCE(revoked_at, NOW()) WHERE id = $1 AND owner_user_id = $2 RETURNING id",
            params![feed_id, &auth.user_id],
        )
        .await?;
    Ok(if row.is_some() { empty(204) } else { err(404, "Feed not found") })
}

async fn rotate_feed(State(s): State<AppState>, auth: Auth, Path(feed_id): Path<String>) -> Result<Response> {
    let (token, hash) = issue_token();
    let feed = s
        .db
        .json_opt(
            "UPDATE calendar_secret_feeds SET token_hash = $1, revoked_at = NULL WHERE id = $2 AND owner_user_id = $3 RETURNING id, calendar_ids, created_at",
            params![hash, feed_id, &auth.user_id],
        )
        .await?;
    let Some(feed) = feed else { return Ok(err(404, "Feed not found")) };
    Ok(ok(json!({ "feed": feed_json(&feed, &token), "secret": token })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_param_strips_one_ics_suffix() {
        assert_eq!(token_param("abc.ics"), "abc");
        assert_eq!(token_param("abc.ICS"), "abc");
        assert_eq!(token_param("abc.ics.ics"), "abc.ics");
        assert_eq!(token_param(".ics"), ".ics");
        assert_eq!(token_param("abc"), "abc");
    }

    #[test]
    fn tokens_must_be_43_base64url_chars() {
        assert!(hash_token(&"a".repeat(43)).is_some());
        assert!(hash_token(&"a".repeat(42)).is_none());
        assert!(hash_token(&format!("{}+", "a".repeat(42))).is_none());
        let (token, hash) = issue_token();
        assert_eq!(hash_token(&token), Some(hash));
    }

    #[test]
    fn folding_and_dates_match_node() {
        let line = format!("SUMMARY:{}", "é".repeat(40));
        let folded = fold(&line);
        assert!(folded.split("\r\n ").all(|l| l.len() <= 75));
        assert_eq!(folded.replace("\r\n ", ""), line);
        let t: DateTime<Utc> = "2026-11-01T09:00:00.123Z".parse().unwrap();
        assert_eq!(date_value(&t, false), "20261101T090000Z");
        assert_eq!(date_value(&t, true), "20261101");
        assert_eq!(escape_text("a,b;c\\d\r\ne"), "a\\,b\\;c\\\\d\\ne");
    }
}
