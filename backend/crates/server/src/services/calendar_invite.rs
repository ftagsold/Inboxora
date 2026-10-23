//! Outgoing calendar invitations: iCalendar rendering for local events, invitations and
//! replies, SMTP delivery, and the retrying outbox (ports of `services/calendarInvitation.ts`,
//! `services/calendarInvitationOutbox.ts` and the iCalendar helpers of `routes/calendar.ts`).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use inboxora_core::pg::{Db, Q};
use inboxora_core::{Result, params};
use inboxora_mail::util::description_content_lines;
use regex::Regex;
use serde_json::{Map, Value, json};

use super::mail_compose::{Compose, IcalEvent, account_transport};

const MAX_ATTEMPTS: i64 = 5;

// ── iCalendar text ──────────────────────────────────────────────────────────────────────────

pub fn escape_text(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\\n")
        .replace(';', "\\;")
        .replace(',', "\\,")
}

/// `20260102T030405Z`, or `20260102` for an all-day value.
pub fn format_date(t: &DateTime<Utc>, all_day: bool) -> String {
    if all_day { t.format("%Y%m%d").to_string() } else { t.format("%Y%m%dT%H%M%SZ").to_string() }
}

/// Fold a content line at 75 octets (74 after the first), never splitting a character.
pub fn fold(line: &str) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let mut chunk = String::new();
    let mut limit = 75;
    for c in line.chars() {
        if chunk.len() + c.len_utf8() > limit && !chunk.is_empty() {
            chunks.push(std::mem::take(&mut chunk));
            limit = 74;
        }
        chunk.push(c);
    }
    chunks.push(chunk);
    chunks.join("\r\n ")
}

fn finish(lines: Vec<String>) -> String {
    lines.iter().map(|l| fold(l)).collect::<Vec<_>>().join("\r\n")
}

fn opt(v: &Option<String>) -> Option<&str> {
    v.as_deref().filter(|s| !s.is_empty())
}

/// The fields a local event's VEVENT is rendered from.
#[derive(Debug, Clone, Default)]
pub struct LocalEvent {
    pub uid: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub url: Option<String>,
    pub organizer: Option<String>,
    pub attendees: Vec<String>,
    /// Each attendee's recorded answer, keyed by lower-cased address.
    pub partstats: HashMap<String, String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub all_day: bool,
}

/// `localEventIcal`.
pub fn local_event_ical(e: &LocalEvent) -> String {
    let dp = if e.all_day { ";VALUE=DATE" } else { "" };
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".into(),
        "PRODID:-//Inboxora//DAV Hub//EN".into(),
        "BEGIN:VEVENT".into(),
        format!("UID:{}", e.uid),
        format!("DTSTAMP:{}", format_date(&Utc::now(), false)),
        format!("DTSTART{dp}:{}", format_date(&e.starts_at, e.all_day)),
        format!("DTEND{dp}:{}", format_date(&e.ends_at, e.all_day)),
    ];
    if let Some(s) = opt(&e.summary) {
        lines.push(format!("SUMMARY:{}", escape_text(s)));
    }
    lines.extend(description_content_lines(e.description.as_deref(), escape_text));
    if let Some(l) = opt(&e.location) {
        lines.push(format!("LOCATION:{}", escape_text(l)));
    }
    if let Some(u) = opt(&e.url) {
        lines.push(format!("URL:{}", u.replace(['\r', '\n'], "")));
    }
    if let Some(o) = opt(&e.organizer) {
        static MAILTO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^mailto:").expect("regex"));
        lines.push(format!("ORGANIZER:mailto:{}", escape_text(&MAILTO.replace(o, ""))));
    }
    for email in &e.attendees {
        match e.partstats.get(&email.to_lowercase()) {
            Some(p) => lines.push(format!("ATTENDEE;PARTSTAT={p}:mailto:{email}")),
            None => lines.push(format!("ATTENDEE:mailto:{email}")),
        }
    }
    lines.extend(["END:VEVENT".into(), "END:VCALENDAR".into(), String::new()]);
    finish(lines)
}

/// Exchange treats VALUE=DATE events as all-day only with these hints.
fn all_day_hints(all_day: bool) -> Vec<String> {
    if all_day {
        vec!["X-MICROSOFT-CDO-ALLDAYEVENT:TRUE".into(), "X-MICROSOFT-MSNCALENDAR-ALLDAYEVENT:TRUE".into()]
    } else {
        vec![]
    }
}

