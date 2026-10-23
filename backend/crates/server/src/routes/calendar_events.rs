//! `/api/calendar` event mutations and received invitations (the write half of
//! `routes/calendar.ts`): create/update/delete with invitation delivery through the outbox,
//! single/following occurrence edits, and adding, answering or removing mailed invitations.

use std::sync::LazyLock;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{any, get, patch, post};
use chrono::{DateTime, Utc};
use inboxora_core::pg::{Db, Q, Tx};
use inboxora_core::{Error, Result, crypto, params};
use inboxora_mail::util::normalize_description;
use inboxora_pim::ical::component::{parse_component, stringify_component};
use inboxora_pim::ical::{ParsedEvent, merge_calendar_resource, parse_calendar_event, truncate_series_before};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::http::{Auth, Body, Qs};
use crate::routes::search::js_date_parse;
use crate::routes::util::{empty, err, json as respond, ok};
use crate::services::calendar_invite::{
    Delivery, Existing, Invite, LocalEvent, Next, ReplyOut, deliver_outbox, deliver_stored, delivery_response,
    local_event_ical, normalize_attendees, preserved_partstats, resolve_actions, send_invitation, send_reply,
    update_plan,
};
use crate::services::dav_sync::{Action, Outcome, WritableCalendar, push_event_to_remote};
use crate::services::invitations::{Invitation, apply_invitation_reply, parse_inbound_invitation, parse_inbound_reply};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/events", post(create_event))
        .route("/events/{event_id}", patch(update_event).delete(delete_event))
        .route("/events/{event_id}/occurrence", any(occurrence))
        .route("/invitations/{message_id}", get(read_invitation).post(add_invitation).delete(remove_invitation))
        .route("/invitations/{message_id}/respond", post(respond_invitation))
}

const RETURNING: &str = "id, calendar_id, uid, etag, summary, description, location, url, organizer, starts_at, ends_at, all_day, timezone, attendees, invite_account_id, invite_hide_attendees, invitation_sequence, created_at, updated_at";
const READ_ATTENDEES: &str =
    "CASE WHEN jsonb_typeof(attendees) = 'array' THEN attendees ELSE '[]'::jsonb END AS attendees";
const BUMP_SEQUENCE: &str = "CASE WHEN NOT $16::boolean THEN invitation_sequence WHEN (invite_account_id IS NOT NULL AND jsonb_typeof(attendees) = 'array' AND jsonb_array_length(attendees) > 0) OR invitation_sequence > 0 THEN invitation_sequence + 1 ELSE 0 END";
const SAVE_FAILED: &str = "The event and invitation could not be saved; no partial changes were kept.";
const CANCEL_FAILED_UPDATE: &str = "The previous invitation could not be cancelled, so the event was not changed.";

// ── shared ──────────────────────────────────────────────────────────────────────────────────

/// `writableCalendar`: an owned, writable calendar, or the 404/403 response.
async fn writable(db: &impl Q, user: &str, calendar: &str) -> Result<std::result::Result<WritableCalendar, Response>> {
    let Some(row) = db
        .opt(
            "SELECT id, user_id, source, read_only, external_url FROM calendars WHERE id = $1 AND user_id = $2 AND owner_user_id = $2",
            params![calendar, user],
        )
        .await?
    else {
        return Ok(Err(err(404, "Calendar not found")));
    };
    let cal = WritableCalendar::from_row(&row);
    if cal.read_only || !["local", "caldav"].contains(&cal.source.as_str()) {
        return Ok(Err(err(403, "This calendar is read-only")));
    }
    Ok(Ok(cal))
}

/// `pushIfExternal`: mirror a committed change to a writable CalDAV calendar.
async fn push_if_external(
    db: &Db,
    cal: &WritableCalendar,
    uid: &str,
    action: Action,
    raw: Option<&str>,
    remote: Option<(Option<String>, Option<String>)>,
) -> Option<Value> {
    if cal.source != "caldav" {
        return None;
    }
    match push_event_to_remote(db, cal, uid, raw, action, remote).await {
        Outcome::Skipped => None,
        Outcome::Ok => Some(json!({ "ok": true })),
        Outcome::Conflict(m) => Some(json!({ "ok": false, "conflict": true, "message": m })),
        Outcome::Error(e) => Some(json!({ "ok": false, "conflict": false, "error": e })),
    }
}

fn with_remote(mut m: Map<String, Value>, remote: Option<Value>) -> Value {
    if let Some(r) = remote {
        m.insert("remoteSync".into(), r);
    }
    Value::Object(m)
}

/// `new Date(String(body.x ?? ''))`, valid only as a non-empty range.
fn parse_times(b: &Body) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let t = |k: &str| match b.get(k) {
        Some(Value::String(s)) => js_date_parse(s),
        _ => None,
    };
    let (s, e) = (t("startsAt")?, t("endsAt")?);
    (e > s).then_some((s, e))
}

fn text(b: &Body, k: &str) -> Option<String> {
    b.get(k).and_then(Value::as_str).map(str::to_owned)
}

fn truthy(v: Option<&Value>) -> bool {
    inboxora_core::json::truthy(v)
}

/// The request's event values, shared by create and update.
#[derive(Debug, Clone)]
struct EventInput {
    calendar_id: String,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    url: Option<String>,
    organizer: Option<String>,
    all_day: bool,
    timezone: Option<String>,
    attendees: Vec<String>,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    send_invites: bool,
    invite_account_id: Option<String>,
    hide_attendees: bool,
}

impl EventInput {
    fn local(&self, uid: &str, partstats: std::collections::HashMap<String, String>) -> LocalEvent {
        LocalEvent {
            uid: uid.to_owned(),
            summary: self.summary.clone(),
            description: self.description.clone(),
            location: self.location.clone(),
            url: self.url.clone(),
            organizer: self.organizer.clone(),
            attendees: self.attendees.clone(),
            partstats,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
            all_day: self.all_day,
        }
    }

    fn invite(&self, account: &str, uid: &str, method: &str, sequence: i64, attendees: Vec<String>) -> Invite {
        Invite {
            account_id: Some(account.to_owned()),
            attendees,
            summary: self.summary.clone(),
            description: self.description.clone(),
            location: self.location.clone(),
            uid: uid.to_owned(),
            all_day: self.all_day,
            method: method.into(),
            sequence,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
            guests: self.attendees.clone(),
            hide_attendees: self.hide_attendees,
            timezone: self.timezone.clone(),
        }
    }

    fn fingerprint(&self, event_id: Option<&str>) -> String {
        crypto::sha256_hex(
            json!({
                "eventId": event_id, "calendarId": self.calendar_id, "summary": self.summary.as_deref().filter(|s| !s.is_empty()),
                "description": self.description, "location": self.location, "url": self.url, "organizer": self.organizer,
                "allDay": self.all_day, "timezone": self.timezone, "attendees": self.attendees, "inviteAccountId": self.invite_account_id,
                "hideAttendees": self.hide_attendees, "startsAt": inboxora_core::json::js_iso(&self.starts_at), "endsAt": inboxora_core::json::js_iso(&self.ends_at),
            })
            .to_string(),
        )
    }

    /// The `$1..$12` values of an UPDATE (`raw` first).
    fn summary_or_null(&self) -> Option<String> {
        self.summary.clone().filter(|s| !s.is_empty())
    }
}

