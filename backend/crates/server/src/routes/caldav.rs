//! CalDAV server for local calendars (port of `routes/caldav.ts`), mounted at `/caldav`
//! and sharing DAV application-password authentication with CardDAV.

use std::sync::LazyLock;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use inboxora_core::pg::Row;
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Result, params};
use inboxora_mail::sanitize::encode_uri_component;
use inboxora_mail::text::js_trim;
use inboxora_pim::ical::{parse_calendar_event, parse_utc};
use regex::Regex;
use serde_json::{Map, Value};
use uuid::Uuid;

use super::dav::{self, Buckets, present, send_with_etag, send_xml, tagged, with_etag, xml_escape};
use super::util::empty;
use crate::http::ClientIp;
use crate::state::AppState;

const MOUNT: &str = "/caldav";
const DAV_NS: &str = "DAV:";
const CALDAV_NS: &str = "urn:ietf:params:xml:ns:caldav";
const XML_HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;

static BUCKETS: LazyLock<Buckets> = LazyLock::new(Buckets::default);
static SYNC_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"<(?:[A-Za-z][A-Za-z0-9_.-]*:)?sync-token(?:\s[^>]*)?>([^<]*)</(?:[A-Za-z][A-Za-z0-9_.-]*:)?sync-token>",
    )
    .unwrap()
});
static HREF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<(?:[A-Za-z][A-Za-z0-9_.-]*:)?href(?:\s[^>]*)?>([^<]+)</(?:[A-Za-z][A-Za-z0-9_.-]*:)?href>").unwrap()
});
static TIME_RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)<(?:[A-Za-z][A-Za-z0-9_.-]*:)?time-range(?-u:\b)[^>]*(?-u:\b)start=["']([0-9]{8}T[0-9]{6}Z)["'][^>]*(?-u:\b)end=["']([0-9]{8}T[0-9]{6}Z)["'][^>]*/?\s*>"#,
    )
    .unwrap()
});
static UP_TO_SLASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^.*/").unwrap());

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/caldav", any(handle))
        .route("/caldav/", any(handle))
        .route("/caldav/{*rest}", any(handle))
        .route("/.well-known/caldav", any(dav::well_known_caldav))
        .route("/.well-known/caldav/", any(dav::well_known_caldav))
}

async fn handle(State(s): State<AppState>, ClientIp(ip): ClientIp, req: Request) -> Response {
    let method = req.method().clone();
    if let Some(limited) = BUCKETS.check(&ip) {
        return tagged(limited, &method, MOUNT);
    }
    let user_id = match dav::authenticate(&s, req.headers(), &ip, "Inboxora CalDAV", "caldav_auth_fail").await {
        Ok(u) => u,
        Err(rejection) => return tagged(rejection, &method, MOUNT),
    };
    let segs = match dav::segments(dav::rest_of(&req, MOUNT)) {
        Ok(s) => s,
        Err(e) => return tagged(e.into_response(), &method, "unmatched"),
    };
    let m = &method;
    let (pattern, result) = match segs.as_deref() {
        _ if dav::is(m, "OPTIONS") => ("/caldav*", Ok(options())),
        Some([]) if dav::is(m, "PROPFIND") => ("/caldav/", Ok(propfind_root(&user_id))),
        Some([u]) if dav::is(m, "PROPFIND") => ("/caldav/:userId/", propfind_user(&s, &user_id, u).await),
        Some([u, c]) if dav::is(m, "PROPFIND") => {
            ("/caldav/:userId/:calendarId/", propfind_calendar(&s, &user_id, u, c).await)
        }
        Some([u, c]) if dav::is(m, "REPORT") => {
            let (u, c) = (u.clone(), c.clone());
            ("/caldav/:userId/:calendarId/", report(&s, &user_id, &u, &c, req).await)
        }
        Some([u, c, f]) if dav::is(m, "GET") || dav::is(m, "HEAD") => {
            ("/caldav/:userId/:calendarId/:filename", get_event(&s, &user_id, u, c, f).await)
        }
        Some([u, c, f]) if dav::is(m, "PUT") => {
            let (u, c, f) = (u.clone(), c.clone(), f.clone());
            ("/caldav/:userId/:calendarId/:filename", put_event(&s, &user_id, &u, &c, &f, req).await)
        }
        Some([u, c, f]) if dav::is(m, "DELETE") => {
            ("/caldav/:userId/:calendarId/:filename", delete_event(&s, &user_id, u, c, f, req.headers()).await)
        }
        _ => return tagged(crate::http::middleware::not_found(req).await, m, "unmatched"),
    };
    tagged(result.unwrap_or_else(IntoResponse::into_response), m, pattern)
}