/// One invitation to render and send (an outbox action without its account row).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Invite {
    pub account_id: Option<String>,
    pub attendees: Vec<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub uid: String,
    pub all_day: bool,
    pub method: String,
    pub sequence: i64,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    /// The ATTENDEE list the invitation shows (every attendee); empty means `attendees`.
    pub guests: Vec<String>,
    /// Hide the guest list: each recipient gets an invitation naming only them.
    pub hide_attendees: bool,
    /// The event's IANA zone: timed events are stated in it, with its VTIMEZONE.
    pub timezone: Option<String>,
}

impl Invite {
    /// `invitationActionsForStorage`: the payload form (account id only, never credentials).
    pub fn to_json(&self) -> Value {
        json!({
            "attendees": self.attendees, "summary": self.summary, "description": self.description, "location": self.location,
            "uid": self.uid, "allDay": self.all_day, "method": self.method, "sequence": self.sequence,
            "startsAt": inboxora_core::json::js_iso(&self.starts_at), "endsAt": inboxora_core::json::js_iso(&self.ends_at),
            "accountId": self.account_id, "guests": self.guests, "hideAttendees": self.hide_attendees,
            "timezone": self.timezone,
        })
    }

    fn from_json(v: &Value, fallback_account: Option<&str>) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_owned);
        let date = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
                .map(|d| d.with_timezone(&Utc))
        };
        Some(Invite {
            account_id: s("accountId").or_else(|| fallback_account.map(str::to_owned)),
            attendees: v.get("attendees")?.as_array()?.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
            summary: s("summary"),
            description: s("description"),
            location: s("location"),
            uid: s("uid")?,
            all_day: v.get("allDay").and_then(Value::as_bool).unwrap_or(false),
            method: s("method").unwrap_or_else(|| "REQUEST".into()),
            sequence: v.get("sequence").and_then(Value::as_i64).unwrap_or(0),
            starts_at: date("startsAt")?,
            ends_at: date("endsAt")?,
            guests: v
                .get("guests")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
            hide_attendees: v.get("hideAttendees").and_then(Value::as_bool).unwrap_or(false),
            timezone: s("timezone"),
        })
    }

    /// The invitations one send consists of: one per recipient when the guest list is hidden.
    fn deliveries(&self) -> Vec<Invite> {
        if !self.hide_attendees {
            return vec![self.clone()];
        }
        self.attendees
            .iter()
            .map(|a| Invite { attendees: vec![a.clone()], guests: vec![a.clone()], ..self.clone() })
            .collect()
    }
}

/// `CN="…"` for a display name (quotes are not allowed inside a quoted parameter).
fn cn_param(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !matches!(c, '"' | '\r' | '\n')).collect();
    if clean.trim().is_empty() { String::new() } else { format!(";CN=\"{}\"", clean.trim()) }
}

