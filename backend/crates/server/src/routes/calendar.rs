//! `/api/calendar` (port of routes/calendar.ts). This module serves calendars and event
//! reads; event mutations, invitations and external sources join it with CalDAV writeback
//! and mail delivery.

use std::sync::LazyLock;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderValue;
use axum::response::Response;
use axum::routing::{get, patch};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Result, params};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::http::{Auth, Body, Qs};
use crate::routes::contacts::query_string;
use crate::routes::search::js_date_parse;
use crate::routes::util::{empty, err, json, ok};
use crate::services::calendar::{self as cal, EVENT_COLUMNS, coverage_predicate};
use crate::services::remote_dav;
use crate::state::AppState;

const MAX_EVENT_RANGE_DAYS: i64 = 366;
const CONTACT_CALENDAR_ID: &str = "contacts-birthdays";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/calendars", get(list_calendars).post(create_calendar))
        .route("/calendars/{calendar_id}", patch(update_calendar).delete(delete_calendar))
        .route("/events", get(list_events))
        .route("/events/search", get(search_events))
        .route("/events/{id}/attendees", get(attendees))
        .merge(super::calendar_events::router())
        .merge(super::calendar_feed::api_router())
        .merge(super::calendar_sources::router())
}

async fn contact_appearance(s: &AppState, user_id: &str) -> Result<Map<String, Value>> {
    let row =
        s.db.json_opt(
            "SELECT preferences->'calendarContactAppearance' AS appearance FROM users WHERE id = $1",
            params![user_id],
        )
        .await?;
    Ok(row.and_then(|r| r.get("appearance").and_then(Value::as_object).cloned()).unwrap_or_default())
}

fn truthy_str(m: &Map<String, Value>, k: &str) -> Option<Value> {
    m.get(k).filter(|v| inboxora_core::json::truthy(Some(v))).cloned()
}

async fn attendees(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Response> {
    let id = id.split('@').next().unwrap_or("").to_owned();
    let row = s
        .db
        .opt(
            "SELECT e.raw_ical FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id WHERE e.id::text = $1 AND e.user_id = $2 AND c.owner_user_id = $2",
            params![&id, &auth.user_id],
        )
        .await?;
    let raw: Option<String> = match row {
        Some(row) => row.get(0),
        None => match remote_dav::event_by_id(&s.db, &auth.user_id, &id).await {
            Some(e) => e.get("raw_ical").and_then(Value::as_str).map(str::to_owned),
            None => return Ok(err(404, "Event not found")),
        },
    };
    Ok(ok(json!({ "attendees": inboxora_pim::ical::attendee_statuses(raw.as_deref().unwrap_or("")) })))
}

async fn list_calendars(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let mut rows = s
        .db
        .json_rows(
            // `source_name`: the DAV server / feed a synced calendar comes from (`source:<id>[:<hash>]`),
            // so the sidebar can group calendars by origin.
            "SELECT c.id, c.name, c.description, c.color, c.source, c.external_url, c.read_only, c.display_visible, c.owner_user_id, \
             c.sync_token, c.created_at, c.updated_at, NULLIF(src.display_name, '') AS source_name \
             FROM calendars c LEFT JOIN calendar_import_sources src ON c.external_url LIKE 'source:%' \
               AND src.id::text = split_part(substr(c.external_url, 8), ':', 1) AND src.user_id = c.user_id \
             WHERE c.user_id = $1 AND c.owner_user_id = $1 ORDER BY c.created_at ASC",
            params![&auth.user_id],
        )
        .await?;
    let a = contact_appearance(&s, &auth.user_id).await?;
    rows.push(json!({
        "id": CONTACT_CALENDAR_ID,
        "name": truthy_str(&a, "name").unwrap_or_else(|| json!("Contact dates")),
        "custom_name": truthy_str(&a, "name").is_some(),
        "description": "Birthdays and anniversaries from contacts",
        "color": truthy_str(&a, "color").unwrap_or_else(|| json!("#e879f9")),
        "source": "contacts",
        "external_url": null,
        "read_only": true,
        "display_visible": a.get("displayVisible") == Some(&Value::Bool(true)),
    }));
    Ok(ok(json!({ "calendars": rows })))
}

fn calendar_name(v: Option<&Value>) -> Option<String> {
    let n = v.and_then(Value::as_str).map(inboxora_mail::text::js_trim).unwrap_or("");
    (!n.is_empty() && inboxora_mail::text::js_len(n) <= 120).then(|| n.to_owned())
}

/// `Ok(None)` = null color, `Err` = invalid.
fn calendar_color(v: Option<&Value>) -> std::result::Result<Option<String>, ()> {
    static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^#[0-9a-f]{6}$").expect("regex"));
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) if HEX.is_match(s) => Ok(Some(s.clone())),
        _ => Err(()),
    }
}