fn options() -> Response {
    let mut res = empty(200);
    let h = res.headers_mut();
    h.insert(header::ALLOW, HeaderValue::from_static("OPTIONS, GET, PUT, DELETE, PROPFIND, REPORT"));
    h.insert("dav", HeaderValue::from_static("1, 2, 3, calendar-access"));
    res
}

fn multistatus(responses: &[String]) -> String {
    format!(
        "{XML_HEADER}<D:multistatus xmlns:D=\"{DAV_NS}\" xmlns:C=\"{CALDAV_NS}\">{}</D:multistatus>",
        responses.concat()
    )
}

fn response(href: &str, props: &[String], status: &str) -> String {
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>{}</D:prop><D:status>HTTP/1.1 {status}</D:status></D:propstat></D:response>",
        xml_escape(Some(href)),
        props.concat()
    )
}

fn propfind_root(user_id: &str) -> Response {
    let principal = format!("/caldav/{user_id}/");
    send_xml(
        207,
        multistatus(&[response(
            "/caldav/",
            &[
                "<D:resourcetype><D:collection/></D:resourcetype>".into(),
                format!(
                    "<D:current-user-principal><D:href>{}</D:href></D:current-user-principal>",
                    xml_escape(Some(&principal))
                ),
            ],
            "200 OK",
        )]),
    )
}

async fn propfind_user(s: &AppState, user_id: &str, u: &str) -> Result<Response> {
    if u != user_id {
        return Ok(empty(403));
    }
    let first = s
        .db
        .rows("SELECT id, name, sync_token FROM calendars WHERE user_id = $1 ORDER BY created_at ASC", params![user_id])
        .await?
        .first()
        .map(|r| r.get::<_, Uuid>("id"));
    let principal = format!("/caldav/{user_id}/");
    let home = first.map_or_else(|| principal.clone(), |id| format!("/caldav/{user_id}/{id}/"));
    let p = xml_escape(Some(&principal));
    Ok(send_xml(
        207,
        multistatus(&[response(
            &principal,
            &[
                "<D:resourcetype><D:principal/><D:collection/></D:resourcetype>".into(),
                format!("<D:displayname>{}</D:displayname>", xml_escape(Some(user_id))),
                format!("<D:current-user-principal><D:href>{p}</D:href></D:current-user-principal>"),
                format!("<C:calendar-home-set><D:href>{}</D:href></C:calendar-home-set>", xml_escape(Some(&home))),
            ],
            "200 OK",
        )]),
    ))
}

async fn propfind_calendar(s: &AppState, user_id: &str, u: &str, c: &str) -> Result<Response> {
    if u != user_id {
        return Ok(empty(403));
    }
    let Some(cal) =
        s.db.opt("SELECT id, name, sync_token FROM calendars WHERE id = $1 AND user_id = $2", params![c, user_id])
            .await?
    else {
        return Ok(empty(404));
    };
    let id: Uuid = cal.get("id");
    let token = match live_resources(s, user_id, c).await {
        Some(live) => Some(live_token(&live)),
        None => cal.get::<_, Option<String>>("sync_token"),
    };
    Ok(send_xml(
        207,
        multistatus(&[response(
            &format!("/caldav/{user_id}/{id}/"),
            &[
                "<D:resourcetype><D:collection/><C:calendar/></D:resourcetype>".into(),
                format!("<D:displayname>{}</D:displayname>", xml_escape(cal.get::<_, Option<&str>>("name"))),
                format!("<D:sync-token>{}</D:sync-token>", xml_escape(token.as_deref())),
            ],
            "200 OK",
        )]),
    ))
}