/// `invitationIcal`: shaped like the invitations Outlook, Apple Calendar and Google send. A
/// timed event is stated in its own zone with that zone's VTIMEZONE; an all-day event uses
/// DATE values and carries the VTIMEZONE too, since Exchange reads date-only values in UTC
/// without one (the event then spills into the next day).
pub fn invitation_ical(i: &Invite, organizer_email: &str, organizer_name: &str) -> String {
    use chrono::Datelike;
    use inboxora_pim::ical::vtimezone;
    let zone = i.timezone.as_deref().and_then(|n| Some((n, vtimezone::zone(n)?, vtimezone::vtimezone(n, i.starts_at.year())?)));
    let when = |t: &DateTime<Utc>| match (&zone, i.all_day) {
        (_, true) => format!(";VALUE=DATE:{}", format_date(t, true)),
        (Some((name, tz, _)), false) => format!(";TZID={name}:{}", vtimezone::local_stamp(t, *tz)),
        (None, false) => format!(":{}", format_date(t, false)),
    };
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".into(),
        "PRODID:-//Inboxora//Calendar//EN".into(),
        "CALSCALE:GREGORIAN".into(),
        format!("METHOD:{}", i.method),
    ];
    if let Some((name, _, vtz)) = &zone {
        lines.push(format!("X-WR-TIMEZONE:{name}"));
        lines.extend(vtz.iter().cloned());
    }
    lines.extend([
        "BEGIN:VEVENT".into(),
        format!("UID:{}", i.uid),
        format!("SEQUENCE:{}", i.sequence),
        format!("DTSTAMP:{}", format_date(&Utc::now(), false)),
        format!("DTSTART{}", when(&i.starts_at)),
        format!("DTEND{}", when(&i.ends_at)),
        format!("ORGANIZER{}:mailto:{organizer_email}", cn_param(organizer_name)),
        format!("STATUS:{}", if i.method == "CANCEL" { "CANCELLED" } else { "CONFIRMED" }),
        "TRANSP:OPAQUE".into(),
    ]);
    lines.extend(all_day_hints(i.all_day));
    if let Some(s) = opt(&i.summary) {
        lines.push(format!("SUMMARY:{}", escape_text(s)));
    }
    lines.extend(description_content_lines(i.description.as_deref(), escape_text));
    if let Some(l) = opt(&i.location) {
        lines.push(format!("LOCATION:{}", escape_text(l)));
    }
    let guests = if i.guests.is_empty() { &i.attendees } else { &i.guests };
    for a in guests {
        lines.push(format!("ATTENDEE;CUTYPE=INDIVIDUAL;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:{a}"));
    }
    lines.extend(["END:VEVENT".into(), "END:VCALENDAR".into(), String::new()]);
    finish(lines)
}

fn sender_name(account: &Map<String, Value>, fallback: &str) -> String {
    ["sender_name", "name"]
        .iter()
        .find_map(|k| account.get(*k).and_then(Value::as_str).filter(|v| !v.is_empty()))
        .unwrap_or(fallback)
        .to_owned()
}

