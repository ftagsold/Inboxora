//! iCalendar handling: ports of `utils/ical.ts` and `utils/calendarRecurrence.ts` on top of a
//! port of the ical.js subset they use.
//!
//! The ical.js port is deliberately literal (statement order, quirks and all) so occurrence
//! dates and recurrence ids match the Node backend; lints that would restructure it are allowed.
#![allow(
    clippy::should_implement_trait,
    clippy::nonminimal_bool,
    clippy::implicit_saturating_sub,
    clippy::type_complexity,
    clippy::while_let_loop
)]

pub mod component;
pub mod event;
pub mod recur;
pub mod time;
pub mod timezone;
pub mod vtimezone;

use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Map, Value};

use component::{Component, PValue, Property, parse_component, stringify_component, unescape_text};
use event::Event;
use recur::Recur;
use time::{Fields, Time, Zone};
use timezone::{Calendar, Timezone};

/// JS `parseInt(s, 10)`.
pub fn js_parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n: i64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// JS `parseFloat`.
pub fn js_parse_float(s: &str) -> Option<f64> {
    let t = s.trim_start();
    let end = t
        .char_indices()
        .take_while(|(i, c)| {
            c.is_ascii_digit() || *c == '.' || ((*c == '-' || *c == '+') && *i == 0) || *c == 'e' || *c == 'E'
        })
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    (1..=end).rev().find_map(|n| t[..n].parse::<f64>().ok())
}

/// A property line parsed by the lightweight `propertyFromLine`.
#[derive(Debug, Clone, PartialEq)]
pub struct LineProperty {
    pub name: String,
    pub parameters: HashMap<String, String>,
    pub value: String,
}

fn unfold_lines(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    for physical in normalized.split('\n') {
        if physical.starts_with([' ', '\t']) && !lines.is_empty() {
            let last = lines.len() - 1;
            lines[last].push_str(&physical[1..]);
        } else if !physical.is_empty() {
            lines.push(physical.to_owned());
        }
    }
    lines
}

/// `propertyFromLine`: name, parameters (uppercased names, quotes stripped) and value.
pub fn property_from_line(line: &str) -> Option<LineProperty> {
    let mut quoted = false;
    let mut sep = None;
    for (i, c) in line.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c == ':' && !quoted {
            sep = Some(i);
            break;
        }
    }
    let sep = sep.filter(|s| *s >= 1)?;
    let head = &line[..sep];
    // Split on ';' outside quotes (the lookahead counts an even number of quotes to the end).
    let mut parts: Vec<&str> = Vec::new();
    let mut start = 0;
    let quote_positions: Vec<usize> = head.match_indices('"').map(|(i, _)| i).collect();
    for (i, c) in head.char_indices() {
        if c == ';' {
            let quotes_after = quote_positions.iter().filter(|q| **q > i).count();
            if quotes_after % 2 == 0 {
                parts.push(&head[start..i]);
                start = i + 1;
            }
        }
    }
    parts.push(&head[start..]);
    let name = parts[0].to_uppercase();
    let mut parameters = HashMap::new();
    for part in &parts[1..] {
        match part.find('=') {
            Some(eq) if eq >= 1 => {
                let v = &part[eq + 1..];
                let v = v.strip_prefix('"').unwrap_or(v);
                let v = v.strip_suffix('"').unwrap_or(v);
                parameters.insert(part[..eq].to_uppercase(), v.to_owned());
            }
            _ => {
                parameters.insert(part.to_uppercase(), String::new());
            }
        }
    }
    Some(LineProperty { name, parameters, value: line[sep + 1..].to_owned() })
}

/// `unescapeICalendarText`.
pub fn unescape_ical_text(v: &str) -> String {
    unescape_text(v)
}

