//! Calendar invitations received by mail: strict REQUEST/CANCEL parsing and persistence
//! (`inboundCalendarInvitation*.ts`), and attendee REPLY handling for invitations the user
//! sent (`calendarReplies.ts`).

use std::collections::HashMap;

use chrono::{DateTime, SecondsFormat, Utc};
use inboxora_core::params;
use inboxora_core::pg::{Db, Q};
use inboxora_imap::{Client, FetchQuery, Message};
use inboxora_pim::ical::component::{parse_component, stringify_component};
use inboxora_pim::ical::{
    LineProperty, local_date_in_time_zone, parse_ical_date, property_from_line, unescape_ical_text, zone_resolver,
};
use uuid::Uuid;

use crate::engine::mime::{Collected, decode_body, walk_structure};

const MAX_ICAL_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Invitation {
    pub method: String,
    pub state: &'static str,
    pub uid: String,
    pub recurrence_id: String,
    pub sequence: i64,
    pub summary: Option<String>,
    pub organizer: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub all_day: Option<bool>,
    pub time_zone: Option<String>,
    pub raw: String,
}

fn unfold(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    for physical in normalized.split('\n') {
        if (physical.starts_with(' ') || physical.starts_with('\t')) && !lines.is_empty() {
            let last = lines.len() - 1;
            lines[last].push_str(&physical[1..]);
        } else if !physical.is_empty() {
            lines.push(physical.to_owned());
        }
    }
    lines
}

fn marker(line: &str) -> Option<(String, String)> {
    let (kind, name) = line.split_once(':')?;
    let kind = kind.to_uppercase();
    if (kind == "BEGIN" || kind == "END")
        && !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Some((kind, name.to_uppercase()));
    }
    None
}

/// The VCALENDAR-level and single-VEVENT lines, or None for any other shape.
fn structure(lines: &[String]) -> Option<(Vec<&String>, Vec<&String>)> {
    let mut stack: Vec<String> = Vec::new();
    let (mut cal, mut ev) = (Vec::new(), Vec::new());
    let mut events = 0;
    for line in lines {
        if let Some((kind, name)) = marker(line) {
            if kind == "BEGIN" {
                if stack.is_empty() && name != "VCALENDAR" {
                    return None;
                }
                if stack.len() == 1 && name == "VEVENT" {
                    events += 1;
                }
                stack.push(name);
            } else if stack.pop().as_deref() != Some(name.as_str()) {
                return None;
            }
            continue;
        }
        if stack.len() == 1 && stack[0] == "VCALENDAR" {
            cal.push(line);
        } else if stack.len() == 2 && stack[0] == "VCALENDAR" && stack[1] == "VEVENT" {
            ev.push(line);
        }
    }
    (stack.is_empty() && events == 1).then_some((cal, ev))
}

#[derive(Debug, Clone, PartialEq)]
struct DateForm {
    date: DateTime<Utc>,
    all_day: bool,
    time_zone: Option<String>,
    form: &'static str,
}