fn mailbox(name: &str, email: &str) -> String {
    if name.is_empty() {
        email.to_owned()
    } else {
        format!("\"{}\" <{email}>", name.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

async fn deliver(
    db: &Db,
    account: Map<String, Value>,
    compose: impl FnOnce(&Map<String, Value>) -> Compose,
) -> std::result::Result<(), String> {
    let (transport, account) = account_transport(db, account).await.map_err(|(_, e)| e)?;
    let built = compose(&account).build()?;
    transport.send_raw(&built.envelope_from, &built.recipients, &built.raw).await.map(|_| ()).map_err(|e| e.message)
}

/// `sendCalendarInvitation`: one message, or one per attendee when the guest list is hidden.
pub async fn send_invitation(db: &Db, account: Map<String, Value>, i: &Invite) -> std::result::Result<(), String> {
    let mut failed: Option<String> = None;
    for one in i.deliveries() {
        if let Err(e) = send_one(db, account.clone(), &one).await {
            failed.get_or_insert(e);
        }
    }
    failed.map_or(Ok(()), Err)
}

async fn send_one(db: &Db, account: Map<String, Value>, i: &Invite) -> std::result::Result<(), String> {
    deliver(db, account, |acc| {
        let from_email = acc.get("email_address").and_then(Value::as_str).unwrap_or("").to_owned();
        let name = sender_name(acc, &from_email);
        let what = opt(&i.summary).unwrap_or("Meeting");
        let ics = invitation_ical(i, &from_email, &name);
        let (subject, text) = if i.method == "CANCEL" {
            (format!("Canceled: {what}"), format!("{name} canceled {}.", opt(&i.summary).unwrap_or("a meeting")))
        } else {
            (format!("Invitation: {what}"), format!("{name} invited you to {}.", opt(&i.summary).unwrap_or("a meeting")))
        };
        Compose {
            from: mailbox(&name, &from_email),
            to: i.attendees.clone(),
            subject,
            text: Some(text),
            // iMIP: a text/calendar alternative plus the .ics attachment, as Outlook and Google send.
            ical: Some(IcalEvent { method: i.method.clone(), content: ics, filename: Some("invite.ics".into()) }),
            ..Default::default()
        }
    })
    .await
}

/// One answer to a received invitation (RFC 5546 METHOD:REPLY).
#[derive(Debug, Clone)]
pub struct ReplyOut {
    pub attendee: String,
    pub organizer: String,
    pub response: String,
    pub uid: String,
    pub recurrence_id: Option<String>,
    pub sequence: i64,
    pub summary: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub all_day: bool,
}

fn reply_word(response: &str) -> &'static str {
    match response {
        "ACCEPTED" => "Accepted",
        "TENTATIVE" => "Tentative",
        _ => "Declined",
    }
}

/// `invitationReplyIcal`.
pub fn reply_ical(r: &ReplyOut) -> String {
    let dp = if r.all_day { ";VALUE=DATE" } else { "" };
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".into(),
        "PRODID:-//Inboxora//Calendar//EN".into(),
        "METHOD:REPLY".into(),
        "BEGIN:VEVENT".into(),
        format!("UID:{}", r.uid),
        format!("SEQUENCE:{}", r.sequence),
        format!("DTSTAMP:{}", format_date(&Utc::now(), false)),
    ];
    if let Some(rid) = opt(&r.recurrence_id) {
        lines.push(format!("RECURRENCE-ID:{rid}"));
    }
    lines.extend([
        format!("DTSTART{dp}:{}", format_date(&r.starts_at, r.all_day)),
        format!("DTEND{dp}:{}", format_date(&r.ends_at, r.all_day)),
        format!("ORGANIZER:mailto:{}", r.organizer),
        format!("ATTENDEE;PARTSTAT={}:mailto:{}", r.response, r.attendee),
    ]);
    lines.extend(all_day_hints(r.all_day));
    if let Some(s) = opt(&r.summary) {
        lines.push(format!("SUMMARY:{}", escape_text(s)));
    }
    lines.extend(["END:VEVENT".into(), "END:VCALENDAR".into(), String::new()]);
    finish(lines)
}

/// `sendCalendarReply`.
pub async fn send_reply(db: &Db, account: Map<String, Value>, r: &ReplyOut) -> std::result::Result<(), String> {
    deliver(db, account, |acc| {
        let name = sender_name(acc, &r.attendee);
        let word = reply_word(&r.response);
        let what = opt(&r.summary).unwrap_or("Meeting");
        Compose {
            from: mailbox(&name, &r.attendee),
            to: vec![r.organizer.clone()],
            subject: format!("{word}: {what}"),
            text: Some(format!("{name} {} the invitation \"{what}\".", word.to_lowercase())),
            ical: Some(IcalEvent { method: "REPLY".into(), content: reply_ical(r), filename: None }),
            ..Default::default()
        }
    })
    .await
}

// ── planning ────────────────────────────────────────────────────────────────────────────────

static ATTENDEE_BAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[\r\n\x00\s,;"<>]"#).expect("regex"));
static ATTENDEE_SHAPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^@]+@[^@]+\.[^@]+$").expect("regex"));

/// `normalizeAttendees`: trimmed, lower-cased, unique; `None` when any address is invalid.
pub fn normalize_attendees(v: &Value) -> Option<Vec<String>> {
    let list = v.as_array()?;
    let mut out: Vec<String> = Vec::new();
    for e in list {
        let a = e.as_str().map(|s| s.trim().to_lowercase()).unwrap_or_default();
        if a.is_empty() {
            continue;
        }
        if ATTENDEE_BAD.is_match(&a) || !ATTENDEE_SHAPE.is_match(&a) {
            return None;
        }
        if !out.contains(&a) {
            out.push(a);
        }
    }
    Some(out)
}

/// The stored event fields an edit is compared against.
#[derive(Debug, Clone, Default)]
pub struct Existing {
    pub attendees: Vec<String>,
    pub invite_account_id: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub all_day: bool,
    pub hide_attendees: bool,
}

pub struct Next<'a> {
    pub attendees: &'a [String],
    pub invite_account_id: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub description: Option<&'a str>,
    pub location: Option<&'a str>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub all_day: bool,
    pub hide_attendees: bool,
}

/// `invitationUpdatePlan`: whether the sequence moves on, and who hears about this edit.
pub fn update_plan(existing: &Existing, next: &Next<'_>) -> (bool, Vec<String>) {
    let previous: Vec<String> = existing.attendees.iter().map(|e| e.to_lowercase()).collect();
    let had = existing.invite_account_id.is_some() && !previous.is_empty();
    let Some(account) = next.invite_account_id else { return (had, vec![]) };
    if !had || existing.invite_account_id.as_deref() != Some(account) {
        return (true, next.attendees.to_vec());
    }
    let text = |v: Option<&str>| v.unwrap_or("").trim().to_owned();
    let changed = text(existing.summary.as_deref()) != text(next.summary)
        || text(existing.description.as_deref()) != text(next.description)
        || text(existing.location.as_deref()) != text(next.location)
        || existing.starts_at != Some(next.starts_at)
        || existing.ends_at != Some(next.ends_at)
        || existing.all_day != next.all_day
        || existing.hide_attendees != next.hide_attendees;
    if changed {
        return (true, next.attendees.to_vec());
    }
    let removed = previous.iter().any(|e| !next.attendees.contains(e));
    (removed, next.attendees.iter().filter(|e| !previous.contains(e)).cloned().collect())
}