/// `new Date(Date.UTC(...))`, rejecting rolled-over fields and two-digit years like JS does.
fn utc_date(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> Option<DateTime<Utc>> {
    if (0..=99).contains(&y) {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(i32::try_from(y).ok()?, u32::try_from(mo).ok()?, u32::try_from(d).ok()?)?;
    let dt = date.and_hms_opt(u32::try_from(h).ok()?, u32::try_from(mi).ok()?, u32::try_from(s).ok()?)?;
    Some(dt.and_utc())
}

fn digits(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit())
}

/// Local wall time in an IANA zone → instant, with the same fixed-point iteration as the
/// Node code (which used Intl); `None` for unknown zones and nonexistent local times.
pub fn local_date_in_time_zone(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64, zone: &str) -> Option<DateTime<Utc>> {
    let wall = utc_date(y, mo, d, h, mi, s)?;
    let tz = chrono_tz::Tz::from_str(zone).or_else(|_| chrono_tz::Tz::from_str_insensitive(zone)).ok()?;
    let parts = |instant: DateTime<Utc>| instant.with_timezone(&tz).naive_local();
    let mut instant = wall;
    for _ in 0..3 {
        let local = parts(instant).and_utc();
        let offset = local - instant;
        instant = wall - offset;
    }
    let resolved = parts(instant);
    (resolved == wall.naive_utc()).then_some(instant)
}

/// `parseUtc`.
pub fn parse_utc(v: &str) -> Option<DateTime<Utc>> {
    let b = v.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' || !digits(&v[..8], 8) || !digits(&v[9..15], 6) {
        return None;
    }
    let n = |a: usize, z: usize| v[a..z].parse::<i64>().ok();
    utc_date(n(0, 4)?, n(4, 6)?, n(6, 8)?, n(9, 11)?, n(11, 13)?, n(13, 15)?)
}

/// A parsed date value.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDate {
    pub date: DateTime<Utc>,
    pub all_day: bool,
    pub time_zone: Option<String>,
}

pub type ZoneFor<'a> = &'a dyn Fn(&str) -> Option<Rc<Timezone>>;

/// `parseICalendarDate({ value, parameters }, zoneFor)`.
pub fn parse_ical_date(
    value: &str,
    params: &HashMap<String, String>,
    zone_for: Option<ZoneFor<'_>>,
) -> Option<ParsedDate> {
    let value_param = params.get("VALUE").map(|v| v.to_uppercase());
    let date_only = value_param.as_deref() == Some("DATE") || digits(value, 8);
    if date_only {
        if !digits(value, 8) {
            return None;
        }
        let n = |a: usize, z: usize| value[a..z].parse::<i64>().ok();
        let date = utc_date(n(0, 4)?, n(4, 6)?, n(6, 8)?, 0, 0, 0)?;
        return Some(ParsedDate { date, all_day: true, time_zone: None });
    }
    let b = value.as_bytes();
    let utc = value.ends_with('Z');
    let core = if utc { &value[..value.len() - 1] } else { value };
    if core.len() != 15 || b[8] != b'T' || !digits(&core[..8], 8) || !digits(&core[9..15], 6) {
        return None;
    }
    if value_param.as_deref().is_some_and(|v| v != "DATE-TIME") {
        return None;
    }
    let n = |a: usize, z: usize| core[a..z].parse::<i64>().unwrap_or(-1);
    let (y, mo, d, h, mi, s) = (n(0, 4), n(4, 6), n(6, 8), n(9, 11), n(11, 13), n(13, 15));
    if utc {
        if params.contains_key("TZID") {
            return None;
        }
        return utc_date(y, mo, d, h, mi, s).map(|date| ParsedDate { date, all_day: false, time_zone: None });
    }
    let tzid = params.get("TZID").filter(|t| !t.is_empty())?;
    utc_date(y, mo, d, h, mi, s)?;
    let date = match zone_for.and_then(|f| f(tzid)) {
        Some(zone) => {
            let t = Time::new(
                Fields { year: y, month: mo, day: d, hour: h, minute: mi, second: s, is_date: false },
                Zone::Tz(zone),
            );
            DateTime::<Utc>::from_timestamp(t.to_unix_time(), 0)?
        }
        None => local_date_in_time_zone(y, mo, d, h, mi, s, tzid)?,
    };
    Some(ParsedDate { date, all_day: false, time_zone: Some(tzid.clone()) })
}

/// `parseDuration` (positive durations only, in milliseconds).
fn parse_duration_ms(value: &str) -> Option<i64> {
    let rest = value.strip_prefix('P')?;
    let num = |s: &str| if s.is_empty() { Some(0) } else { s.parse::<i64>().ok() };
    let total = if let Some(w) = rest.strip_suffix('W') {
        if !digits(w, w.len()) || w.is_empty() {
            return None;
        }
        num(w)? * 7 * 86_400
    } else {
        let (date_part, time_part) = match rest.split_once('T') {
            Some((d, t)) => (d, Some(t)),
            None => (rest, None),
        };
        let days = match date_part {
            "" => 0,
            d => {
                let n = d.strip_suffix('D')?;
                if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                num(n)?
            }
        };
        let mut secs = days * 86_400;
        if let Some(t) = time_part {
            let mut rest = t;
            for (unit, mult) in [('H', 3600), ('M', 60), ('S', 1)] {
                if let Some(i) = rest.find(unit) {
                    let n = &rest[..i];
                    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                        return None;
                    }
                    secs += num(n)? * mult;
                    rest = &rest[i + 1..];
                }
            }
            if !rest.is_empty() {
                return None;
            }
        }
        secs
    };
    let ms = total * 1000;
    (ms > 0).then_some(ms)
}