/// Parse and validate the create/update body; `Err` is the 400 response.
fn event_input(b: &Body) -> std::result::Result<EventInput, Response> {
    let times = parse_times(b);
    let calendar_id = b.get("calendarId").filter(|v| truthy(Some(v))).map(inboxora_mail::text::js_string);
    let (Some(calendar_id), Some((starts_at, ends_at))) = (calendar_id, times) else {
        return Err(err(400, "calendarId and a valid event range are required"));
    };
    let raw_attendees = b.get("attendees").filter(|v| truthy(Some(v))).cloned().unwrap_or_else(|| json!([]));
    let Some(attendees) = normalize_attendees(&raw_attendees) else {
        return Err(err(400, "Attendees must be valid email addresses"));
    };
    let send_invites = truthy(b.get("sendInvites"));
    let invite_account_id = b.get("inviteAccountId").filter(|v| truthy(Some(v))).map(inboxora_mail::text::js_string);
    if send_invites && (invite_account_id.is_none() || attendees.is_empty()) {
        return Err(err(400, "A sender account and at least one attendee are required for invitations"));
    }
    Ok(EventInput {
        calendar_id,
        summary: text(b, "summary"),
        description: text(b, "description").and_then(|d| normalize_description(&d)),
        location: text(b, "location"),
        url: text(b, "url"),
        organizer: text(b, "organizer"),
        all_day: truthy(b.get("allDay")),
        timezone: text(b, "timezone"),
        attendees,
        starts_at,
        ends_at,
        hide_attendees: send_invites && truthy(b.get("hideAttendees")),
        send_invites,
        invite_account_id,
    })
}

async fn sender_account(db: &impl Q, user: &str, id: &str, enabled_only: bool) -> Result<Option<Map<String, Value>>> {
    let sql = if enabled_only {
        "SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2 AND enabled = true AND smtp_host IS NOT NULL"
    } else {
        "SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2 AND smtp_host IS NOT NULL"
    };
    db.json_opt(sql, params![id, user]).await
}

fn idempotency_key(h: &HeaderMap) -> Option<String> {
    h.get("x-idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| k.chars().take(128).collect())
}

fn str_of(m: &Map<String, Value>, k: &str) -> Option<String> {
    m.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned)
}

fn date_of(m: &Map<String, Value>, k: &str) -> Option<DateTime<Utc>> {
    m.get(k).and_then(Value::as_str).and_then(|d| DateTime::parse_from_rfc3339(d).ok()).map(|d| d.with_timezone(&Utc))
}

fn existing_of(m: &Map<String, Value>) -> Existing {
    Existing {
        attendees: m
            .get("attendees")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default(),
        invite_account_id: str_of(m, "invite_account_id"),
        summary: m.get("summary").and_then(Value::as_str).map(str::to_owned),
        description: m.get("description").and_then(Value::as_str).map(str::to_owned),
        location: m.get("location").and_then(Value::as_str).map(str::to_owned),
        starts_at: date_of(m, "starts_at"),
        ends_at: date_of(m, "ends_at"),
        all_day: m.get("all_day").and_then(Value::as_bool).unwrap_or(false),
        hide_attendees: m.get("invite_hide_attendees").and_then(Value::as_bool).unwrap_or(false),
    }
}

/// A CANCEL for the attendees of a stored event (sequence + 1).
fn cancel_of(m: &Map<String, Value>, account: &str, attendees: Vec<String>) -> Option<Invite> {
    Some(Invite {
        account_id: Some(account.to_owned()),
        guests: attendees.clone(),
        hide_attendees: m.get("invite_hide_attendees").and_then(Value::as_bool).unwrap_or(false),
        timezone: str_of(m, "timezone"),
        attendees,
        summary: str_of(m, "summary"),
        description: str_of(m, "description"),
        location: str_of(m, "location"),
        uid: str_of(m, "uid")?,
        all_day: m.get("all_day").and_then(Value::as_bool).unwrap_or(false),
        method: "CANCEL".into(),
        sequence: m.get("invitation_sequence").and_then(Value::as_i64).unwrap_or(0) + 1,
        starts_at: date_of(m, "starts_at")?,
        ends_at: date_of(m, "ends_at")?,
    })
}

/// A transaction's outcome when its SMTP cancellation could not be sent (nothing written).
enum Guarded<T> {
    NotFound,
    CancelFailed,
    Done(T),
}

// ── create ──────────────────────────────────────────────────────────────────────────────────

enum Created {
    Conflict,
    Duplicate(Value, Delivery),
    Retry(Value, String, Value),
    New(Value, String, String),
}