/// `uidFromCalendarHref`: the decoded last path segment, or `None`.
fn uid_from_href(href: &str) -> Option<String> {
    let last = UP_TO_SLASH.replace(js_trim(href), "");
    dav::decode_uri_component(&last).filter(|s| !s.is_empty())
}

/// `etagMatches(header, etag)`.
fn etag_matches(header: &str, etag: Option<&str>) -> bool {
    header == "*"
        || header.split(',').any(|v| {
            let v = js_trim(v);
            Some(v.strip_prefix("W/").unwrap_or(v).replace('"', "")).as_deref() == etag
        })
}

struct EventRow {
    uid: Option<String>,
    etag: Option<String>,
    deleted: bool,
    raw_ical: Option<String>,
    dav_filename: Option<String>,
}

impl EventRow {
    fn from_row(r: &Row) -> Self {
        let text = |c: &str| r.try_get::<_, Option<String>>(c).ok().flatten();
        EventRow {
            uid: text("uid"),
            etag: text("etag"),
            deleted: r.try_get::<_, Option<bool>>("deleted").ok().flatten().unwrap_or(false),
            raw_ical: text("raw_ical"),
            dav_filename: text("dav_filename"),
        }
    }
}

/// The resources of a read-only remote calendar (served live), or `None` for a stored one.
async fn live_resources(s: &AppState, user_id: &str, c: &str) -> Option<Vec<EventRow>> {
    if !inboxora_mail::util::is_uuid(c) {
        return None;
    }
    let ids = [c.to_owned()];
    if crate::services::remote_dav::live_calendars(&s.db, user_id, Some(&ids)).await.is_empty() {
        return None;
    }
    let rows = crate::services::remote_dav::event_rows(&s.db, user_id, Some(&ids)).await;
    let text = |r: &Map<String, Value>, k: &str| r.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(
        rows.iter()
            .map(|r| EventRow {
                uid: text(r, "uid"),
                etag: text(r, "etag"),
                deleted: false,
                raw_ical: text(r, "raw_ical"),
                dav_filename: text(r, "remote_href").and_then(|h| uid_from_href(&h)),
            })
            .collect(),
    )
}

/// A live calendar's sync token: changes with any resource's etag (or the set of resources).
fn live_token(rows: &[EventRow]) -> String {
    let mut parts: Vec<String> = rows
        .iter()
        .map(|r| format!("{}:{}", r.uid.as_deref().unwrap_or(""), r.etag.as_deref().unwrap_or("")))
        .collect();
    parts.sort();
    format!("live-{}", &inboxora_core::crypto::sha256_hex(parts.join("\n"))[..16])
}

fn live_filename(e: &EventRow) -> String {
    e.dav_filename
        .clone()
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| format!("{}.ics", e.uid.as_deref().unwrap_or("undefined")))
}