/// A VEVENT summarised for storage (`parseCalendarEvent`).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    pub uid: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub all_day: bool,
    pub time_zone: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub url: Option<String>,
    pub organizer: Option<String>,
    pub attendees: Vec<String>,
    pub raw: String,
}

/// `calendarZoneResolver(raw)`: zones defined in the document (with STANDARD/DAYLIGHT rules).
pub fn zone_resolver(raw: &str) -> impl Fn(&str) -> Option<Rc<Timezone>> + '_ {
    let parsed: std::cell::OnceCell<Option<Component>> = std::cell::OnceCell::new();
    let cache: std::cell::RefCell<HashMap<String, Option<Rc<Timezone>>>> = std::cell::RefCell::new(HashMap::new());
    move |tzid: &str| {
        if let Some(z) = cache.borrow().get(tzid) {
            return z.clone();
        }
        let root = parsed.get_or_init(|| parse_component(raw).ok());
        let resolved = root.as_ref().and_then(|r| {
            r.subcomponents("vtimezone")
                .find(|z| z.first_value("tzid").as_deref() == Some(tzid))
                .filter(|z| z.comps.iter().any(|c| c.name == "standard" || c.name == "daylight"))
                .map(|z| Rc::new(Timezone::new(tzid, z.clone())))
        });
        cache.borrow_mut().insert(tzid.to_owned(), resolved.clone());
        resolved
    }
}

fn mailto_strip(s: &str) -> String {
    if s.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("mailto:")) { s[7..].to_owned() } else { s.to_owned() }
}

/// `parseCalendarEvent(raw)`.
pub fn parse_calendar_event(raw: &str) -> Option<ParsedEvent> {
    if raw.is_empty() || raw.len() > 1024 * 1024 {
        return None;
    }
    let lines = unfold_lines(raw);
    let upper: Vec<String> = lines.iter().map(|l| l.to_uppercase()).collect();
    let starts = upper.iter().filter(|l| *l == "BEGIN:VEVENT").count();
    let ends = upper.iter().filter(|l| *l == "END:VEVENT").count();
    if starts > 1 && starts == ends {
        let mut root = parse_component(raw).ok()?;
        let events: Vec<&Component> = root.subcomponents("vevent").collect();
        let uid = events.first()?.first_value("uid").filter(|u| !u.is_empty())?;
        if events.iter().any(|e| e.first_value("uid").as_deref() != Some(uid.as_str())) {
            return None;
        }
        if events.iter().filter(|e| !e.has("recurrence-id")).count() > 1 {
            return None;
        }
        let master_idx = root
            .comps
            .iter()
            .position(|c| c.name == "vevent" && !c.has("recurrence-id"))
            .or_else(|| root.comps.iter().position(|c| c.name == "vevent"))?;
        // removeSubcomponent drops the other events; the master keeps its position.
        let mut i = 0;
        root.comps.retain(|c| {
            let keep = c.name != "vevent" || i == master_idx;
            i += 1;
            keep
        });
        let parsed = parse_calendar_event(&stringify_component(&root))?;
        return Some(ParsedEvent { raw: raw.to_owned(), ..parsed });
    }
    let start = upper.iter().position(|l| l == "BEGIN:VEVENT");
    let end = upper.iter().position(|l| l == "END:VEVENT");
    let (Some(start), Some(end)) = (start, end) else { return None };
    if starts != 1 || ends != 1 || end <= start {
        return None;
    }
    let mut depth = 0i32;
    let mut props = Vec::new();
    for line in &lines[start + 1..end] {
        let up = line.to_uppercase();
        if up.starts_with("BEGIN:") {
            depth += 1;
            continue;
        }
        if up.starts_with("END:") {
            depth -= 1;
            continue;
        }
        if depth == 0 {
            props.push(property_from_line(line)?);
        }
    }
    let zone_for = zone_resolver(raw);
    let named = |n: &str| props.iter().filter(|p| p.name == n).collect::<Vec<_>>();
    let uid = named("UID");
    let dtstart = named("DTSTART");
    let dtend = named("DTEND");
    let duration = named("DURATION");
    let uid = uid.first().filter(|u| !u.value.trim().is_empty() && named("UID").len() == 1)?;
    if dtstart.len() != 1 || dtend.len() > 1 || duration.len() > 1 || (!dtend.is_empty() && !duration.is_empty()) {
        return None;
    }
    let starts_at = parse_ical_date(&dtstart[0].value, &dtstart[0].parameters, Some(&zone_for))?;
    let ends_at = if let Some(e) = dtend.first() {
        let end = parse_ical_date(&e.value, &e.parameters, Some(&zone_for))?;
        if end.all_day != starts_at.all_day {
            return None;
        }
        end.date
    } else if let Some(d) = duration.first() {
        let ms = parse_duration_ms(&d.value)?;
        if starts_at.all_day && ms % (86_400 * 1000) != 0 {
            return None;
        }
        starts_at.date + chrono::Duration::milliseconds(ms)
    } else if starts_at.all_day {
        starts_at.date + chrono::Duration::days(1)
    } else {
        return None;
    };
    if ends_at <= starts_at.date {
        return None;
    }
    let first = |n: &str| named(n).first().map(|p| p.value.clone());
    Some(ParsedEvent {
        uid: uid.value.clone(),
        starts_at: starts_at.date,
        ends_at,
        all_day: starts_at.all_day,
        time_zone: starts_at.time_zone,
        summary: first("SUMMARY").map(|s| unescape_ical_text(&s)),
        description: event_description(&props, raw),
        location: first("LOCATION").map(|s| unescape_ical_text(&s)),
        url: first("URL").filter(|u| !u.is_empty()),
        organizer: first("ORGANIZER").map(|o| mailto_strip(&o)).filter(|o| !o.is_empty()),
        attendees: named("ATTENDEE").iter().map(|p| mailto_strip(&p.value)).collect(),
        raw: raw.to_owned(),
    })
}