async fn create_event(State(s): State<AppState>, a: Auth, headers: HeaderMap, b: Body) -> Result<Response> {
    let input = match event_input(&b) {
        Ok(i) => i,
        Err(r) => return Ok(r),
    };
    let user = a.user_id.clone();
    let cal = match writable(&s.db, &user, &input.calendar_id).await? {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let mut account: Option<Map<String, Value>> = None;
    if input.send_invites {
        account = sender_account(&s.db, &user, input.invite_account_id.as_deref().unwrap_or(""), true).await?;
        if account.is_none() {
            return Ok(err(400, "The selected sender account is unavailable"));
        }
    }

    if let (Some(acc), Some(key)) = (&account, idempotency_key(&headers)) {
        let account_id = str_of(acc, "id").unwrap_or_default();
        let fingerprint = input.fingerprint(None);
        let (u, i, k, f, aid) = (user.clone(), input.clone(), key.clone(), fingerprint.clone(), account_id.clone());
        let outcome = s
            .db
            .transaction(false, 2, move |tx| {
                let (user, input, key, fingerprint, account_id) = (u.clone(), i.clone(), k.clone(), f.clone(), aid.clone());
                Box::pin(async move {
                    if let Some(prior) = tx
                        .json_opt(
                            "SELECT id, event_id, request_fingerprint, status, last_error, payload FROM calendar_invitation_outbox WHERE user_id = $1 AND idempotency_key = $2 FOR UPDATE",
                            params![&user, &key],
                        )
                        .await?
                    {
                        if str_of(&prior, "request_fingerprint").as_deref() != Some(fingerprint.as_str()) {
                            return Ok(Created::Conflict);
                        }
                        let event = tx
                            .json_opt(&format!("SELECT {RETURNING} FROM calendar_events WHERE id = $1"), params![prior.get("event_id").and_then(Value::as_str)])
                            .await?
                            .map_or(Value::Null, Value::Object);
                        if str_of(&prior, "status").as_deref() == Some("sent") {
                            return Ok(Created::Duplicate(event, Delivery::sent()));
                        }
                        return Ok(Created::Retry(event, str_of(&prior, "id").unwrap_or_default(), prior.get("payload").cloned().unwrap_or(Value::Null)));
                    }
                    let uid = uuid::Uuid::new_v4().to_string();
                    let raw = local_event_ical(&input.local(&uid, Default::default()));
                    let event = insert_event(tx, &user, &input, &uid, &raw, Some(&account_id)).await?;
                    let sequence = event.get("invitation_sequence").and_then(Value::as_i64).unwrap_or(0);
                    let action = input.invite(&account_id, &uid, "REQUEST", sequence, input.attendees.clone());
                    let outbox: uuid::Uuid = tx
                        .opt(
                            "INSERT INTO calendar_invitation_outbox (user_id, event_id, idempotency_key, request_fingerprint, payload) VALUES ($1,$2::uuid,$3,$4,$5::jsonb) RETURNING id",
                            params![&user, event.get("id").and_then(Value::as_str), &key, &fingerprint, json!({ "actions": [action.to_json()] }).to_string()],
                        )
                        .await?
                        .ok_or_else(|| Error::internal("outbox insert returned no row"))?
                        .get("id");
                    Ok(Created::New(Value::Object(event), outbox.to_string(), raw))
                })
            })
            .await;
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                tracing::error!("Calendar invitation transaction failed: {e}");
                return Ok(err(500, SAVE_FAILED));
            }
        };
        return Ok(match outcome {
            Created::Conflict => err(409, "The idempotency key was already used for a different calendar operation"),
            Created::Duplicate(event, d) => respond(201, Value::Object(delivery_response(event, &d))),
            Created::Retry(event, outbox, payload) => {
                let fallback = event.get("invite_account_id").and_then(Value::as_str).map(str::to_owned);
                let d = deliver_stored(&s.db, &user, &outbox, &payload, fallback.as_deref()).await;
                respond(201, Value::Object(delivery_response(event, &d)))
            }
            Created::New(event, outbox, raw) => {
                let uid = event.get("uid").and_then(Value::as_str).unwrap_or("").to_owned();
                let remote = push_if_external(&s.db, &cal, &uid, Action::Put, Some(&raw), None).await;
                let sequence = event.get("invitation_sequence").and_then(Value::as_i64).unwrap_or(0);
                let action = input.invite(&account_id, &uid, "REQUEST", sequence, input.attendees.clone());
                let d = deliver_outbox(&s.db, &outbox, vec![(action, acc.clone())]).await;
                respond(201, with_remote(delivery_response(event, &d), remote))
            }
        });
    }

    let uid = uuid::Uuid::new_v4().to_string();
    let raw = local_event_ical(&input.local(&uid, Default::default()));
    let account_id = account.as_ref().and_then(|a| str_of(a, "id"));
    let event = insert_event(&s.db, &user, &input, &uid, &raw, account_id.as_deref()).await?;
    let mut out = Map::new();
    if let Some(acc) = account {
        let sequence = event.get("invitation_sequence").and_then(Value::as_i64).unwrap_or(0);
        let invite =
            input.invite(account_id.as_deref().unwrap_or(""), &uid, "REQUEST", sequence, input.attendees.clone());
        if let Err(e) = send_invitation(&s.db, acc, &invite).await {
            tracing::error!("Calendar invitation delivery failed: {e}");
            out.insert("invitationError".into(), json!("The event was saved, but the invitation could not be sent."));
        }
    }
    let remote = push_if_external(&s.db, &cal, &uid, Action::Put, Some(&raw), None).await;
    let mut m = Map::new();
    m.insert("event".into(), Value::Object(event));
    m.extend(out);
    Ok(respond(201, with_remote(m, remote)))
}

async fn insert_event(
    db: &impl Q,
    user: &str,
    i: &EventInput,
    uid: &str,
    raw: &str,
    account: Option<&str>,
) -> Result<Map<String, Value>> {
    db.json_opt(
        &format!(
            "INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, summary, description, location, url, organizer, starts_at, ends_at, all_day, timezone, attendees, invite_account_id, invite_hide_attendees) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14::jsonb,$15,$16) RETURNING {RETURNING}"
        ),
        params![
            &i.calendar_id,
            user,
            uid,
            raw,
            i.summary_or_null(),
            &i.description,
            &i.location,
            &i.url,
            &i.organizer,
            i.starts_at,
            i.ends_at,
            i.all_day,
            &i.timezone,
            json!(i.attendees).to_string(),
            account,
            i.hide_attendees
        ],
    )
    .await?
    .ok_or_else(|| Error::internal("event insert returned no row"))
}

/// The UPDATE shared by both edit paths; `$16` decides whether the sequence moves on.
async fn update_row(
    tx: &Tx<'_>,
    user: &str,
    event_id: &str,
    i: &EventInput,
    raw: &str,
    account: Option<&str>,
    bump: bool,
) -> Result<Option<Map<String, Value>>> {
    tx.json_opt(
        &format!(
            "UPDATE calendar_events SET raw_ical = $1, summary = $2, description = $3, location = $4, url = $5, organizer = $6, starts_at = $7, ends_at = $8, \
             all_day = $9, timezone = $10, attendees = $11::jsonb, invite_account_id = $12, invite_hide_attendees = $17, invitation_sequence = {BUMP_SEQUENCE}, \
             etag = gen_random_uuid()::text, updated_at = NOW() WHERE id = $13 AND calendar_id = $14 AND user_id = $15 RETURNING {RETURNING}"
        ),
        params![
            raw,
            i.summary_or_null(),
            &i.description,
            &i.location,
            &i.url,
            &i.organizer,
            i.starts_at,
            i.ends_at,
            i.all_day,
            &i.timezone,
            json!(i.attendees).to_string(),
            account,
            event_id,
            &i.calendar_id,
            user,
            bump,
            i.hide_attendees
        ],
    )
    .await
}

async fn lock_existing(
    tx: &Tx<'_>,
    user: &str,
    event_id: &str,
    calendar: &str,
    extra: &str,
) -> Result<Option<Map<String, Value>>> {
    tx.json_opt(
        &format!(
            "SELECT uid, raw_ical, {READ_ATTENDEES}, invite_account_id, invite_hide_attendees, invitation_sequence, summary, description, location, starts_at, ends_at, all_day, timezone{extra} \
             FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3 FOR UPDATE"
        ),
        params![event_id, calendar, user],
    )
    .await
}

// ── update ──────────────────────────────────────────────────────────────────────────────────

enum Updated {
    NotFound,
    Conflict,
    CancelFailed,
    Duplicate(Value, Delivery),
    Retry(Value, String, Value),
    Unchanged(Value, String, String),
    Queued(Value, String, String, String, Vec<Invite>),
}