const CAL_RETURNING: &str = "RETURNING id, user_id, owner_user_id, name, description, color, source, external_url, read_only, display_visible, sync_token, created_at, updated_at";
const CAL_INVALID: &str = "name, a hex color, and displayVisible are required";

async fn create_calendar(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let name = calendar_name(body.get("name"));
    let color = calendar_color(body.get("color"));
    let visible = match body.get("displayVisible") {
        None | Some(Value::Null) => Some(true),
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    };
    let (Some(name), Ok(color), Some(visible)) = (name, color, visible) else { return Ok(err(400, CAL_INVALID)) };
    match s
        .db
        .json_opt(
            &format!("INSERT INTO calendars (user_id, owner_user_id, name, color, display_visible, source, read_only) VALUES ($1, $1, $2, $3, $4, 'local', false) {CAL_RETURNING}"),
            params![&auth.user_id, &name, color, visible],
        )
        .await
    {
        Ok(row) => Ok(json(201, json!({ "calendar": row }))),
        Err(e) if e.is_unique_violation() => Ok(err(409, "A calendar with that name already exists")),
        Err(e) => Err(e),
    }
}

async fn update_calendar(
    State(s): State<AppState>,
    auth: Auth,
    Path(calendar_id): Path<String>,
    body: Body,
) -> Result<Response> {
    let name = calendar_name(body.get("name"));
    let color = calendar_color(body.get("color"));
    let visible = body.get("displayVisible").and_then(Value::as_bool);
    let (Some(name), Ok(color), Some(visible)) = (name, color, visible) else { return Ok(err(400, CAL_INVALID)) };
    if calendar_id == CONTACT_CALENDAR_ID {
        let custom = if body.get("customName") == Some(&Value::Bool(false)) { None } else { Some(name) };
        s.db.exec(
            "UPDATE users SET preferences = COALESCE(preferences, '{}'::jsonb) || jsonb_build_object('calendarContactAppearance', $2::jsonb) WHERE id = $1",
            &[Val::from(&auth.user_id), Val::from(inboxora_core::json::stringify(&json!({ "name": custom, "color": color, "displayVisible": visible })))],
        )
        .await?;
        return Ok(ok(json!({ "calendar": {
            "id": CONTACT_CALENDAR_ID, "name": custom.clone().unwrap_or_else(|| "Contact dates".into()), "custom_name": custom.is_some(),
            "color": color, "display_visible": visible, "source": "contacts", "read_only": true,
        } })));
    }
    match s
        .db
        .json_opt(
            &format!("UPDATE calendars SET name = $1, color = $2, display_visible = $3, updated_at = NOW() WHERE id = $4 AND owner_user_id = $5 AND user_id = $5 {CAL_RETURNING}"),
            params![&name, color, visible, &calendar_id, &auth.user_id],
        )
        .await
    {
        Ok(Some(row)) => Ok(ok(json!({ "calendar": row }))),
        Ok(None) => Ok(err(404, "Calendar not found")),
        Err(e) if e.is_unique_violation() => Ok(err(409, "A calendar with that name already exists")),
        Err(e) => Err(e),
    }
}

async fn delete_calendar(
    State(s): State<AppState>,
    auth: Auth,
    Path(calendar_id): Path<String>,
    body: Body,
) -> Result<Response> {
    let Some(confirm) = calendar_name(body.get("confirmName")) else { return Ok(err(400, "confirmName is required")) };
    let row = s
        .db
        .opt(
            "DELETE FROM calendars WHERE id = $1 AND owner_user_id = $2 AND user_id = $2 AND name = $3 AND source = 'local' AND read_only = false RETURNING id",
            params![&calendar_id, &auth.user_id, &confirm],
        )
        .await?;
    Ok(if row.is_some() { empty(204) } else { err(404, "Calendar not found") })
}