fn event_description(props: &[LineProperty], raw: &str) -> Option<String> {
    use inboxora_mail::util::{is_html_description, sanitize_description_html};
    let html_alt = props.iter().find(|p| {
        p.name == "X-ALT-DESC" && p.parameters.get("FMTTYPE").is_some_and(|f| f.to_lowercase().contains("html"))
    });
    if let Some(alt) = html_alt {
        let html = sanitize_description_html(&unescape_ical_text(&alt.value));
        if is_html_description(&html) {
            return Some(html);
        }
    }
    if let Some(plain) = props.iter().find(|p| p.name == "DESCRIPTION") {
        return Some(sanitize_description_html(&unescape_ical_text(&plain.value)));
    }
    let root = parse_component(raw).ok()?;
    calendar_description(root.first_subcomponent("vevent"))
}

/// `calendarDescription(component)`.
pub fn calendar_description(c: Option<&Component>) -> Option<String> {
    use inboxora_mail::util::{is_html_description, sanitize_description_html};
    let c = c?;
    let html = c
        .props_named("x-alt-desc")
        .find(|p| p.param("fmttype").unwrap_or("undefined").to_lowercase().contains("html"))
        .and_then(Property::first_str)
        .filter(|h| !h.is_empty());
    if let Some(h) = html {
        let s = sanitize_description_html(&h);
        if is_html_description(&s) {
            return Some(s);
        }
    }
    c.first_value("description").filter(|d| !d.is_empty()).map(|d| sanitize_description_html(&d))
}

/// `calendarProjection(raw)`.
pub fn calendar_projection(raw: &str) -> Result<ParsedEvent, String> {
    parse_calendar_event(raw).ok_or_else(|| "Invalid calendar event".into())
}

/// `calendarResources(raw)`: split a multi-event document into one resource per UID.
pub fn calendar_resources(raw: &str) -> Result<Vec<String>, String> {
    let root = parse_component(raw)?;
    let has_end = raw.lines().any(|l| l.trim_end().eq_ignore_ascii_case("END:VCALENDAR"));
    if root.name != "vcalendar" || !has_end {
        return Err("Invalid calendar document".into());
    }
    let mut groups: Vec<(Option<String>, Vec<&Component>)> = Vec::new();
    for ev in root.subcomponents("vevent") {
        let uid = ev.first_value("uid").filter(|u| !u.is_empty());
        match uid.as_ref().and_then(|u| groups.iter_mut().find(|(k, _)| k.as_deref() == Some(u.as_str()))) {
            Some((_, list)) => list.push(ev),
            None => groups.push((uid, vec![ev])),
        }
    }
    Ok(groups
        .into_iter()
        .map(|(_, events)| {
            let mut cal = Component::new("vcalendar");
            cal.update_text("version", "2.0");
            cal.comps.extend(root.subcomponents("vtimezone").cloned());
            cal.comps.extend(events.into_iter().cloned());
            stringify_component(&cal)
        })
        .collect())
}