async fn update_event(
    State(s): State<AppState>,
    a: Auth,
    Path(event_id): Path<String>,
    headers: HeaderMap,
    b: Body,
) -> Result<Response> {
    let input = match event_input(&b) {
        Ok(i) => i,
        Err(r) => return Ok(r),
    };
    let user = a.user_id.clone();
    let cal = match writable(&s.db, &user, &input.calendar_id).await? {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let mut account: Option<Map<String, Value>> = None;
    if input.send_invites {
        account = sender_account(&s.db, &user, input.invite_account_id.as_deref().unwrap_or(""), true).await?;
        if account.is_none() {
            return Ok(err(400, "The selected sender account is unavailable"));
        }
    }

    if let (Some(acc), Some(key)) = (&account, idempotency_key(&headers)) {
        let account_id = str_of(acc, "id").unwrap_or_default();
        let fingerprint = input.fingerprint(Some(&event_id));
        let (u, i, k, f, aid, eid) =
            (user.clone(), input.clone(), key.clone(), fingerprint.clone(), account_id.clone(), event_id.clone());
        let outcome =
            s.db.transaction(false, 2, move |tx| {
                let (user, input, key, fingerprint, account_id, event_id) =
                    (u.clone(), i.clone(), k.clone(), f.clone(), aid.clone(), eid.clone());
                Box::pin(
                    async move { update_invited(tx, &user, &event_id, &input, &key, &fingerprint, &account_id).await },
                )
            })
            .await;
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                tracing::error!("Calendar invitation transaction failed: {e}");
                return Ok(err(500, SAVE_FAILED));
            }
        };
        return Ok(match outcome {
            Updated::NotFound => err(404, "Event not found"),
            Updated::Conflict => err(409, "The idempotency key was already used for a different calendar update"),
            Updated::CancelFailed => err(502, CANCEL_FAILED_UPDATE),
            Updated::Duplicate(event, d) => ok(Value::Object(delivery_response(event, &d))),
            Updated::Retry(event, outbox, payload) => {
                let fallback = event.get("invite_account_id").and_then(Value::as_str).map(str::to_owned);
                let d = deliver_stored(&s.db, &user, &outbox, &payload, fallback.as_deref()).await;
                ok(Value::Object(delivery_response(event, &d)))
            }
            Updated::Unchanged(event, raw, uid) => {
                let remote = push_if_external(&s.db, &cal, &uid, Action::Put, Some(&raw), None).await;
                ok(with_remote(delivery_response(event, &Delivery::sent()), remote))
            }
            Updated::Queued(event, outbox, raw, uid, actions) => {
                let remote = push_if_external(&s.db, &cal, &uid, Action::Put, Some(&raw), None).await;
                let resolved = resolve_actions(&s.db, &user, &actions).await.unwrap_or_default();
                let d = deliver_outbox(&s.db, &outbox, resolved).await;
                ok(with_remote(delivery_response(event, &d), remote))
            }
        });
    }

    // Without an idempotency key the cancellation and REQUEST are sent while the row is
    // locked, so the transaction must not be retried.
    let (u, i, eid, acc) = (user.clone(), input.clone(), event_id.clone(), account.clone());
    let db = s.db.clone();
    let outcome =
        s.db.transaction(false, 0, move |tx| {
            let (user, input, event_id, account, db) = (u.clone(), i.clone(), eid.clone(), acc.clone(), db.clone());
            Box::pin(async move {
                let Some(existing) = lock_existing(tx, &user, &event_id, &input.calendar_id, "").await? else {
                    return Ok(Guarded::NotFound);
                };
                let ex = existing_of(&existing);
                let account_id = account.as_ref().and_then(|a| str_of(a, "id"));
                let had = ex.invite_account_id.is_some() && !ex.attendees.is_empty();
                let sender_changed = had && input.send_invites && account_id != ex.invite_account_id;
                let cancelled: Vec<String> = if !had {
                    vec![]
                } else if sender_changed || !input.send_invites {
                    ex.attendees.clone()
                } else {
                    ex.attendees.iter().filter(|e| !input.attendees.contains(e)).cloned().collect()
                };
                if !cancelled.is_empty() {
                    let previous = ex.invite_account_id.clone().unwrap_or_default();
                    let cancel_account = if account_id.as_deref() == Some(previous.as_str()) {
                        account.clone()
                    } else {
                        sender_account(tx, &user, &previous, false).await?
                    };
                    let cancel = cancel_of(&existing, &previous, cancelled);
                    let (Some(cancel_account), Some(cancel)) = (cancel_account, cancel) else {
                        return Ok(Guarded::CancelFailed);
                    };
                    if let Err(e) = send_invitation(&db, cancel_account, &cancel).await {
                        tracing::error!("Calendar invitation cancellation before update failed: {e}");
                        return Ok(Guarded::CancelFailed);
                    }
                }
                let (bump, recipients) = update_plan(&ex, &plan_next(&input, account_id.as_deref()));
                let uid = str_of(&existing, "uid").unwrap_or_default();
                let raw_existing = existing.get("raw_ical").and_then(Value::as_str);
                let partstats = preserved_partstats(raw_existing, &ex, input.starts_at, input.ends_at);
                let raw = merge_calendar_resource(
                    raw_existing,
                    &local_event_ical(&input.local(&uid, partstats)),
                    None,
                    false,
                )
                .map_err(Error::internal)?;
                let Some(event) = update_row(tx, &user, &event_id, &input, &raw, account_id.as_deref(), bump).await?
                else {
                    return Ok(Guarded::NotFound);
                };
                let mut delivered: Option<Delivery> = None;
                if let (Some(acc), false) = (&account, recipients.is_empty()) {
                    let sequence = event.get("invitation_sequence").and_then(Value::as_i64).unwrap_or(0);
                    let invite =
                        input.invite(account_id.as_deref().unwrap_or(""), &uid, "REQUEST", sequence, recipients);
                    delivered = Some(match send_invitation(&db, acc.clone(), &invite).await {
                        Ok(()) => Delivery::sent(),
                        Err(e) => {
                            tracing::error!("Calendar invitation delivery failed: {e}");
                            Delivery::failed(e)
                        }
                    });
                }
                Ok(Guarded::Done((event, delivered, raw, uid)))
            })
        })
        .await?;
    let (event, delivered, raw, uid) = match outcome {
        Guarded::Done(v) => v,
        Guarded::NotFound => return Ok(err(404, "Event not found")),
        Guarded::CancelFailed => return Ok(err(502, CANCEL_FAILED_UPDATE)),
    };
    let remote = push_if_external(&s.db, &cal, &uid, Action::Put, Some(&raw), None).await;
    Ok(ok(with_remote(delivery_response(Value::Object(event), &delivered.unwrap_or_else(Delivery::sent)), remote)))
}

fn plan_next<'a>(i: &'a EventInput, account: Option<&'a str>) -> Next<'a> {
    Next {
        attendees: &i.attendees,
        invite_account_id: account,
        summary: i.summary.as_deref(),
        description: i.description.as_deref(),
        location: i.location.as_deref(),
        starts_at: i.starts_at,
        ends_at: i.ends_at,
        all_day: i.all_day,
        hide_attendees: i.hide_attendees,
    }
}