/// `preservedPartstats`: answers kept across an edit unless the time changed.
pub fn preserved_partstats(
    raw: Option<&str>,
    existing: &Existing,
    starts: DateTime<Utc>,
    ends: DateTime<Utc>,
) -> HashMap<String, String> {
    if existing.starts_at != Some(starts) || existing.ends_at != Some(ends) {
        return HashMap::new();
    }
    inboxora_pim::ical::attendee_statuses(raw.unwrap_or(""))
        .into_iter()
        .filter(|a| a.partstat != "NEEDS-ACTION")
        .map(|a| (a.email, a.partstat))
        .collect()
}

// ── outbox ──────────────────────────────────────────────────────────────────────────────────

/// `invitationDeliveryError`.
pub fn delivery_error(status: &str, last_error: Option<&str>) -> Option<String> {
    if status == "sent" {
        return None;
    }
    let detail = last_error.filter(|e| !e.is_empty()).map(|e| format!(" ({e})")).unwrap_or_default();
    Some(format!(
        "The event was saved, but the invitation could not be sent{detail}. Use \"Retry save\" to send it again."
    ))
}

/// `{ status, lastError }` of one delivery.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub status: String,
    pub last_error: Option<String>,
}

impl Delivery {
    pub fn sent() -> Self {
        Delivery { status: "sent".into(), last_error: None }
    }
    pub fn failed(e: impl Into<String>) -> Self {
        Delivery { status: "failed".into(), last_error: Some(e.into()) }
    }
}

/// `invitationDeliveryResponse`.
pub fn delivery_response(event: Value, d: &Delivery) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("event".into(), event);
    m.insert("invitationStatus".into(), json!({ "status": d.status, "lastError": d.last_error }));
    if let Some(e) = delivery_error(&d.status, d.last_error.as_deref()) {
        m.insert("invitationError".into(), json!(e));
    }
    m
}

/// `resolveInvitationActions`: actions with their account rows (by id, for this user).
pub async fn resolve_actions(db: &Db, user: &str, actions: &[Invite]) -> Result<Vec<(Invite, Map<String, Value>)>> {
    let ids: Vec<String> =
        actions.iter().filter_map(|a| a.account_id.clone()).collect::<HashSet<_>>().into_iter().collect();
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let rows = db
        .json_rows("SELECT * FROM email_accounts WHERE user_id = $1 AND id = ANY($2::uuid[])", params![user, ids])
        .await?;
    Ok(actions
        .iter()
        .filter_map(|a| {
            let id = a.account_id.as_deref()?;
            let row = rows.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(id))?;
            Some((a.clone(), row.as_object()?.clone()))
        })
        .collect())
}

async fn mark_failed(db: &Db, outbox: &str, message: &str) {
    let msg: String = message.chars().take(2000).collect();
    let _ = db
        .exec(
            "UPDATE calendar_invitation_outbox SET status = 'failed', attempts = attempts + 1, last_error = $2, \
             next_attempt_at = NOW() + make_interval(secs => LEAST(3600, 30 * power(2, attempts))::int) WHERE id = $1",
            params![outbox, if msg.is_empty() { "delivery failed".to_owned() } else { msg }],
        )
        .await;
}

/// `deliverInvitationOutbox`: send every action; a failure is recorded, never raised.
pub async fn deliver_outbox(db: &Db, outbox: &str, actions: Vec<(Invite, Map<String, Value>)>) -> Delivery {
    if actions.is_empty() {
        let e = "no sender account is available for this invitation";
        mark_failed(db, outbox, e).await;
        return Delivery::failed(e);
    }
    for (invite, account) in actions {
        if let Err(e) = send_invitation(db, account, &invite).await {
            mark_failed(db, outbox, &e).await;
            tracing::error!("Calendar invitation delivery failed: {e}");
            return Delivery::failed(e);
        }
    }
    let _ = db
        .exec(
            "UPDATE calendar_invitation_outbox SET status = 'sent', attempts = attempts + 1, delivered_at = NOW(), last_error = NULL, next_attempt_at = NULL WHERE id = $1",
            params![outbox],
        )
        .await;
    Delivery::sent()
}