async fn report(s: &AppState, user_id: &str, u: &str, c: &str, req: Request) -> Result<Response> {
    if u != user_id {
        return Ok(empty(403));
    }
    let Some(cal) =
        s.db.opt(
            "SELECT id, sync_token, sync_version, sync_floor FROM calendars WHERE id = $1 AND user_id = $2",
            params![c, user_id],
        )
        .await?
    else {
        return Ok(empty(404));
    };
    let cal_id: Uuid = cal.get("id");
    let sync_version: i64 = cal.get("sync_version");
    let sync_floor: i64 = cal.get("sync_floor");
    let sync_token: Option<String> = cal.get("sync_token");
    let body = match dav::raw_body(req).await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    let is_sync = body.contains("sync-collection");
    let is_query = body.contains("calendar-query");
    let is_multiget = body.contains("calendar-multiget");
    if !is_sync && !is_query && !is_multiget {
        return Ok(empty(400));
    }
    let base = format!("/caldav/{user_id}/{cal_id}/");
    if let Some(live) = live_resources(s, user_id, c).await {
        return Ok(live_report(&base, &body, live, is_sync, is_multiget));
    }
    let rows = if is_sync {
        let token = SYNC_TOKEN.captures(&body).map(|m| js_trim(m.get(1).map_or("", |x| x.as_str())).to_owned());
        match token.filter(|t| !t.is_empty()) {
            Some(token) => {
                let version = token
                    .strip_prefix("sync-")
                    .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
                    .and_then(|d| d.parse::<f64>().ok())
                    .filter(|v| *v <= 9_007_199_254_740_991.0)
                    .map(|v| v as i64);
                let Some(version) = version.filter(|v| *v <= sync_version && *v >= sync_floor) else {
                    return Ok(send_xml(
                        409,
                        format!("{XML_HEADER}<D:error xmlns:D=\"{DAV_NS}\"><D:valid-sync-token/></D:error>"),
                    ));
                };
                s.db.rows(
                    "SELECT ch.uid, ch.recurrence_id, e.etag, (ch.deleted OR e.id IS NULL) AS deleted, e.raw_ical, \
                       COALESCE(e.dav_filename, ch.dav_filename) AS dav_filename \
                     FROM calendar_sync_changes ch LEFT JOIN calendar_events e \
                       ON e.calendar_id = ch.calendar_id AND e.uid = ch.uid AND e.recurrence_id = ch.recurrence_id \
                     WHERE ch.calendar_id = $1 AND ch.version > $2 ORDER BY ch.uid, ch.recurrence_id",
                    params![cal_id, version],
                )
                .await?
            }
            None => {
                s.db.rows(
                    "SELECT uid, recurrence_id, etag, false AS deleted, raw_ical, dav_filename FROM calendar_events \
                     WHERE calendar_id = $1 AND recurrence_id = $2 ORDER BY uid ASC",
                    params![cal_id, ""],
                )
                .await?
            }
        }
    } else if is_multiget {
        let uids: Vec<String> = HREF.captures_iter(&body).filter_map(|m| uid_from_href(&m[1])).collect();
        if uids.is_empty() {
            return Ok(empty(400));
        }
        s.db.rows(
            "SELECT uid, recurrence_id, etag, raw_ical, dav_filename FROM calendar_events WHERE calendar_id = $1 \
             AND recurrence_id = $2 AND COALESCE(dav_filename, uid || '.ics') = ANY($3) ORDER BY uid ASC",
            &[Val::from(cal_id), Val::from(""), Val::from(uids)],
        )
        .await?
    } else {
        let range = TIME_RANGE.captures(&body).map(|m| (parse_utc(&m[1]), parse_utc(&m[2])));
        match range {
            Some((Some(start), Some(end))) if end > start => {
                s.db.rows(
                    "SELECT uid, recurrence_id, etag, raw_ical, dav_filename FROM calendar_events WHERE calendar_id = $1 \
                     AND recurrence_id = $2 AND ((starts_at < $4 AND ends_at > $3) OR recurring) ORDER BY uid ASC",
                    params![cal_id, "", start, end],
                )
                .await?
            }
            Some(_) => return Ok(empty(400)),
            None => {
                s.db.rows(
                    "SELECT uid, recurrence_id, etag, raw_ical, dav_filename FROM calendar_events WHERE calendar_id = $1 \
                     AND recurrence_id = $2 ORDER BY uid ASC",
                    params![cal_id, ""],
                )
                .await?
            }
        }
    };
    let responses: Vec<String> = rows
        .iter()
        .map(EventRow::from_row)
        .map(|e| {
            let name = e
                .dav_filename
                .clone()
                .filter(|f| !f.is_empty())
                .unwrap_or_else(|| format!("{}.ics", e.uid.as_deref().unwrap_or("undefined")));
            let href = format!("{base}{}", encode_uri_component(&name));
            if e.deleted {
                response(&href, &["<D:resourcetype/>".into()], "404 Not Found")
            } else {
                response(
                    &href,
                    &[
                        "<D:resourcetype/>".into(),
                        format!("<D:getetag>\"{}\"</D:getetag>", xml_escape(e.etag.as_deref())),
                        "<D:getcontenttype>text/calendar;charset=utf-8</D:getcontenttype>".into(),
                        format!("<C:calendar-data>{}</C:calendar-data>", xml_escape(e.raw_ical.as_deref())),
                    ],
                    "200 OK",
                )
            }
        })
        .collect();
    let xml = multistatus(&responses).replacen(
        "</D:multistatus>",
        &format!("<D:sync-token>{}</D:sync-token></D:multistatus>", xml_escape(sync_token.as_deref())),
        1,
    );
    Ok(send_xml(207, xml))
}