/// `updateInvitedEvent`: the edit plus its CANCEL/REQUEST actions in the outbox, atomically.
async fn update_invited(
    tx: &Tx<'_>,
    user: &str,
    event_id: &str,
    input: &EventInput,
    key: &str,
    fingerprint: &str,
    account_id: &str,
) -> Result<Updated> {
    if let Some(prior) = tx
        .json_opt(
            "SELECT id, event_id, request_fingerprint, status, last_error, payload FROM calendar_invitation_outbox WHERE user_id = $1 AND idempotency_key = $2 FOR UPDATE",
            params![user, key],
        )
        .await?
    {
        if str_of(&prior, "request_fingerprint").as_deref() != Some(fingerprint) {
            return Ok(Updated::Conflict);
        }
        let event = tx
            .json_opt(
                &format!("SELECT {RETURNING} FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3"),
                params![prior.get("event_id").and_then(Value::as_str), &input.calendar_id, user],
            )
            .await?;
        let Some(event) = event.filter(|e| e.get("id").and_then(Value::as_str) == Some(event_id)) else {
            return Ok(Updated::Conflict);
        };
        if str_of(&prior, "status").as_deref() == Some("sent") {
            return Ok(Updated::Duplicate(Value::Object(event), Delivery::sent()));
        }
        return Ok(Updated::Retry(Value::Object(event), str_of(&prior, "id").unwrap_or_default(), prior.get("payload").cloned().unwrap_or(Value::Null)));
    }
    let Some(existing) = lock_existing(tx, user, event_id, &input.calendar_id, "").await? else {
        return Ok(Updated::NotFound);
    };
    let ex = existing_of(&existing);
    let had = ex.invite_account_id.is_some() && !ex.attendees.is_empty();
    let sender_changed = had && ex.invite_account_id.as_deref() != Some(account_id);
    let cancelled: Vec<String> = if !had {
        vec![]
    } else if sender_changed {
        ex.attendees.clone()
    } else {
        ex.attendees.iter().filter(|e| !input.attendees.contains(e)).cloned().collect()
    };
    let previous = ex.invite_account_id.clone().unwrap_or_default();
    if !cancelled.is_empty() && previous != account_id && sender_account(tx, user, &previous, false).await?.is_none() {
        return Ok(Updated::CancelFailed);
    }
    let (bump, recipients) = update_plan(&ex, &plan_next(input, Some(account_id)));
    let uid = str_of(&existing, "uid").unwrap_or_default();
    let raw_existing = existing.get("raw_ical").and_then(Value::as_str);
    let partstats = preserved_partstats(raw_existing, &ex, input.starts_at, input.ends_at);
    let raw = merge_calendar_resource(raw_existing, &local_event_ical(&input.local(&uid, partstats)), None, false)
        .map_err(Error::internal)?;
    let Some(event) = update_row(tx, user, event_id, input, &raw, Some(account_id), bump).await? else {
        return Ok(Updated::NotFound);
    };
    let mut actions: Vec<Invite> = Vec::new();
    if !cancelled.is_empty()
        && let Some(c) = cancel_of(&existing, &previous, cancelled)
    {
        actions.push(c);
    }
    if !recipients.is_empty() {
        let sequence = event.get("invitation_sequence").and_then(Value::as_i64).unwrap_or(0);
        actions.push(input.invite(account_id, &uid, "REQUEST", sequence, recipients));
    }
    if actions.is_empty() {
        return Ok(Updated::Unchanged(Value::Object(event), raw, uid));
    }
    let payload = json!({ "actions": actions.iter().map(Invite::to_json).collect::<Vec<_>>() });
    let outbox: uuid::Uuid = tx
        .opt(
            "INSERT INTO calendar_invitation_outbox (user_id, event_id, idempotency_key, request_fingerprint, payload) VALUES ($1,$2::uuid,$3,$4,$5::jsonb) RETURNING id",
            params![user, event.get("id").and_then(Value::as_str), key, fingerprint, payload.to_string()],
        )
        .await?
        .ok_or_else(|| Error::internal("outbox insert returned no row"))?
        .get("id");
    Ok(Updated::Queued(Value::Object(event), outbox.to_string(), raw, uid, actions))
}

// ── delete and occurrences ──────────────────────────────────────────────────────────────────

async fn delete_event(State(s): State<AppState>, a: Auth, Path(event_id): Path<String>, q: Qs) -> Result<Response> {
    let Some(calendar_id) = q.str("calendarId").map(str::to_owned) else {
        return Ok(err(400, "calendarId is required"));
    };
    let user = a.user_id.clone();
    let cal = match writable(&s.db, &user, &calendar_id).await? {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let (u, cid, eid, db) = (user.clone(), calendar_id.clone(), event_id.clone(), s.db.clone());
    let outcome =
        s.db.transaction(false, 0, move |tx| {
            let (user, calendar_id, event_id, db) = (u.clone(), cid.clone(), eid.clone(), db.clone());
            Box::pin(async move {
                let Some(event) =
                    lock_existing(tx, &user, &event_id, &calendar_id, ", remote_href, remote_etag").await?
                else {
                    return Ok(Guarded::NotFound);
                };
                let attendees = existing_of(&event).attendees;
                if let Some(account_id) = str_of(&event, "invite_account_id").filter(|_| !attendees.is_empty()) {
                    let Some(account) = sender_account(tx, &user, &account_id, false).await? else {
                        return Ok(Guarded::CancelFailed);
                    };
                    let Some(cancel) = cancel_of(&event, &account_id, attendees) else {
                        return Ok(Guarded::CancelFailed);
                    };
                    if let Err(e) = send_invitation(&db, account, &cancel).await {
                        tracing::error!("Calendar invitation cancellation before deletion failed: {e}");
                        return Ok(Guarded::CancelFailed);
                    }
                }
                let deleted = tx
                    .opt(
                        "DELETE FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3 RETURNING id",
                        params![&event_id, &calendar_id, &user],
                    )
                    .await?
                    .is_some();
                let href = str_of(&event, "remote_href").and_then(|h| crypto::decrypt(&h));
                if !deleted {
                    return Ok(Guarded::NotFound);
                }
                Ok(Guarded::Done((str_of(&event, "uid").unwrap_or_default(), href, str_of(&event, "remote_etag"))))
            })
        })
        .await?;
    let (uid, href, etag) = match outcome {
        Guarded::Done(v) => v,
        Guarded::NotFound => return Ok(err(404, "Event not found")),
        Guarded::CancelFailed => {
            return Ok(err(502, "The invitation could not be cancelled, so the event was not deleted."));
        }
    };
    push_if_external(&s.db, &cal, &uid, Action::Delete, None, Some((href, etag))).await;
    Ok(empty(204))
}

static RECURRENCE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}:\d{2}Z?)?$").expect("regex"));

enum Occurrence {
    Status(u16),
    Deleted(String, Option<String>, Option<String>),
    Saved(String, String),
}

fn parsed_local(e: &ParsedEvent) -> LocalEvent {
    LocalEvent {
        uid: e.uid.clone(),
        summary: e.summary.clone(),
        description: e.description.clone(),
        location: e.location.clone(),
        url: e.url.clone(),
        organizer: e.organizer.clone(),
        attendees: e.attendees.clone(),
        partstats: Default::default(),
        starts_at: e.starts_at,
        ends_at: e.ends_at,
        all_day: e.all_day,
    }
}