/// `parseCalendarSelection`: `Ok(None)` = every calendar.
fn calendar_selection(q: &Qs) -> std::result::Result<Option<Vec<String>>, ()> {
    let Some(raw) = q.get("calendarIds") else { return Ok(None) };
    let values: Vec<String> = match raw {
        Value::Array(a) => a.iter().map(inboxora_mail::text::js_string).collect(),
        Value::Object(_) => vec!["[object Object]".into()],
        other => vec![inboxora_mail::text::js_string(other)],
    };
    let mut ids: Vec<String> = Vec::new();
    for part in values.iter().flat_map(|v| v.split(',')).map(inboxora_mail::text::js_trim).filter(|p| !p.is_empty()) {
        if part != CONTACT_CALENDAR_ID && !inboxora_mail::util::is_uuid(part) {
            return Err(());
        }
        if !ids.iter().any(|i| i == part) {
            ids.push(part.to_owned());
        }
    }
    Ok(Some(ids))
}

const SOURCE_COLUMNS: &str = "CASE WHEN sa.id IS NOT NULL THEN e.source_message_id END AS source_message_id, sm.folder AS source_folder, \
    sa.id AS source_account_id, c.name AS calendar_name, c.color AS calendar_color, c.source, c.read_only";
const SOURCE_JOINS: &str = "LEFT JOIN messages sm ON sm.id = e.source_message_id LEFT JOIN email_accounts sa ON sa.id = sm.account_id AND sa.user_id = e.user_id";