fn date_of(
    cal_zone: &dyn Fn(&str) -> Option<Rc<Timezone>>,
    t: &Time,
    prop: Option<&Property>,
) -> Option<DateTime<Utc>> {
    let value = t.to_ical_string();
    let param_tzid = prop.and_then(|p| p.param("tzid")).filter(|s| !s.is_empty()).map(str::to_owned);
    let tzid = param_tzid.or_else(|| Some(t.zone.tzid().to_owned()));
    let mut params = HashMap::new();
    if t.is_date() {
        params.insert("VALUE".into(), "DATE".into());
    } else if !value.ends_with('Z')
        && let Some(z) = tzid
    {
        params.insert("TZID".into(), z);
    }
    parse_ical_date(&value, &params, Some(cal_zone)).map(|p| p.date)
}

/// Projection limits (`ProjectOptions`).
#[derive(Clone, Default)]
pub struct ProjectOptions {
    pub max_iterations: Option<u64>,
    pub deadline: Option<std::time::Instant>,
    pub should_abort: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
    pub full_scan: bool,
}

pub const DEFAULT_MAX_ITERATIONS: u64 = 100_000;
const MAX_BEHIND_UTC_OFFSET_S: i64 = 14 * 3600;
const MAX_AHEAD_UTC_OFFSET_S: i64 = 12 * 3600;

/// One projected occurrence: the row metadata merged with occurrence fields.
#[derive(Debug, Clone, PartialEq)]
pub struct Projected {
    pub fields: Map<String, Value>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectStatus {
    pub events: Vec<Projected>,
    pub truncated: bool,
    pub reason: Option<&'static str>,
    pub error: Option<String>,
}

fn wall_clock_secs(t: &Time) -> Option<i64> {
    let f = t.fields();
    if f.year < 100 {
        return None;
    }
    Some(time::date_utc_seconds(f.year, f.month - 1, f.day, f.hour, f.minute, f.second))
}

fn metadata_time(v: Option<&Value>) -> Option<DateTime<Utc>> {
    v.and_then(Value::as_str).and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc))
}