fn parse_date(p: &LineProperty, raw: &str) -> Option<DateForm> {
    let value = p.value.as_str();
    let value_param = p.parameters.get("VALUE").map(|v| v.to_uppercase());
    let eight = value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit());
    if value_param.as_deref() == Some("DATE") || eight {
        if !eight {
            return None;
        }
        let d = parse_ical_date(value, &HashMap::new(), None)?;
        return Some(DateForm { date: d.date, all_day: true, time_zone: None, form: "date" });
    }
    let b = value.as_bytes();
    let shape = (b.len() == 15 || (b.len() == 16 && b[15] == b'Z'))
        && b[8] == b'T'
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[9..15].iter().all(u8::is_ascii_digit);
    if !shape || value_param.as_deref().is_some_and(|v| v != "DATE-TIME") {
        return None;
    }
    let n = |a: usize, z: usize| value[a..z].parse::<i64>().ok();
    let (y, mo, d, h, mi, s) = (n(0, 4)?, n(4, 6)?, n(6, 8)?, n(9, 11)?, n(11, 13)?, n(13, 15)?);
    if b.len() == 16 {
        if p.parameters.contains_key("TZID") {
            return None;
        }
        let date = chrono::NaiveDate::from_ymd_opt(y as i32, mo as u32, d as u32)?
            .and_hms_opt(h as u32, mi as u32, s as u32)?
            .and_utc();
        return Some(DateForm { date, all_day: false, time_zone: None, form: "utc" });
    }
    let tz = p.parameters.get("TZID")?.clone();
    let resolver = zone_resolver(raw);
    let date = match resolver(&tz) {
        Some(zone) => {
            let z = zone.clone();
            let f = move |_: &str| Some(z.clone());
            parse_ical_date(value, &p.parameters, Some(&f))?.date
        }
        None => local_date_in_time_zone(y, mo, d, h, mi, s, &tz)?,
    };
    Some(DateForm { date, all_day: false, time_zone: Some(tz), form: "timezone" })
}

fn iso_compact(d: &DateTime<Utc>) -> String {
    d.to_rfc3339_opts(SecondsFormat::Millis, true).replace(['-', ':'], "").replace(".000", "")
}