/// A REPORT on a live calendar. Without stored change history, a sync-collection with an
/// outdated token is answered with valid-sync-token (the client then resyncs fully).
fn live_report(base: &str, body: &str, rows: Vec<EventRow>, is_sync: bool, is_multiget: bool) -> Response {
    let token = live_token(&rows);
    let mut selected: Vec<&EventRow> = rows.iter().collect();
    if is_sync {
        let sent = SYNC_TOKEN.captures(body).map(|m| js_trim(m.get(1).map_or("", |x| x.as_str())).to_owned());
        match sent.filter(|t| !t.is_empty()) {
            Some(t) if t == token => selected.clear(),
            Some(_) => {
                return send_xml(
                    409,
                    format!("{XML_HEADER}<D:error xmlns:D=\"{DAV_NS}\"><D:valid-sync-token/></D:error>"),
                );
            }
            None => {}
        }
    } else if is_multiget {
        let wanted: Vec<String> = HREF.captures_iter(body).filter_map(|m| uid_from_href(&m[1])).collect();
        if wanted.is_empty() {
            return empty(400);
        }
        selected.retain(|e| wanted.contains(&live_filename(e)));
    } else if let Some(m) = TIME_RANGE.captures(body) {
        let (Some(start), Some(end)) = (parse_utc(&m[1]), parse_utc(&m[2])) else { return empty(400) };
        if end <= start {
            return empty(400);
        }
        selected.retain(|e| {
            let raw = e.raw_ical.as_deref().unwrap_or("");
            match inboxora_pim::ical::parse_calendar_event(raw) {
                Some(p) => (p.starts_at < end && p.ends_at > start) || raw.to_uppercase().contains("RRULE"),
                None => false,
            }
        });
    }
    let responses: Vec<String> = selected
        .iter()
        .map(|e| {
            response(
                &format!("{base}{}", encode_uri_component(&live_filename(e))),
                &[
                    "<D:resourcetype/>".into(),
                    format!("<D:getetag>\"{}\"</D:getetag>", xml_escape(e.etag.as_deref())),
                    "<D:getcontenttype>text/calendar;charset=utf-8</D:getcontenttype>".into(),
                    format!("<C:calendar-data>{}</C:calendar-data>", xml_escape(e.raw_ical.as_deref())),
                ],
                "200 OK",
            )
        })
        .collect();
    let xml = multistatus(&responses).replacen(
        "</D:multistatus>",
        &format!("<D:sync-token>{}</D:sync-token></D:multistatus>", xml_escape(Some(&token))),
        1,
    );
    send_xml(207, xml)
}