/// `projectCalendarResourceWithStatus(row, from, to, options)`.
pub fn project_calendar_resource_with_status(
    row: &Map<String, Value>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    options: &ProjectOptions,
) -> Result<ProjectStatus, String> {
    let max_iterations = options.max_iterations.filter(|m| *m > 0).unwrap_or(DEFAULT_MAX_ITERATIONS);
    let mut metadata = row.clone();
    let raw = metadata.shift_remove("raw_ical").and_then(|v| v.as_str().map(str::to_owned)).filter(|s| !s.is_empty());
    let mut status = ProjectStatus::default();
    let passthrough = |metadata: &Map<String, Value>| -> Vec<Projected> {
        let s = metadata_time(metadata.get("starts_at")).unwrap_or_default();
        let e = metadata_time(metadata.get("ends_at")).unwrap_or_default();
        vec![Projected { fields: metadata.clone(), starts_at: s, ends_at: e }]
    };
    let Some(raw) = raw else {
        status.events = passthrough(&metadata);
        return Ok(status);
    };
    let Ok(root) = parse_component(&raw) else {
        status.events = passthrough(&metadata);
        return Ok(status);
    };
    let cal = Calendar::new(root);
    let events: Vec<&Component> = cal.root.subcomponents("vevent").collect();
    let master = events.iter().copied().find(|c| !c.has("recurrence-id"));
    let Some(base) = master.or_else(|| events.first().copied()) else {
        status.events = passthrough(&metadata);
        return Ok(status);
    };
    let zone_cache: std::cell::RefCell<HashMap<String, Option<Rc<Timezone>>>> = std::cell::RefCell::new(HashMap::new());
    let zone_for = |tzid: &str| -> Option<Rc<Timezone>> {
        if let Some(z) = zone_cache.borrow().get(tzid) {
            return z.clone();
        }
        let z = cal
            .root
            .subcomponents("vtimezone")
            .find(|z| z.first_value("tzid").as_deref() == Some(tzid))
            .filter(|z| z.comps.iter().any(|c| c.name == "standard" || c.name == "daylight"))
            .map(|z| Rc::new(Timezone::new(tzid, z.clone())));
        zone_cache.borrow_mut().insert(tzid.to_owned(), z.clone());
        z
    };
    let recurring = master.is_some_and(|m| m.has("rrule") || m.has("rdate"));
    let event = Event::new(&cal, base, &cal.root.comps)?;
    let mut seen: Vec<String> = Vec::new();
    let row_id = metadata.get("id").cloned().unwrap_or(Value::Null);
    let row_id_str = match &row_id {
        Value::String(s) => s.clone(),
        Value::Null => "undefined".into(),
        other => other.to_string(),
    };
    let base_dtstart = base.prop("dtstart");

    let mut result: Vec<Projected> = Vec::new();
    let mut append = |item: &Event<'_>, start: &Time, end: &Time, recurrence_id: &str| -> Result<(), String> {
        let comp = item.comp;
        if comp.first_value("status").map(|s| s.to_uppercase()).as_deref() == Some("CANCELLED") {
            return Ok(());
        }
        let start_prop = comp.prop("dtstart").or(base_dtstart);
        let end_prop = comp.prop("dtend").or(comp.prop("dtstart")).or(base_dtstart);
        let (Some(starts_at), Some(ends_at)) =
            (date_of(&zone_for, start, start_prop), date_of(&zone_for, end, end_prop))
        else {
            return Ok(());
        };
        if starts_at >= to || ends_at <= from || seen.iter().any(|s| s == recurrence_id) {
            return Ok(());
        }
        seen.push(recurrence_id.to_owned());
        let mut f = metadata.clone();
        if recurring {
            f.insert("id".into(), Value::String(format!("{row_id_str}@{recurrence_id}")));
            f.insert("series_id".into(), row_id.clone());
            f.insert("recurrence_id".into(), Value::String(recurrence_id.to_owned()));
            f.insert("recurring".into(), Value::Bool(true));
        }
        let text_or = |name: &str, key: &str| -> Value {
            comp.first_value(name)
                .map(Value::String)
                .or_else(|| metadata.get(key).filter(|v| !v.is_null()).cloned())
                .unwrap_or(Value::Null)
        };
        f.insert("summary".into(), text_or("summary", "summary"));
        let description = calendar_description(Some(comp)).or_else(|| calendar_description(Some(base)));
        match description.map(Value::String).or_else(|| metadata.get("description").cloned()) {
            Some(v) => f.insert("description".into(), v),
            None => f.shift_remove("description"),
        };
        f.insert("location".into(), text_or("location", "location"));
        match comp.first_value("url").map(Value::String).or_else(|| metadata.get("url").cloned()) {
            Some(v) => f.insert("url".into(), v),
            None => f.shift_remove("url"),
        };
        let organizer = comp
            .first_value("organizer")
            .filter(|o| !o.is_empty())
            .or_else(|| {
                metadata.get("organizer").filter(|v| inboxora_mail_truthy(v)).map(inboxora_mail::text::js_string)
            })
            .unwrap_or_default();
        let organizer = mailto_strip(&organizer);
        f.insert("organizer".into(), if organizer.is_empty() { Value::Null } else { Value::String(organizer) });
        let attendees = if comp.has("attendee") {
            Value::Array(
                comp.props_named("attendee")
                    .map(|p| Value::String(mailto_strip(&p.first_str().unwrap_or_else(|| "null".into()))))
                    .collect(),
            )
        } else {
            metadata
                .get("attendees")
                .filter(|v| inboxora_mail_truthy(v))
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()))
        };
        f.insert("attendees".into(), attendees);
        f.insert("starts_at".into(), Value::String(js_iso(&starts_at)));
        f.insert("ends_at".into(), Value::String(js_iso(&ends_at)));
        f.insert("all_day".into(), Value::Bool(start.is_date()));
        result.push(Projected { fields: f, starts_at, ends_at });
        Ok(())
    };

    if !recurring {
        let start = event.start_date()?.ok_or("Cannot read properties of null (reading 'toICALString')")?;
        let end = event.end_date()?.ok_or("Cannot read properties of null (reading 'toICALString')")?;
        append(&event, &start, &end, "")?;
        status.events = result;
        return Ok(status);
    }

    let mut iterator = match event.iterator() {
        Ok(it) => it,
        Err(_) => {
            status.events = result;
            status.truncated = true;
            status.reason = Some("iterator-failed");
            return Ok(status);
        }
    };
    let has_range_exceptions =
        events.iter().any(|e| e.prop("recurrence-id").and_then(|p| p.param("range")) == Some("THISANDFUTURE"));
    let fast_forward = !options.full_scan && !has_range_exceptions;
    let base_duration = match (event.start_date(), event.end_date()) {
        (Ok(Some(s)), Ok(Some(e))) => (e.to_unix_time() - s.to_unix_time()).max(0),
        _ => 0,
    };
    let (from_s, to_s) = (from.timestamp(), to.timestamp());
    let mut iterations: u64 = 0;
    let run = (|| -> Result<(), String> {
        while let Some(occ) = iterator.next()? {
            iterations += 1;
            if iterations > max_iterations {
                status.truncated = true;
                status.reason = Some("iteration-limit");
                break;
            }
            if iterations & 63 == 0 {
                if options.deadline.is_some_and(|d| std::time::Instant::now() > d) {
                    status.truncated = true;
                    status.reason = Some("deadline");
                    break;
                }
                if options.should_abort.as_ref().is_some_and(|f| f()) {
                    status.truncated = true;
                    status.reason = Some("aborted");
                    break;
                }
            }
            if fast_forward {
                match wall_clock_secs(&occ) {
                    None => {}
                    Some(wall) => {
                        if wall - MAX_BEHIND_UTC_OFFSET_S >= to_s {
                            break;
                        }
                        if wall + MAX_AHEAD_UTC_OFFSET_S + base_duration <= from_s {
                            continue;
                        }
                    }
                }
            } else if let Some(instant) = date_of(&zone_for, &occ, base_dtstart)
                && instant >= to
            {
                break;
            }
            let (item, start, end) = event.occurrence_details(&occ)?;
            append(item, &start, &end, &occ.to_js_string())?;
        }
        Ok(())
    })();
    if let Err(e) = run {
        status.truncated = true;
        status.reason = Some("rule-error");
        status.error = Some(e);
    }
    for ex in events.iter().filter(|c| c.has("recurrence-id")) {
        let item = Event::standalone(&cal, ex);
        let (Ok(Some(start)), Ok(Some(end)), Ok(Some(rid))) =
            (item.start_date(), item.end_date(), item.recurrence_id())
        else {
            continue;
        };
        let _ = append(&item, &start, &end, &rid.to_js_string());
    }
    status.events = result;
    Ok(status)
}