async fn occurrence(
    State(s): State<AppState>,
    a: Auth,
    method: axum::http::Method,
    Path(event_id): Path<String>,
    b: Body,
) -> Result<Response> {
    if method != axum::http::Method::PATCH && method != axum::http::Method::DELETE {
        return Ok(empty(405));
    }
    let calendar_id = b.get("calendarId").filter(|v| truthy(Some(v))).map(inboxora_mail::text::js_string);
    let recurrence = b.str("recurrenceId").filter(|r| RECURRENCE_ID.is_match(r)).map(str::to_owned);
    let (Some(calendar_id), Some(recurrence)) = (calendar_id, recurrence) else {
        return Ok(err(400, "A valid occurrence and calendar are required"));
    };
    let following = b.str("scope") == Some("following");
    let cancel = method == axum::http::Method::DELETE;
    if following && !cancel {
        return Ok(err(400, "Only a cancellation can affect following occurrences"));
    }
    let user = a.user_id.clone();
    let cal = match writable(&s.db, &user, &calendar_id).await? {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let times = if cancel { None } else { parse_times(&b) };
    let attendees =
        normalize_attendees(&b.get("attendees").filter(|v| truthy(Some(v))).cloned().unwrap_or_else(|| json!([])));
    if !cancel && (times.is_none() || attendees.is_none()) {
        return Ok(err(400, "Invalid event values"));
    }
    let replacement_input = times.map(|(starts_at, ends_at)| LocalEvent {
        uid: String::new(),
        summary: text(&b, "summary"),
        description: text(&b, "description").and_then(|d| normalize_description(&d)),
        location: text(&b, "location"),
        url: text(&b, "url"),
        organizer: text(&b, "organizer"),
        attendees: attendees.clone().unwrap_or_default(),
        partstats: Default::default(),
        starts_at,
        ends_at,
        all_day: truthy(b.get("allDay")),
    });
    let (u, cid, eid, rid) = (user.clone(), calendar_id.clone(), event_id.clone(), recurrence.clone());
    let outcome = s
        .db
        .transaction(false, 2, move |tx| {
            let (user, calendar_id, event_id, recurrence, replacement_input) = (u.clone(), cid.clone(), eid.clone(), rid.clone(), replacement_input.clone());
            Box::pin(async move {
                let Some(row) = tx
                    .json_opt(
                        "SELECT uid, raw_ical, invite_account_id, remote_href, remote_etag FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3 FOR UPDATE",
                        params![&event_id, &calendar_id, &user],
                    )
                    .await?
                else {
                    return Ok(Occurrence::Status(404));
                };
                let uid = str_of(&row, "uid").unwrap_or_default();
                let raw = row.get("raw_ical").and_then(Value::as_str).unwrap_or("").to_owned();
                if following {
                    let Some((truncated, now_empty)) = truncate_series_before(&raw, &recurrence) else { return Ok(Occurrence::Status(409)) };
                    if now_empty {
                        tx.exec("DELETE FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3", params![&event_id, &calendar_id, &user]).await?;
                        let href = str_of(&row, "remote_href").and_then(|h| crypto::decrypt(&h));
                        return Ok(Occurrence::Deleted(uid, href, str_of(&row, "remote_etag")));
                    }
                    tx.exec(
                        "UPDATE calendar_events SET raw_ical = $1, etag = gen_random_uuid()::text, updated_at = NOW() WHERE id = $2 AND calendar_id = $3 AND user_id = $4",
                        params![&truncated, &event_id, &calendar_id, &user],
                    )
                    .await?;
                    return Ok(Occurrence::Saved(uid, truncated));
                }
                let Some(event) = parse_calendar_event(&raw) else { return Ok(Occurrence::Status(409)) };
                let replacement = match (cancel, replacement_input) {
                    (false, Some(mut r)) => {
                        r.uid = uid.clone();
                        local_event_ical(&r)
                    }
                    _ => local_event_ical(&parsed_local(&event)),
                };
                let merged = merge_calendar_resource(Some(&raw), &replacement, Some(&recurrence), cancel).map_err(Error::internal)?;
                tx.exec(
                    "UPDATE calendar_events SET raw_ical = $1, etag = gen_random_uuid()::text, updated_at = NOW() WHERE id = $2 AND calendar_id = $3 AND user_id = $4",
                    params![&merged, &event_id, &calendar_id, &user],
                )
                .await?;
                Ok(Occurrence::Saved(uid, merged))
            })
        })
        .await?;
    let remote = match outcome {
        Occurrence::Status(code) => return Ok(err(code, "Calendar occurrence unavailable")),
        Occurrence::Deleted(uid, href, etag) => {
            push_if_external(&s.db, &cal, &uid, Action::Delete, None, Some((href, etag))).await
        }
        Occurrence::Saved(uid, raw) => push_if_external(&s.db, &cal, &uid, Action::Put, Some(&raw), None).await,
    };
    let mut m = Map::new();
    m.insert("updated".into(), json!(true));
    m.insert("scope".into(), json!(if following { "following" } else { "single" }));
    Ok(ok(with_remote(m, remote)))
}

// ── received invitations ────────────────────────────────────────────────────────────────────

static ICS_TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(text/calendar|application/(ics|ical|calendar))$").expect("regex"));
static ICS_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.ics$").expect("regex"));

/// The message's location and attachment list (from its cached body, fetched when needed).
async fn message_row(s: &AppState, message: &str, user: &str) -> Result<Option<Map<String, Value>>> {
    let Some(mut row) =
        s.db.json_opt(
            "SELECT m.id, m.account_id, m.uid, m.folder FROM messages m JOIN email_accounts a ON a.id = m.account_id \
             WHERE m.id = $1 AND a.user_id = $2",
            params![message, user],
        )
        .await?
    else {
        return Ok(None);
    };
    if let Some(loc) = crate::engine::cache::Location::from_row(&row) {
        match s.mail.message_body(&loc).await {
            Ok(b) => {
                row.insert("attachments".into(), b.attachments);
            }
            Err(e) => tracing::warn!("Invitation attachment list fetch failed: {e}"),
        }
    }
    Ok(Some(row))
}

/// `fetchInvitationAttachment`: the single .ics part of a message, read over IMAP.
async fn fetch_ics(s: &AppState, row: &Map<String, Value>, user: &str) -> Result<Option<String>> {
    let atts = crate::routes::mail::attachments_of(row.get("attachments"));
    let candidates: Vec<&Value> = atts
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| {
                    ICS_TYPE.is_match(x.get("type").and_then(Value::as_str).unwrap_or(""))
                        || ICS_NAME.is_match(x.get("filename").and_then(Value::as_str).unwrap_or(""))
                })
                .collect()
        })
        .unwrap_or_default();
    let [only] = candidates.as_slice() else { return Ok(None) };
    let size = match only.get("size") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(v)) => v.trim().parse().unwrap_or(f64::NAN),
        _ => 0.0,
    };
    if size > 1024.0 * 1024.0 {
        return Ok(None);
    }
    let account_id = row.get("account_id").and_then(Value::as_str).unwrap_or("");
    let Some(account) = crate::routes::mail::account_for_user(s, account_id, user).await? else { return Ok(None) };
    let uid = match row.get("uid") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(v)) => v.parse().unwrap_or(0),
        _ => 0,
    };
    let folder = row.get("folder").and_then(Value::as_str).unwrap_or("");
    let part = only.get("part").and_then(Value::as_str).unwrap_or("");
    match s.mail.fetch_attachment(&account, uid, folder, part).await {
        Ok(Some(bytes)) => {
            let raw = String::from_utf8_lossy(&bytes).into_owned();
            Ok((!raw.trim().is_empty()).then_some(raw))
        }
        Ok(None) => Ok(None),
        Err(e) => {
            tracing::warn!("Calendar invitation attachment fetch failed: {e}");
            Ok(None)
        }
    }
}

struct MessageInvitation {
    inv: Invitation,
    event: Option<ParsedEvent>,
    account_id: String,
}