pub fn actions_from_payload(payload: &Value, fallback_account: Option<&str>) -> Vec<Invite> {
    payload
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| Invite::from_json(x, fallback_account)).collect())
        .unwrap_or_default()
}

/// `deliverStoredInvitation`: the explicit-retry path, resending from the stored payload.
pub async fn deliver_stored(
    db: &Db,
    user: &str,
    outbox: &str,
    payload: &Value,
    fallback_account: Option<&str>,
) -> Delivery {
    let actions = actions_from_payload(payload, fallback_account);
    match resolve_actions(db, user, &actions).await {
        Ok(resolved) => deliver_outbox(db, outbox, resolved).await,
        Err(e) => {
            mark_failed(db, outbox, &e.to_string()).await;
            Delivery::failed(e.to_string())
        }
    }
}

/// `drainPendingInvitations`.
pub async fn drain(db: &Db, limit: i64) -> Result<usize> {
    let rows = db
        .json_rows(
            "SELECT o.id, o.user_id, o.payload, e.invite_account_id FROM calendar_invitation_outbox o \
             LEFT JOIN calendar_events e ON e.id = o.event_id \
             WHERE o.status <> 'sent' AND o.attempts < $2 AND (o.next_attempt_at IS NULL OR o.next_attempt_at <= NOW()) \
             ORDER BY o.created_at ASC LIMIT $1",
            params![limit, MAX_ATTEMPTS],
        )
        .await?;
    let n = rows.len();
    for r in rows {
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let payload = r.get("payload").cloned().unwrap_or(Value::Null);
        let fallback = r.get("invite_account_id").and_then(Value::as_str);
        deliver_stored(db, &s("user_id"), &s("id"), &payload, fallback).await;
    }
    Ok(n)
}