/// `parseInboundCalendarInvitation`: a strict single-event REQUEST or CANCEL, or None.
pub fn parse_inbound_invitation(raw: &str) -> Option<Invitation> {
    if raw.trim().is_empty() || raw.len() > MAX_ICAL_BYTES {
        return None;
    }
    let lines = unfold(raw);
    if !lines.first()?.eq_ignore_ascii_case("BEGIN:VCALENDAR") || !lines.last()?.eq_ignore_ascii_case("END:VCALENDAR") {
        return None;
    }
    let (cal_lines, ev_lines) = structure(&lines)?;
    let cal: Vec<LineProperty> = cal_lines.iter().map(|l| property_from_line(l)).collect::<Option<_>>()?;
    let ev: Vec<LineProperty> = ev_lines.iter().map(|l| property_from_line(l)).collect::<Option<_>>()?;
    let named =
        |ps: &[LineProperty], n: &str| -> Vec<LineProperty> { ps.iter().filter(|p| p.name == n).cloned().collect() };
    let methods = named(&cal, "METHOD");
    let method = methods.first()?.value.trim().to_uppercase();
    if !(method == "REQUEST" || method == "CANCEL") || methods.len() != 1 {
        return None;
    }
    let first = |n: &str| named(&ev, n).into_iter().next();
    let (uid, start, end, stamp, sequence, summary, organizer, recurrence) = (
        first("UID"),
        first("DTSTART"),
        first("DTEND"),
        first("DTSTAMP"),
        first("SEQUENCE"),
        first("SUMMARY"),
        first("ORGANIZER"),
        first("RECURRENCE-ID"),
    );
    let stamp_date = stamp.as_ref().and_then(|s| parse_date(s, raw));
    let uid = uid.filter(|u| !u.value.trim().is_empty())?;
    if named(&ev, "UID").len() != 1 {
        return None;
    }
    let seq_value: i64 = match &sequence {
        Some(s) => {
            if s.value.is_empty() || !s.value.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            s.value.parse().ok().filter(|v: &i64| *v <= 9_007_199_254_740_991)?
        }
        None => 0,
    };
    if named(&ev, "SEQUENCE").len() > 1 {
        return None;
    }
    let recurrence_value = match &recurrence {
        Some(r) => {
            let v = parse_date(r, raw).map(|d| iso_compact(&d.date))?;
            if named(&ev, "RECURRENCE-ID").len() != 1 {
                return None;
            }
            v
        }
        None => String::new(),
    };
    let summary_text = summary.map(|s| unescape_ical_text(&s.value));
    if method == "CANCEL" {
        let allowed = [
            "UID",
            "DTSTAMP",
            "DTSTART",
            "DTEND",
            "SEQUENCE",
            "RECURRENCE-ID",
            "ORGANIZER",
            "ATTENDEE",
            "STATUS",
            "SUMMARY",
            "DESCRIPTION",
            "LOCATION",
        ];
        let singletons = [
            "UID",
            "DTSTAMP",
            "DTSTART",
            "DTEND",
            "SEQUENCE",
            "RECURRENCE-ID",
            "ORGANIZER",
            "STATUS",
            "SUMMARY",
            "DESCRIPTION",
            "LOCATION",
        ];
        if ev.iter().any(|p| !allowed.contains(&p.name.as_str())) || singletons.iter().any(|n| named(&ev, n).len() > 1)
        {
            return None;
        }
        let organizer = organizer.filter(|o| !o.value.trim().is_empty())?;
        if stamp.is_none() || stamp_date.as_ref().map(|d| d.form) != Some("utc") || sequence.is_none() {
            return None;
        }
        if let Some(status) = named(&ev, "STATUS").first()
            && status.value.trim().to_uppercase() != "CANCELLED"
        {
            return None;
        }
        if named(&ev, "ATTENDEE").iter().any(|a| a.value.trim().is_empty()) {
            return None;
        }
        let starts = match &start {
            Some(s) => Some(parse_date(s, raw)?),
            None => None,
        };
        let ends = match &end {
            Some(e) => Some(parse_date(e, raw)?),
            None => None,
        };
        if let (Some(s), Some(e)) = (&starts, &ends)
            && (s.form != e.form || s.time_zone != e.time_zone || e.date <= s.date)
        {
            return None;
        }
        return Some(Invitation {
            method,
            state: "cancelled",
            uid: uid.value,
            recurrence_id: recurrence_value,
            sequence: seq_value,
            summary: summary_text,
            organizer: Some(organizer.value),
            starts_at: starts.as_ref().map(|s| s.date),
            ends_at: ends.as_ref().map(|e| e.date),
            all_day: starts.as_ref().map(|s| s.all_day),
            time_zone: starts.and_then(|s| s.time_zone),
            raw: raw.to_owned(),
        });
    }
    let attendees = named(&ev, "ATTENDEE");
    let organizer = organizer.filter(|o| !o.value.trim().is_empty())?;
    if stamp.is_none()
        || named(&ev, "DTSTAMP").len() != 1
        || stamp_date.map(|d| d.form) != Some("utc")
        || named(&ev, "ORGANIZER").len() != 1
    {
        return None;
    }
    if start.is_none()
        || named(&ev, "DTSTART").len() != 1
        || end.is_none()
        || named(&ev, "DTEND").len() != 1
        || attendees.is_empty()
    {
        return None;
    }
    if attendees.iter().any(|a| a.value.trim().is_empty()) {
        return None;
    }
    let starts = parse_date(start.as_ref()?, raw)?;
    let ends = parse_date(end.as_ref()?, raw)?;
    if starts.form != ends.form || starts.time_zone != ends.time_zone || ends.date <= starts.date {
        return None;
    }
    if let Some(r) = &recurrence {
        let rd = parse_date(r, raw)?;
        if rd.form != starts.form || rd.time_zone != starts.time_zone {
            return None;
        }
    }
    Some(Invitation {
        method,
        state: "pending",
        uid: uid.value,
        recurrence_id: recurrence_value,
        sequence: seq_value,
        summary: summary_text,
        organizer: Some(organizer.value),
        starts_at: Some(starts.date),
        ends_at: Some(ends.date),
        all_day: Some(starts.all_day),
        time_zone: starts.time_zone,
        raw: raw.to_owned(),
    })
}