fn inboxora_mail_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn js_iso(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// `truncateSeriesBefore(raw, recurrenceId)` → (raw, empty).
pub fn truncate_series_before(raw: &str, recurrence_id: &str) -> Option<(String, bool)> {
    let root = parse_component(raw).ok()?;
    let cal = Calendar::new(root);
    let master_idx = cal.root.comps.iter().position(|c| c.name == "vevent" && !c.has("recurrence-id"))?;
    let master = &cal.root.comps[master_idx];
    let rrule_idx = master.props.iter().position(|p| p.name == "rrule")?;
    let PValue::Recur(data) = master.props[rrule_idx].values.first()? else { return None };
    let mut rule = Recur::from_data(data);
    let dtstart_prop = master.prop("dtstart");
    let id = recur::time_from_string(recurrence_id).ok()?;
    let zone_for = zone_resolver(raw);
    let start_date = date_of(&zone_for, &id, dtstart_prop)?;
    let series_start =
        cal.first_time(master, "dtstart").and_then(Result::ok).and_then(|t| date_of(&zone_for, &t, dtstart_prop));
    let empty = series_start.is_some_and(|s| s >= start_date);
    let before = start_date - chrono::Duration::seconds(1);
    rule.until = Some(if id.is_date() {
        Time::date(
            i64::from(chrono::Datelike::year(&before)),
            i64::from(chrono::Datelike::month(&before)),
            i64::from(chrono::Datelike::day(&before)),
        )
    } else {
        Time::from_unix(before.timestamp(), false)
    });
    let cutoff = start_date;
    let dtstart_prop_owned = dtstart_prop.cloned();
    let keep: Vec<bool> = cal
        .root
        .comps
        .iter()
        .map(|c| {
            if c.name != "vevent" || !c.has("recurrence-id") {
                return true;
            }
            let ex = cal
                .first_time(c, "recurrence-id")
                .and_then(Result::ok)
                .and_then(|t| date_of(&zone_for, &t, dtstart_prop_owned.as_ref()));
            !ex.is_some_and(|d| d >= cutoff)
        })
        .collect();
    let mut root = cal.root.clone();
    root.comps[master_idx].props[rrule_idx].values = vec![PValue::Recur(rule.to_data())];
    let mut i = 0;
    root.comps.retain(|_| {
        let k = keep[i];
        i += 1;
        k
    });
    Some((stringify_component(&root), empty))
}

/// `mergeCalendarResource(raw, replacementRaw, recurrenceId, cancel)`.
pub fn merge_calendar_resource(
    raw: Option<&str>,
    replacement_raw: &str,
    recurrence_id: Option<&str>,
    cancel: bool,
) -> Result<String, String> {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else { return Ok(replacement_raw.to_owned()) };
    let mut root = parse_component(raw)?;
    let replacement_root = parse_component(replacement_raw)?;
    let Some(replacement) = replacement_root.first_subcomponent("vevent") else {
        return Ok(replacement_raw.to_owned());
    };
    let Some(master_idx) = root.comps.iter().position(|c| c.name == "vevent" && !c.has("recurrence-id")) else {
        return Ok(replacement_raw.to_owned());
    };
    let target_idx = match recurrence_id.filter(|r| !r.is_empty()) {
        None => master_idx,
        Some(rid) => {
            let cal = Calendar::new(root.clone());
            let existing = root.comps.iter().position(|c| {
                c.name == "vevent"
                    && cal.first_time(c, "recurrence-id").and_then(Result::ok).is_some_and(|t| t.to_js_string() == rid)
            });
            match existing {
                Some(i) => i,
                None => {
                    let id = recur::time_from_string(rid)?;
                    let master = &root.comps[master_idx];
                    let mut target = master.clone();
                    for f in ["rrule", "rdate", "exdate"] {
                        target.remove_all(f);
                    }
                    let mut prop = Property {
                        name: "recurrence-id".into(),
                        params: Vec::new(),
                        vtype: if id.is_date() { "date".into() } else { "date-time".into() },
                        values: vec![PValue::Str(id.to_js_string())],
                    };
                    if let Some(tzid) = master.prop("dtstart").and_then(|p| p.param("tzid")).filter(|t| !t.is_empty())
                        && !id.is_date()
                    {
                        prop.set_param("tzid", tzid);
                    }
                    target.props.push(prop);
                    root.comps.push(target);
                    root.comps.len() - 1
                }
            }
        }
    };
    let target = &mut root.comps[target_idx];
    for field in [
        "dtstart",
        "dtend",
        "duration",
        "summary",
        "description",
        "location",
        "url",
        "organizer",
        "attendee",
        "dtstamp",
    ] {
        target.remove_all(field);
        target.props.extend(replacement.props_named(field).cloned());
    }
    if cancel {
        target.update_text("status", "CANCELLED");
    } else if target.first_value("status").as_deref() == Some("CANCELLED") {
        target.remove_all("status");
    }
    Ok(stringify_component(&root))
}

/// An attendee's answer on the series master (`attendeeStatuses`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AttendeeStatus {
    pub email: String,
    pub name: Option<String>,
    pub partstat: String,
}

fn ical_address(v: &str) -> String {
    mailto_strip(v.trim_start()).trim().to_lowercase()
}

/// `attendeeStatuses(rawIcal)`.
pub fn attendee_statuses(raw: &str) -> Vec<AttendeeStatus> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let Ok(root) = parse_component(raw) else { return Vec::new() };
    let events: Vec<&Component> = root.subcomponents("vevent").collect();
    let Some(master) = events.iter().find(|e| !e.has("recurrence-id")).or(events.first()) else { return Vec::new() };
    master
        .props_named("attendee")
        .filter_map(|p| {
            let partstat = p.param("partstat").unwrap_or("NEEDS-ACTION").to_uppercase();
            let partstat =
                if matches!(partstat.as_str(), "ACCEPTED" | "TENTATIVE" | "DECLINED" | "NEEDS-ACTION" | "DELEGATED") {
                    partstat
                } else {
                    "NEEDS-ACTION".into()
                };
            let email = ical_address(&p.first_str().unwrap_or_else(|| "null".into()));
            email.contains('@').then(|| AttendeeStatus {
                email,
                name: p.param("cn").filter(|n| !n.is_empty()).map(str::to_owned),
                partstat,
            })
        })
        .collect()
}