/// `startCalendarInvitationOutboxWorker`: retry due invitations every minute.
pub fn start_outbox_worker(db: Db) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_secs(60));
        iv.tick().await;
        loop {
            iv.tick().await;
            if let Err(e) = drain(&db, 5).await {
                tracing::warn!("Calendar invitation retry failed: {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 4, h, 0, 0).unwrap()
    }

    #[test]
    fn folds_on_octets_without_splitting_characters() {
        let line = format!("SUMMARY:{}", "ä".repeat(60));
        let folded = fold(&line);
        assert!(folded.split("\r\n").all(|l| l.len() <= 75));
        assert_eq!(folded.replace("\r\n ", ""), line);
    }

    #[test]
    fn renders_local_event_and_invitation() {
        let e = LocalEvent {
            uid: "u1".into(),
            summary: Some("Lunch, team; ok".into()),
            attendees: vec!["a@b.co".into()],
            partstats: HashMap::from([("a@b.co".into(), "ACCEPTED".into())]),
            starts_at: at(10),
            ends_at: at(11),
            organizer: Some("MAILTO:me@x.co".into()),
            ..Default::default()
        };
        let ics = local_event_ical(&e);
        assert!(ics.contains("SUMMARY:Lunch\\, team\\; ok\r\n"));
        assert!(ics.contains("DTSTART:20260304T100000Z\r\n"));
        assert!(ics.contains("ORGANIZER:mailto:me@x.co\r\n"));
        assert!(ics.contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:a@b.co\r\n"));
        let inv = Invite {
            uid: "u1".into(),
            method: "CANCEL".into(),
            all_day: true,
            starts_at: at(0),
            ends_at: at(0) + chrono::Duration::days(1),
            attendees: vec!["a@b.co".into()],
            ..Default::default()
        };
        let ics = invitation_ical(&inv, "me@x.co", "");
        assert!(
            ics.contains("METHOD:CANCEL\r\n")
                && ics.contains("STATUS:CANCELLED\r\n")
                && ics.contains("DTEND;VALUE=DATE:20260305\r\n")
        );
        assert!(ics.contains("X-MICROSOFT-CDO-ALLDAYEVENT:TRUE"));
        assert!(ics.contains("ORGANIZER:mailto:me@x.co\r\n"));
        assert_eq!(Invite::from_json(&inv.to_json(), None), Some(inv));
    }

    #[test]
    fn invitations_carry_the_event_zone() {
        let inv = Invite {
            uid: "u2".into(),
            method: "REQUEST".into(),
            starts_at: at(10),
            ends_at: at(11),
            attendees: vec!["new@b.co".into()],
            guests: vec!["old@b.co".into(), "new@b.co".into()],
            timezone: Some("Europe/Berlin".into()),
            ..Default::default()
        };
        let ics = invitation_ical(&inv, "me@x.co", "Me \"Boss\"");
        assert!(ics.contains("BEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\n"));
        assert!(ics.contains("DTSTART;TZID=Europe/Berlin:20260304T110000\r\n"));
        assert!(ics.contains("ORGANIZER;CN=\"Me Boss\":mailto:me@x.co\r\n"));
        assert!(ics.contains("STATUS:CONFIRMED\r\n"));
        assert!(ics.contains("mailto:old@b.co") && ics.contains("mailto:new@b.co"));
        let all_day = Invite { all_day: true, ends_at: at(10) + chrono::Duration::days(1), ..inv.clone() };
        let ics = invitation_ical(&all_day, "me@x.co", "");
        assert!(ics.contains("BEGIN:VTIMEZONE") && ics.contains("DTSTART;VALUE=DATE:20260304\r\n"));
        assert_eq!(Invite::from_json(&inv.to_json(), None), Some(inv));
    }

    #[test]
    fn hidden_guest_lists_send_one_invitation_per_attendee() {
        let inv = Invite {
            uid: "u3".into(),
            attendees: vec!["a@b.co".into(), "c@d.co".into()],
            guests: vec!["a@b.co".into(), "c@d.co".into()],
            hide_attendees: true,
            ..Default::default()
        };
        let each = inv.deliveries();
        assert_eq!(each.len(), 2);
        assert_eq!(each[1].attendees, vec!["c@d.co".to_owned()]);
        assert_eq!(each[1].guests, vec!["c@d.co".to_owned()]);
        assert_eq!(Invite { hide_attendees: false, ..inv }.deliveries().len(), 1);
    }

    #[test]
    fn plans_updates() {
        let existing = Existing {
            attendees: vec!["a@b.co".into(), "c@d.co".into()],
            invite_account_id: Some("acc".into()),
            summary: Some("S".into()),
            starts_at: Some(at(10)),
            ends_at: Some(at(11)),
            ..Default::default()
        };
        let next = |att: &'static [&'static str], summary: &'static str| {
            (att.iter().map(|s| s.to_string()).collect::<Vec<_>>(), summary)
        };
        let (att, summary) = next(&["a@b.co", "e@f.co"], "S");
        let plan = update_plan(
            &existing,
            &Next {
                attendees: &att,
                invite_account_id: Some("acc"),
                summary: Some(summary),
                description: None,
                location: None,
                starts_at: at(10),
                ends_at: at(11),
                all_day: false,
                hide_attendees: false,
            },
        );
        assert_eq!(plan, (true, vec!["e@f.co".to_owned()]));
        let (att, summary) = next(&["a@b.co", "c@d.co"], "Changed");
        let plan = update_plan(
            &existing,
            &Next {
                attendees: &att,
                invite_account_id: Some("acc"),
                summary: Some(summary),
                description: None,
                location: None,
                starts_at: at(10),
                ends_at: at(11),
                all_day: false,
                hide_attendees: false,
            },
        );
        assert_eq!(plan, (true, att.clone()));
        let plan = update_plan(
            &existing,
            &Next {
                attendees: &att,
                invite_account_id: None,
                summary: None,
                description: None,
                location: None,
                starts_at: at(10),
                ends_at: at(11),
                all_day: false,
                hide_attendees: false,
            },
        );
        assert_eq!(plan, (true, vec![]));
        assert_eq!(normalize_attendees(&json!([" A@B.co ", "a@b.co", ""])), Some(vec!["a@b.co".to_owned()]));
        assert_eq!(normalize_attendees(&json!(["a b@c.de"])), None);
        assert_eq!(normalize_attendees(&json!("x")), None);
    }
}