/// `readMessageInvitation`: the captured invitation, else the raw MIME part.
async fn read_message_invitation(s: &AppState, message: &str, user: &str) -> Result<Option<MessageInvitation>> {
    let Some(row) = message_row(s, message, user).await? else { return Ok(None) };
    let account_id = row.get("account_id").and_then(Value::as_str).unwrap_or("").to_owned();
    let mut sources: Vec<String> =
        row.get("raw_ical").and_then(Value::as_str).filter(|r| !r.is_empty()).map(str::to_owned).into_iter().collect();
    for i in 0..2 {
        if i == 1 {
            sources = fetch_ics(s, &row, user).await?.into_iter().collect();
        }
        for raw in &sources {
            if let Some(inv) = parse_inbound_invitation(raw) {
                return Ok(Some(MessageInvitation { inv, event: parse_calendar_event(raw), account_id }));
            }
        }
    }
    Ok(None)
}

fn iso(t: &Option<DateTime<Utc>>) -> Value {
    t.as_ref().map_or(Value::Null, |t| json!(inboxora_core::json::js_iso(t)))
}

async fn imported_event(s: &AppState, message: &str, user: &str) -> Result<Option<Map<String, Value>>> {
    s.db.json_opt(
        "SELECT id, calendar_id, invitation_sequence, starts_at, ends_at, all_day FROM calendar_events WHERE source_message_id = $1 AND user_id = $2 ORDER BY created_at ASC LIMIT 1",
        params![message, user],
    )
    .await
}

fn organizer_address(v: Option<&str>) -> String {
    static MAILTO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^mailto:").expect("regex"));
    MAILTO.replace(v.unwrap_or(""), "").trim().to_owned()
}

async fn read_invitation(State(s): State<AppState>, a: Auth, Path(message): Path<String>) -> Result<Response> {
    let user = a.user_id.as_str();
    let Some(mi) = read_message_invitation(&s, &message, user).await? else {
        let Some(row) = message_row(&s, &message, user).await? else {
            return Ok(err(404, "Calendar invitation not found"));
        };
        let Some(reply) = fetch_ics(&s, &row, user).await?.and_then(|raw| parse_inbound_reply(&raw)) else {
            return Ok(err(404, "Calendar invitation not found"));
        };
        let updated = apply_invitation_reply(&s.db, user, &reply).await?;
        let recorded = updated > 0
            || s.db
                .opt("SELECT 1 FROM calendar_events WHERE user_id = $1 AND uid = $2 AND invite_account_id IS NOT NULL LIMIT 1", params![user, &reply.uid])
                .await?
                .is_some();
        return Ok(ok(json!({ "invitation": {
            "method": "REPLY", "uid": reply.uid, "recurrenceId": reply.recurrence_id, "sequence": reply.sequence, "summary": reply.summary,
            "startsAt": reply.starts_at, "endsAt": reply.ends_at, "allDay": reply.all_day,
            "attendees": reply.attendees.iter().map(|(e, n, p)| json!({ "email": e, "name": n, "partstat": p })).collect::<Vec<_>>(),
            "recordedInCalendar": recorded,
        } })));
    };
    let local = imported_event(&s, &message, user).await?.map(|e| {
        json!({ "id": e.get("id"), "calendarId": e.get("calendar_id"), "sequence": e.get("invitation_sequence").and_then(Value::as_i64).unwrap_or(0),
                "startsAt": e.get("starts_at"), "endsAt": e.get("ends_at"), "allDay": e.get("all_day") })
    });
    let i = &mi.inv;
    let response = s
        .db
        .opt(
            "SELECT response FROM calendar_invitation_responses WHERE user_id = $1 AND uid = $2 AND recurrence_id = $3 AND organizer = $4 AND sequence >= $5",
            params![user, &i.uid, &i.recurrence_id, organizer_address(i.organizer.as_deref()).to_lowercase(), i.sequence],
        )
        .await?
        .map(|r| r.get::<_, String>("response"));
    let mut out = Map::new();
    for (k, v) in [
        ("method", json!(i.method)),
        ("state", json!(i.state)),
        ("uid", json!(i.uid)),
        ("recurrenceId", json!(i.recurrence_id)),
        ("sequence", json!(i.sequence)),
        ("summary", json!(i.summary)),
        ("organizer", json!(i.organizer)),
        ("startsAt", iso(&i.starts_at)),
        ("endsAt", iso(&i.ends_at)),
        ("allDay", json!(i.all_day)),
        ("timeZone", json!(i.time_zone)),
        ("localEvent", local.unwrap_or(Value::Null)),
        ("response", json!(response)),
    ] {
        out.insert(k.into(), v);
    }
    if let Some(e) = &mi.event {
        out.insert("description".into(), json!(e.description));
        out.insert("location".into(), json!(e.location));
        out.insert("url".into(), json!(e.url));
    }
    out.insert("attendees".into(), json!(mi.event.as_ref().map(|e| e.attendees.clone()).unwrap_or_default()));
    Ok(ok(json!({ "invitation": out })))
}

async fn remove_invitation(State(s): State<AppState>, a: Auth, Path(message): Path<String>) -> Result<Response> {
    let user = a.user_id.as_str();
    let Some(mi) = read_message_invitation(&s, &message, user).await? else {
        return Ok(err(404, "Calendar invitation not found"));
    };
    let Some(local) = imported_event(&s, &message, user).await? else {
        return Ok(err(404, "This invitation was not added to a calendar"));
    };
    if mi.inv.sequence < local.get("invitation_sequence").and_then(Value::as_i64).unwrap_or(0) {
        return Ok(err(409, "This cancellation is older than the event already in the calendar"));
    }
    let removed = s
        .db
        .opt(
            "DELETE FROM calendar_events WHERE id = $1 AND user_id = $2 AND source_message_id = $3 AND invite_account_id IS NULL RETURNING id",
            params![local.get("id").and_then(Value::as_str), user, &message],
        )
        .await?;
    if removed.is_none() {
        return Ok(err(409, "This event can no longer be removed automatically"));
    }
    Ok(ok(json!({ "removed": true, "calendarId": local.get("calendar_id") })))
}