pub async fn persist_inbound(db: &impl Q, message_id: Uuid, inv: &Invitation) -> inboxora_core::Result<()> {
    db.exec(
        "INSERT INTO inbound_calendar_invitations (message_id, method, state, uid, recurrence_id, sequence, summary, organizer, starts_at, ends_at, all_day, timezone, raw_ical) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         ON CONFLICT (message_id) DO UPDATE SET method = EXCLUDED.method, state = EXCLUDED.state, uid = EXCLUDED.uid, recurrence_id = EXCLUDED.recurrence_id, \
           sequence = EXCLUDED.sequence, summary = EXCLUDED.summary, organizer = EXCLUDED.organizer, starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, \
           all_day = EXCLUDED.all_day, timezone = EXCLUDED.timezone, raw_ical = EXCLUDED.raw_ical, parsed_at = NOW(), updated_at = NOW()",
        params![
            message_id,
            &inv.method,
            inv.state,
            &inv.uid,
            &inv.recurrence_id,
            inv.sequence,
            &inv.summary,
            &inv.organizer,
            inv.starts_at,
            inv.ends_at,
            inv.all_day,
            &inv.time_zone,
            &inv.raw
        ],
    )
    .await?;
    Ok(())
}

// ── replies ─────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub uid: String,
    pub recurrence_id: Option<String>,
    pub sequence: i64,
    pub summary: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub all_day: bool,
    pub attendees: Vec<(String, Option<String>, String)>,
}

const PARTSTATS: [&str; 5] = ["ACCEPTED", "TENTATIVE", "DECLINED", "NEEDS-ACTION", "DELEGATED"];

fn address(v: &str) -> String {
    let t = v.trim_start();
    let t = if t.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("mailto:")) { &t[7..] } else { t };
    t.trim().to_lowercase()
}

/// An ICAL.Time-valued property as `toJSDate().toISOString()` (all-day dates at UTC midnight).
fn time_iso(p: &inboxora_pim::ical::component::Property, raw: &str) -> Option<(String, bool)> {
    let value = p.first_str()?;
    let compact = value.replace(['-', ':'], "");
    let params: HashMap<String, String> =
        p.params.iter().map(|(k, v)| (k.to_uppercase(), v.first().to_owned())).collect();
    let date_only = p.vtype == "date" || (compact.len() == 8 && compact.bytes().all(|b| b.is_ascii_digit()));
    let resolver = zone_resolver(raw);
    let parsed = match params.get("TZID").and_then(|tz| resolver(tz)) {
        Some(zone) => {
            let f = move |_: &str| Some(zone.clone());
            parse_ical_date(&compact, &params, Some(&f))
        }
        None => {
            let mut p = params.clone();
            p.remove("TZID");
            let floating = if !date_only && !compact.ends_with('Z') { format!("{compact}Z") } else { compact.clone() };
            parse_ical_date(&floating, &p, None)
        }
    }?;
    Some((parsed.date.to_rfc3339_opts(SecondsFormat::Millis, true), date_only))
}

/// `parseInboundCalendarReply`: an attendee's METHOD:REPLY.
pub fn parse_inbound_reply(raw: &str) -> Option<Reply> {
    if raw.trim().is_empty() || raw.len() > MAX_ICAL_BYTES {
        return None;
    }
    let cal = parse_component(raw).ok()?;
    if cal.first_value("method").unwrap_or_default().to_uppercase() != "REPLY" {
        return None;
    }
    let ev = cal.first_subcomponent("vevent")?;
    let uid = ev.first_value("uid").unwrap_or_default().trim().to_owned();
    if uid.is_empty() {
        return None;
    }
    let attendees: Vec<(String, Option<String>, String)> = ev
        .props_named("attendee")
        .map(|p| {
            let partstat = p.param("partstat").unwrap_or("NEEDS-ACTION").to_uppercase();
            (address(&p.first_str().unwrap_or_default()), p.param("cn").map(str::to_owned), partstat)
        })
        .filter(|(e, _, ps)| e.contains('@') && PARTSTATS.contains(&ps.as_str()))
        .collect();
    if attendees.is_empty() {
        return None;
    }
    let t = |n: &str| ev.prop(n).and_then(|p| time_iso(p, raw));
    let start = t("dtstart");
    Some(Reply {
        uid,
        recurrence_id: t("recurrence-id").map(|(s, _)| s),
        sequence: ev
            .first_value("sequence")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .map_or(0, |v| v as i64),
        summary: ev.first_value("summary").filter(|s| !s.is_empty()),
        all_day: start.as_ref().is_some_and(|(_, d)| *d),
        starts_at: start.map(|(s, _)| s),
        ends_at: t("dtend").map(|(s, _)| s),
        attendees,
    })
}