fn start_of(v: &Value) -> Option<DateTime<Utc>> {
    v.get("starts_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// JS sort comparator on a possibly invalid date (NaN compares as "equal").
fn by_start(a: &Option<DateTime<Utc>>, b: &Option<DateTime<Utc>>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    }
}

async fn list_events(State(s): State<AppState>, auth: Auth, q: Qs) -> Result<Response> {
    let from = query_string(&q, "from").and_then(|v| js_date_parse(&v));
    let to = query_string(&q, "to").and_then(|v| js_date_parse(&v));
    let (Some(from), Some(to)) = (from, to) else { return Ok(err(400, "A valid from/to range is required")) };
    if to < from {
        return Ok(err(400, "A valid from/to range is required"));
    }
    if (to - from).num_milliseconds() > MAX_EVENT_RANGE_DAYS * 86_400_000 {
        return Ok(err(400, "The requested event range is too large"));
    }
    let Ok(selection) = calendar_selection(&q) else { return Ok(err(400, "Invalid calendar id")) };
    let selected: Option<Vec<String>> =
        selection.as_ref().map(|ids| ids.iter().filter(|i| *i != CONTACT_CALENDAR_ID).cloned().collect());
    let include_contacts = selection.as_ref().is_none_or(|ids| ids.iter().any(|i| i == CONTACT_CALENDAR_ID));
    let mut materialized: Vec<Value> = Vec::new();
    let mut fallback: Vec<Map<String, Value>> = Vec::new();
    if selected.as_ref().is_none_or(|ids| !ids.is_empty()) {
        let mut p = vec![Val::from(&auth.user_id), Val::Time(from), Val::Time(to)];
        let mut filter = "";
        if let Some(ids) = &selected {
            p.push(Val::from(ids.clone()));
            filter = " AND c.id = ANY($4::uuid[])";
        }
        let cov = coverage_predicate("s");
        materialized = s
            .db
            .json_rows(
                &format!(
                    "SELECT CASE WHEN o.recurrence_id = '' THEN e.id::text ELSE e.id::text || '@' || o.recurrence_id END AS id, \
                     CASE WHEN e.recurring THEN e.id END AS series_id, e.recurring, o.recurrence_id, o.starts_at, o.ends_at, o.all_day, o.timezone, \
                     COALESCE(o.summary, e.summary) AS summary, COALESCE(o.description, e.description) AS description, \
                     COALESCE(o.location, e.location) AS location, COALESCE(o.url, e.url) AS url, COALESCE(o.organizer, e.organizer) AS organizer, \
                     COALESCE(o.attendees, e.attendees) AS attendees, e.calendar_id, e.uid, e.etag, e.invite_account_id, e.invite_hide_attendees, e.invitation_sequence, \
                     CASE WHEN e.raw_ical LIKE '%PARTSTAT%' THEN e.raw_ical END AS partstat_ical, \
                     {SOURCE_COLUMNS} FROM calendar_occurrences o JOIN calendar_events e ON e.id = o.event_id JOIN calendars c ON c.id = o.calendar_id \
                     {SOURCE_JOINS} LEFT JOIN calendar_occurrence_state s ON s.event_id = e.id \
                     WHERE o.user_id = $1 AND c.user_id = $1 AND c.owner_user_id = $1 AND o.starts_at < $3 AND o.ends_at > $2 AND NOT {cov}{filter} \
                     ORDER BY o.starts_at ASC"
                ),
                &p,
            )
            .await?;
        fallback = s
            .db
            .json_rows(
                &format!(
                    "SELECT {EVENT_COLUMNS}, {SOURCE_COLUMNS} FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id {SOURCE_JOINS} \
                     LEFT JOIN calendar_occurrence_state s ON s.event_id = e.id WHERE e.user_id = $1 AND c.user_id = $1 AND c.owner_user_id = $1 \
                     AND ((e.starts_at < $3 AND e.ends_at > $2) OR e.recurring) AND {cov}{filter} ORDER BY e.starts_at ASC"
                ),
                &p,
            )
            .await?
            .into_iter()
            .filter_map(|v| v.as_object().cloned())
            .collect();
        // Read-only remote calendars are not stored: project them from their live snapshot.
        let in_range = |e: &Map<String, Value>| {
            let t = |k: &str| e.get(k).and_then(Value::as_str).and_then(|v| DateTime::parse_from_rfc3339(v).ok());
            e.get("recurring").and_then(Value::as_bool).unwrap_or(false)
                || matches!((t("starts_at"), t("ends_at")), (Some(s), Some(en)) if s < to && en > from)
        };
        fallback.extend(
            remote_dav::event_rows(&s.db, &auth.user_id, selected.as_deref()).await.into_iter().filter(in_range),
        );
    }
    let mut contact_events: Vec<Value> = Vec::new();
    if include_contacts {
        let (relation, live) = remote_dav::contacts_relation(&s.db, &auth.user_id, 2).await;
        let contacts = s
            .db
            .json_rows(
                &format!(
                    "SELECT id, display_name, primary_email, birthday, anniversary, contact_dates FROM {relation} c WHERE user_id = $1 AND (birthday IS NOT NULL \
                     OR anniversary IS NOT NULL OR (jsonb_typeof(contact_dates) = 'array' AND jsonb_array_length(contact_dates) > 0))"
                ),
                &[Val::from(&auth.user_id), live],
            )
            .await?;
        let a = contact_appearance(&s, &auth.user_id).await?;
        for (_, mut e) in cal::contact_date_events(&contacts, from, to) {
            if let Some(n) = truthy_str(&a, "name") {
                e.insert("calendar_name".into(), n);
            }
            e.insert("calendar_custom_name".into(), Value::Bool(truthy_str(&a, "name").is_some()));
            if let Some(c) = truthy_str(&a, "color") {
                e.insert("calendar_color".into(), c);
            }
            contact_events.push(Value::Object(e));
        }
    }
    let projection = cal::project_resources(fallback, from, to, Some(&auth.user_id), true).await;
    // Each attendee's answer (as the projection adds for unmaterialized rows), parsed once per
    // series from the stored resource — only resources carrying a PARTSTAT ship it at all.
    let mut answers: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for row in materialized.iter_mut().filter_map(Value::as_object_mut) {
        let raw = row.shift_remove("partstat_ical");
        let partstats = match raw.as_ref().and_then(Value::as_str) {
            Some(raw) => {
                let key = row
                    .get("series_id")
                    .filter(|v| !v.is_null())
                    .or(row.get("id"))
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                answers
                    .entry(key)
                    .or_insert_with(|| {
                        Value::Object(
                            inboxora_pim::ical::attendee_statuses(raw)
                                .into_iter()
                                .map(|a| (a.email, Value::String(a.partstat)))
                                .collect(),
                        )
                    })
                    .clone()
            }
            None => json!({}),
        };
        row.insert("partstats".into(), partstats);
    }
    let mut events: Vec<Value> = materialized;
    events.extend(projection.events.into_iter().map(|e| Value::Object(e.fields)));
    events.extend(contact_events);
    let mut keyed: Vec<(Option<DateTime<Utc>>, Value)> = events.into_iter().map(|e| (start_of(&e), e)).collect();
    keyed.sort_by(|a, b| by_start(&a.0, &b.0));
    let events: Vec<Value> = keyed.into_iter().map(|(_, e)| e).collect();
    if projection.truncated {
        for f in &projection.failures {
            tracing::warn!(
                "Calendar projection incomplete: {}",
                json!({ "userId": auth.user_id, "seriesId": f.id, "reason": f.reason })
            );
        }
        let incomplete: Vec<Value> =
            projection.failures.iter().map(|f| json!({ "series_id": f.id, "reason": f.reason })).collect();
        return Ok(ok(json!({ "events": events, "truncated": true, "incompleteSeries": incomplete })));
    }
    Ok(ok(json!({ "events": events, "truncated": false })))
}

static SEARCH_BUCKETS: LazyLock<crate::http::limit::Windows> =
    LazyLock::new(|| crate::http::limit::Windows::new(10_000));

fn search_limited(user: &str) -> Option<Response> {
    let (hits, left) = SEARCH_BUCKETS.hit(user, Duration::from_secs(60));
    (hits > 20).then(|| {
        let mut r = json(429, json!({ "error": "Too many search requests. Try again shortly." }));
        if let Ok(v) = HeaderValue::from_str(&(left.as_secs_f64().ceil() as u64).to_string()) {
            r.headers_mut().insert("retry-after", v);
        }
        r
    })
}

async fn search_events(State(s): State<AppState>, auth: Auth, q: Qs) -> Result<Response> {
    if let Some(r) = search_limited(&auth.user_id) {
        return Ok(r);
    }
    let query = query_string(&q, "q").unwrap_or_default();
    let query = inboxora_mail::text::js_trim(&query).to_owned();
    if query.is_empty() {
        return Ok(ok(json!({ "events": [] })));
    }
    if inboxora_mail::text::js_len(&query) > 200 {
        return Ok(err(400, "Search query is too long"));
    }
    let limit =
        query_string(&q, "limit").and_then(|v| crate::http::query::parse_int(&v)).unwrap_or(200).clamp(1, 500) as usize;
    let mut rows: Vec<Map<String, Value>> = s
        .db
        .json_rows(
            &format!(
                "SELECT {EVENT_COLUMNS}, {SOURCE_COLUMNS} FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id {SOURCE_JOINS} \
                 WHERE e.user_id = $1 AND c.user_id = $1 AND c.owner_user_id = $1 AND (e.summary ILIKE $2 OR e.description ILIKE $2 OR e.location ILIKE $2)"
            ),
            params![&auth.user_id, format!("%{query}%")],
        )
        .await?
        .into_iter()
        .filter_map(|v| v.as_object().cloned())
        .collect();
    let needle = query.to_lowercase();
    let matches = |e: &Map<String, Value>| {
        ["summary", "description", "location"]
            .iter()
            .any(|k| e.get(*k).and_then(Value::as_str).is_some_and(|v| v.to_lowercase().contains(&needle)))
    };
    rows.extend(remote_dav::event_rows(&s.db, &auth.user_id, None).await.into_iter().filter(matches));
    if rows.is_empty() {
        return Ok(ok(json!({ "events": [] })));
    }
    let year = Utc::now().year();
    let from = Utc.with_ymd_and_hms(year - 100, 1, 1, 0, 0, 0).single().unwrap_or_default();
    let to = Utc.with_ymd_and_hms(year + 100, 1, 1, 0, 0, 0).single().unwrap_or_default();
    let projection = cal::project_resources(rows, from, to, Some(&auth.user_id), true).await;
    let mut keyed: Vec<(DateTime<Utc>, Value)> =
        projection.events.into_iter().map(|e| (e.starts_at, Value::Object(e.fields))).collect();
    keyed.sort_by(|a, b| b.0.cmp(&a.0));
    let events: Vec<Value> = keyed.into_iter().take(limit).map(|(_, e)| e).collect();
    Ok(ok(json!({ "events": events, "truncated": projection.truncated })))
}