/// `importInvitationEvent`: add or refresh the local copy (with the user's PARTSTAT when given).
async fn import_invitation(
    s: &AppState,
    user: &str,
    message: &str,
    cal: &WritableCalendar,
    mi: &MessageInvitation,
    partstat: Option<(&str, &str)>,
) -> Result<bool> {
    let Some(event) = &mi.event else { return Err(Error::status(409, "This invitation cannot be added")) };
    let key = json!([mi.inv.uid, mi.inv.organizer, mi.inv.recurrence_id]).to_string();
    let uid = format!("mail-{}", crypto::sha256_hex(key));
    let mut root = parse_component(&event.raw).map_err(Error::internal)?;
    root.remove_all("method");
    if let Some(vevent) = root.comps.iter_mut().find(|c| c.name == "vevent") {
        vevent.update_text("uid", &uid);
        if let Some((email, response)) = partstat {
            for p in vevent.props.iter_mut().filter(|p| p.name == "attendee") {
                if organizer_address(p.first_str().as_deref()).to_lowercase() == email.to_lowercase() {
                    p.set_param("partstat", response);
                }
            }
        }
    }
    let raw = stringify_component(&root);
    let cmp = if partstat.is_some() { "<=" } else { "<" };
    let changed = s
        .db
        .opt(
            &format!(
                "INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at, all_day, timezone, description, location, url, organizer, attendees, invitation_sequence, source_message_id) \
                 VALUES ($1,$2,$3,$4,gen_random_uuid()::text,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14::jsonb,$15,$16) \
                 ON CONFLICT (calendar_id, uid, recurrence_id) DO UPDATE SET \
                   raw_ical = EXCLUDED.raw_ical, etag = gen_random_uuid()::text, summary = EXCLUDED.summary, \
                   starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, all_day = EXCLUDED.all_day, timezone = EXCLUDED.timezone, \
                   description = EXCLUDED.description, location = EXCLUDED.location, url = EXCLUDED.url, \
                   organizer = EXCLUDED.organizer, attendees = EXCLUDED.attendees, invitation_sequence = EXCLUDED.invitation_sequence, \
                   source_message_id = COALESCE(EXCLUDED.source_message_id, calendar_events.source_message_id), updated_at = NOW() \
                 WHERE calendar_events.invitation_sequence {cmp} EXCLUDED.invitation_sequence AND calendar_events.invite_account_id IS NULL \
                 RETURNING id"
            ),
            params![
                &cal.id,
                user,
                &uid,
                &raw,
                &event.summary,
                event.starts_at,
                event.ends_at,
                event.all_day,
                &event.time_zone,
                &event.description,
                &event.location,
                &event.url,
                &event.organizer,
                json!(event.attendees).to_string(),
                mi.inv.sequence,
                message
            ],
        )
        .await?
        .is_some();
    if changed {
        push_if_external(&s.db, cal, &uid, Action::Put, Some(&raw), None).await;
    }
    Ok(changed)
}

async fn add_invitation(State(s): State<AppState>, a: Auth, Path(message): Path<String>, b: Body) -> Result<Response> {
    let user = a.user_id.as_str();
    let Some(calendar_id) = b.get("calendarId").filter(|v| truthy(Some(v))).map(inboxora_mail::text::js_string) else {
        return Ok(err(400, "calendarId is required"));
    };
    let cal = match writable(&s.db, user, &calendar_id).await? {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let Some(mi) = read_message_invitation(&s, &message, user).await? else {
        return Ok(err(404, "Calendar invitation not found"));
    };
    if mi.inv.method != "REQUEST" || mi.event.is_none() {
        return Ok(err(409, "This invitation cannot be added"));
    }
    let changed = import_invitation(&s, user, &message, &cal, &mi, None).await?;
    Ok(ok(json!({ "added": true, "changed": changed, "calendarId": b.get("calendarId") })))
}

/// `invitedAddress`: the receiving account's address (or alias) that was invited.
async fn invited_address(s: &AppState, user: &str, account: &str, attendees: &[String]) -> Result<Option<String>> {
    let rows = s
        .db
        .rows(
            "SELECT email_address AS email, 'account' AS kind FROM email_accounts WHERE id = $1 AND user_id = $2 \
             UNION ALL SELECT al.email, 'alias' AS kind FROM account_aliases al JOIN email_accounts a ON a.id = al.account_id WHERE al.account_id = $1 AND a.user_id = $2",
            params![account, user],
        )
        .await?;
    let own: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get::<_, Option<String>>("email"))
        .map(|e| e.trim().to_owned())
        .filter(|e| !e.is_empty())
        .collect();
    let invited: Vec<String> = attendees.iter().map(|a| a.trim().to_lowercase()).collect();
    Ok(own.iter().find(|a| invited.contains(&a.to_lowercase())).or(own.first()).cloned())
}

async fn respond_invitation(
    State(s): State<AppState>,
    a: Auth,
    Path(message): Path<String>,
    b: Body,
) -> Result<Response> {
    let user = a.user_id.as_str();
    let response = b
        .get("response")
        .filter(|v| truthy(Some(v)))
        .map(|v| inboxora_mail::text::js_string(v).to_uppercase())
        .unwrap_or_default();
    if !["ACCEPTED", "TENTATIVE", "DECLINED"].contains(&response.as_str()) {
        return Ok(err(400, "response must be ACCEPTED, TENTATIVE or DECLINED"));
    }
    let Some(mi) = read_message_invitation(&s, &message, user).await? else {
        return Ok(err(404, "Calendar invitation not found"));
    };
    let Some(event) = mi.event.as_ref().filter(|_| mi.inv.method == "REQUEST") else {
        return Ok(err(409, "This invitation cannot be answered"));
    };
    // `notify: false` records the answer and updates the calendar without replying to the organizer.
    let notify = b.get("notify") != Some(&Value::Bool(false));
    let organizer = organizer_address(mi.inv.organizer.as_deref());
    if notify && !organizer.contains('@') {
        return Ok(err(409, "The invitation has no organizer address to reply to"));
    }
    let mut cal: Option<WritableCalendar> = None;
    if response != "DECLINED" {
        let existing = imported_event(&s, &message, user).await?;
        let calendar_id = b
            .get("calendarId")
            .filter(|v| truthy(Some(v)))
            .map(inboxora_mail::text::js_string)
            .or_else(|| existing.and_then(|e| e.get("calendar_id").and_then(Value::as_str).map(str::to_owned)));
        let Some(calendar_id) = calendar_id else { return Ok(err(400, "calendarId is required")) };
        cal = match writable(&s.db, user, &calendar_id).await? {
            Ok(c) => Some(c),
            Err(r) => return Ok(r),
        };
    }
    let attendee = invited_address(&s, user, &mi.account_id, &event.attendees).await?;
    let account =
        s.db.json_opt("SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2", params![&mi.account_id, user])
            .await?;
    let (Some(attendee), Some(account)) = (attendee, account) else {
        return Ok(err(404, "The receiving account was not found"));
    };
    let reply = ReplyOut {
        attendee: attendee.clone(),
        organizer: organizer.clone(),
        response: response.clone(),
        uid: mi.inv.uid.clone(),
        recurrence_id: Some(mi.inv.recurrence_id.clone()).filter(|r| !r.is_empty()),
        sequence: mi.inv.sequence,
        summary: mi.inv.summary.clone(),
        starts_at: event.starts_at,
        ends_at: event.ends_at,
        all_day: event.all_day,
    };
    if notify && let Err(e) = send_reply(&s.db, account, &reply).await {
        tracing::error!("Calendar invitation reply failed: {e}");
        return Ok(err(502, "Could not send your reply to the organizer"));
    }
    s.db.exec(
        "INSERT INTO calendar_invitation_responses (user_id, uid, recurrence_id, organizer, sequence, response) VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (user_id, uid, recurrence_id, organizer) DO UPDATE SET sequence = EXCLUDED.sequence, response = EXCLUDED.response, responded_at = NOW()",
        params![user, &mi.inv.uid, &mi.inv.recurrence_id, organizer.to_lowercase(), mi.inv.sequence, &response],
    )
    .await?;
    let local = match &cal {
        Some(c) => {
            import_invitation(&s, user, &message, c, &mi, Some((&attendee, &response))).await?;
            json!({ "calendarId": c.id })
        }
        None => {
            s.db.exec("DELETE FROM calendar_events WHERE user_id = $1 AND source_message_id = $2 AND invite_account_id IS NULL", params![user, &message]).await?;
            Value::Null
        }
    };
    Ok(ok(json!({ "response": response, "localEvent": local })))
}