/// `applyReplyToIcal`: set each answering attendee's PARTSTAT; None when nothing changed.
pub fn apply_reply_to_ical(raw: &str, reply: &Reply) -> Option<String> {
    let mut cal = parse_component(raw).ok()?;
    let answers: HashMap<&str, &str> = reply.attendees.iter().map(|(e, _, ps)| (e.as_str(), ps.as_str())).collect();
    let mut changed = false;
    for ev in cal.comps.iter_mut().filter(|c| c.name == "vevent") {
        for p in ev.props.iter_mut().filter(|p| p.name == "attendee") {
            let Some(answer) = answers.get(address(&p.first_str().unwrap_or_default()).as_str()) else { continue };
            if p.param("partstat").unwrap_or("").to_uppercase() == *answer {
                continue;
            }
            p.set_param("partstat", answer);
            changed = true;
        }
    }
    changed.then(|| stringify_component(&cal))
}

/// `applyInvitationReply`: record attendee answers on the user's own sent invitation.
pub async fn apply_invitation_reply(db: &Db, user_id: &str, reply: &Reply) -> inboxora_core::Result<u64> {
    let rows = db
        .rows(
            "SELECT e.id, e.uid, e.raw_ical, e.calendar_id, c.source, c.read_only, c.external_url, c.user_id \
             FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id \
             WHERE e.user_id = $1 AND e.uid = $2 AND e.invite_account_id IS NOT NULL",
            params![user_id, &reply.uid],
        )
        .await?;
    let mut updated = 0;
    for r in rows {
        let current: String = r.get("raw_ical");
        let Some(raw) = apply_reply_to_ical(&current, reply) else { continue };
        let id: Uuid = r.get("id");
        let done = db
            .opt(
                "UPDATE calendar_events SET raw_ical = $1, etag = gen_random_uuid()::text, updated_at = NOW() \
                 WHERE id = $2 AND user_id = $3 AND raw_ical = $4 RETURNING id",
                params![&raw, id, user_id, &current],
            )
            .await?;
        if done.is_none() {
            continue;
        }
        updated += 1;
        let source: String = r.get("source");
        if source == "caldav" {
            let cal = crate::services::dav_sync::WritableCalendar::from_row(&r);
            let uid: String = r.get("uid");
            let out = crate::services::dav_sync::push_event_to_remote(
                db,
                &cal,
                &uid,
                Some(&raw),
                crate::services::dav_sync::Action::Put,
                None,
            )
            .await;
            if let crate::services::dav_sync::Outcome::Error(e) | crate::services::dav_sync::Outcome::Conflict(e) = out
            {
                tracing::warn!("Pushing an invitation reply to the remote calendar failed: {e}");
            }
        }
    }
    Ok(updated)
}