async fn get_event(s: &AppState, user_id: &str, u: &str, c: &str, filename: &str) -> Result<Response> {
    if u != user_id {
        return Ok(empty(403));
    }
    let row =
        s.db.opt(
            "SELECT e.raw_ical, e.etag FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id \
             WHERE c.id = $1 AND c.user_id = $2 AND COALESCE(e.dav_filename, e.uid || '.ics') = $3",
            params![c, user_id, filename],
        )
        .await?;
    let Some(row) = row else {
        let live = live_resources(s, user_id, c).await.unwrap_or_default();
        return Ok(match live.iter().find(|e| live_filename(e) == filename) {
            Some(e) => send_with_etag("text/calendar; charset=utf-8", e.etag.as_deref(), e.raw_ical.clone()),
            None => empty(404),
        });
    };
    let etag: Option<String> = row.get("etag");
    Ok(send_with_etag("text/calendar; charset=utf-8", etag.as_deref(), row.get("raw_ical")))
}

/// The calendar row a mutation targets: `Err` carries the 404/403 to send.
async fn writable_calendar(s: &AppState, c: &str, user_id: &str) -> Result<std::result::Result<Uuid, Response>> {
    let Some(cal) =
        s.db.opt("SELECT id, source, read_only FROM calendars WHERE id = $1 AND user_id = $2", params![c, user_id])
            .await?
    else {
        return Ok(Err(empty(404)));
    };
    let local = cal.get::<_, Option<&str>>("source") == Some("local");
    if !local || cal.get::<_, Option<bool>>("read_only").unwrap_or(false) {
        return Ok(Err(empty(403)));
    }
    Ok(Ok(cal.get("id")))
}

async fn put_event(s: &AppState, user_id: &str, u: &str, c: &str, filename: &str, req: Request) -> Result<Response> {
    if u != user_id {
        return Ok(empty(403));
    }
    let cal_id = match writable_calendar(s, c, user_id).await? {
        Ok(id) => id,
        Err(r) => return Ok(r),
    };
    let headers: HeaderMap = req.headers().clone();
    let body = match dav::raw_body(req).await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    let Some(event) = parse_calendar_event(&body) else { return Ok(empty(400)) };
    let current =
        s.db.opt(
            "SELECT uid, dav_filename, etag, invite_account_id FROM calendar_events WHERE calendar_id = $1 \
             AND (uid = $2 OR COALESCE(dav_filename, uid || '.ics') = $4) AND recurrence_id = $3",
            params![cal_id, &event.uid, "", filename],
        )
        .await?;
    let current = current.map(|r| {
        let invite: Option<Uuid> = r.get("invite_account_id");
        (EventRow::from_row(&r), invite.is_some())
    });
    if let Some((cur, _)) = &current
        && let Some(cur_uid) = cur.uid.as_deref().filter(|u| !u.is_empty())
    {
        let name = cur.dav_filename.clone().filter(|f| !f.is_empty()).unwrap_or_else(|| format!("{cur_uid}.ics"));
        if cur_uid != event.uid || name != filename {
            return Ok(empty(409));
        }
    }
    if current.as_ref().is_some_and(|(_, invite)| *invite) {
        return Ok(empty(409));
    }
    let if_none_match = present(&headers, "if-none-match");
    if if_none_match == Some("*") && current.is_some() {
        return Ok(empty(412));
    }
    let if_match = present(&headers, "if-match");
    if let Some(m) = if_match
        && current.as_ref().is_none_or(|(cur, _)| !etag_matches(m, cur.etag.as_deref()))
    {
        return Ok(empty(412));
    }
    let current_etag = current.as_ref().and_then(|(c, _)| c.etag.clone()).filter(|e| !e.is_empty());
    let attendees = inboxora_core::json::stringify(&serde_json::to_value(&event.attendees).unwrap_or_default());
    let stored = s
        .db
        .opt(
            "INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at, all_day, timezone, \
             description, location, url, organizer, attendees, dav_filename) \
             VALUES ($1, $2, $3, $4, gen_random_uuid()::text, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14::jsonb, $15) \
             ON CONFLICT (calendar_id, uid, recurrence_id) DO UPDATE SET \
             raw_ical = EXCLUDED.raw_ical, etag = gen_random_uuid()::text, summary = EXCLUDED.summary, \
             starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, all_day = EXCLUDED.all_day, \
             timezone = EXCLUDED.timezone, description = EXCLUDED.description, location = EXCLUDED.location, \
             url = EXCLUDED.url, organizer = EXCLUDED.organizer, attendees = EXCLUDED.attendees, dav_filename = EXCLUDED.dav_filename, updated_at = NOW() \
             WHERE calendar_events.invite_account_id IS NULL AND COALESCE(calendar_events.dav_filename, calendar_events.uid || '.ics') = EXCLUDED.dav_filename \
             AND calendar_events.etag = $16 RETURNING uid, etag",
            params![
                cal_id,
                user_id,
                &event.uid,
                &event.raw,
                event.summary.clone(),
                event.starts_at,
                event.ends_at,
                event.all_day,
                event.time_zone.clone(),
                event.description.clone(),
                event.location.clone(),
                event.url.clone(),
                event.organizer.clone(),
                attendees,
                filename,
                current_etag,
            ],
        )
        .await;
    let stored = match stored {
        Ok(r) => r,
        Err(e) if e.is_unique_violation() => return Ok(empty(if if_none_match == Some("*") { 412 } else { 409 })),
        Err(e) => return Err(e),
    };
    let Some(stored) = stored else {
        return Ok(empty(if if_match.is_some() || if_none_match.is_some() { 412 } else { 409 }));
    };
    let etag: String = stored.get("etag");
    Ok(with_etag(if current.is_some() { 204 } else { 201 }, &etag))
}

async fn delete_event(
    s: &AppState,
    user_id: &str,
    u: &str,
    c: &str,
    filename: &str,
    headers: &HeaderMap,
) -> Result<Response> {
    if u != user_id {
        return Ok(empty(403));
    }
    let cal_id = match writable_calendar(s, c, user_id).await? {
        Ok(id) => id,
        Err(r) => return Ok(r),
    };
    let Some(current) =
        s.db.opt(
            "SELECT etag, invite_account_id FROM calendar_events WHERE calendar_id = $1 \
             AND COALESCE(dav_filename, uid || '.ics') = $2 AND recurrence_id = $3",
            params![cal_id, filename, ""],
        )
        .await?
    else {
        return Ok(empty(404));
    };
    if current.get::<_, Option<Uuid>>("invite_account_id").is_some() {
        return Ok(empty(409));
    }
    let etag: Option<String> = current.get("etag");
    let if_match = present(headers, "if-match");
    if if_match.is_some_and(|m| !etag_matches(m, etag.as_deref())) {
        return Ok(empty(412));
    }
    let deleted =
        s.db.opt(
            "DELETE FROM calendar_events WHERE calendar_id = $1 AND COALESCE(dav_filename, uid || '.ics') = $2 \
             AND recurrence_id = $3 AND invite_account_id IS NULL AND etag = $4 RETURNING id",
            params![cal_id, filename, "", etag],
        )
        .await?;
    Ok(empty(if deleted.is_some() {
        204
    } else if if_match.is_some() {
        412
    } else {
        409
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_matching_follows_node() {
        assert!(etag_matches("*", None));
        assert!(etag_matches("W/\"abc\", \"def\"", Some("def")));
        assert!(etag_matches("\"abc\"", Some("abc")));
        assert!(!etag_matches("\"abc\"", Some("abd")));
        assert!(!etag_matches("\"abc\"", None));
    }

    #[test]
    fn hrefs_and_time_ranges_parse_like_node() {
        assert_eq!(uid_from_href(" /caldav/u/c/a%20b.ics ").as_deref(), Some("a b.ics"));
        assert_eq!(uid_from_href("/caldav/u/c/"), None);
        assert_eq!(uid_from_href("/x/%E0"), None);
        let body = r#"<C:TIME-RANGE START="20260101T000000Z" end='20260201T000000Z'/>"#;
        let m = TIME_RANGE.captures(body).unwrap();
        assert_eq!((&m[1], &m[2]), ("20260101T000000Z", "20260201T000000Z"));
    }
}