/// `persistInboundCalendarInvitationFromMessage`: inspect a stored message's calendar parts
/// (fetching any not already in hand) and persist an invitation or apply a reply.
pub async fn persist_inbound_from_message(
    db: &Db,
    client: &mut Client,
    msg: &Message,
    row_id: Uuid,
) -> Result<bool, String> {
    let (Some(uid), Some(structure)) = (msg.uid, msg.body_structure.as_ref()) else { return Ok(false) };
    let mut c = Collected::default();
    walk_structure(structure, &mut c);
    let mut invitation: Option<Invitation> = None;
    let mut reply: Option<Reply> = None;
    for part in &c.calendar_parts {
        let mut payload = msg.body_parts.get(&part.part).cloned();
        if payload.is_none() {
            let q = FetchQuery { uid: true, body_parts: vec![part.part.clone()], ..Default::default() };
            match client.fetch(&uid.to_string(), &q, true, None).await {
                Ok(msgs) => {
                    for m in msgs {
                        if let Some(p) = m.body_parts.get(&part.part) {
                            payload = Some(p.clone());
                        }
                    }
                }
                Err(e) if e.is_fatal() => return Err(e.to_string()),
                Err(_) => continue,
            }
        }
        let decoded = payload.map(|p| decode_body(&p, &part.encoding, &part.charset));
        let parsed = decoded.as_deref().and_then(parse_inbound_invitation);
        let Some(parsed) = parsed else {
            if reply.is_none() {
                reply = decoded.as_deref().and_then(parse_inbound_reply);
            }
            continue;
        };
        if invitation.is_some() {
            return Ok(false);
        }
        invitation = Some(parsed);
    }
    if invitation.is_none()
        && let Some(reply) = reply
    {
        let owner = db
            .opt(
                "SELECT a.user_id FROM messages m JOIN email_accounts a ON a.id = m.account_id WHERE m.id = $1",
                params![row_id],
            )
            .await
            .map_err(|e| e.to_string())?;
        if let Some(o) = owner {
            let user: Uuid = o.get("user_id");
            apply_invitation_reply(db, &user.to_string(), &reply).await.map_err(|e| e.to_string())?;
        }
        return Ok(true);
    }
    let Some(inv) = invitation else { return Ok(false) };
    persist_inbound(db, row_id, &inv).await.map_err(|e| e.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:abc@x\r\nDTSTAMP:20260101T100000Z\r\nDTSTART:20260105T090000Z\r\nDTEND:20260105T100000Z\r\nSUMMARY:Plan\\, review\r\nORGANIZER:mailto:o@x.test\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:a@x.test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn parses_requests_and_rejects_shapes() {
        let inv = parse_inbound_invitation(REQUEST).unwrap();
        assert_eq!((inv.method.as_str(), inv.state, inv.uid.as_str()), ("REQUEST", "pending", "abc@x"));
        assert_eq!(inv.summary.as_deref(), Some("Plan, review"));
        assert_eq!(inv.all_day, Some(false));
        assert!(parse_inbound_invitation(&REQUEST.replace("METHOD:REQUEST", "METHOD:PUBLISH")).is_none());
        assert!(
            parse_inbound_invitation(&REQUEST.replace("DTEND:20260105T100000Z", "DTEND:20260105T080000Z")).is_none()
        );
        let tz = REQUEST
            .replace("DTSTART:20260105T090000Z", "DTSTART;TZID=Europe/Berlin:20260105T090000")
            .replace("DTEND:20260105T100000Z", "DTEND;TZID=Europe/Berlin:20260105T100000");
        let inv = parse_inbound_invitation(&tz).unwrap();
        assert_eq!(inv.starts_at.unwrap().to_rfc3339(), "2026-01-05T08:00:00+00:00");
        assert_eq!(inv.time_zone.as_deref(), Some("Europe/Berlin"));
    }

    #[test]
    fn replies_update_partstat() {
        let reply_raw =
            REQUEST.replace("METHOD:REQUEST", "METHOD:REPLY").replace("PARTSTAT=NEEDS-ACTION", "PARTSTAT=ACCEPTED");
        let reply = parse_inbound_reply(&reply_raw).unwrap();
        assert_eq!(reply.attendees[0].0, "a@x.test");
        assert_eq!(reply.starts_at.as_deref(), Some("2026-01-05T09:00:00.000Z"));
        let updated = apply_reply_to_ical(REQUEST, &reply).unwrap();
        assert!(updated.contains("PARTSTAT=ACCEPTED"));
        assert!(apply_reply_to_ical(&updated, &reply).is_none());
    }
}
