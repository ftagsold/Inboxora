//! iCalendar component tree, parser and serialiser (ports of ical.js `parse.js`,
//! `stringify.js` and the icalendar design set).
//!
//! Values are stored in jCal form (what `ICAL.parse` produces): text unescaped, dates as
//! `YYYY-MM-DD`, date-times as `YYYY-MM-DDTHH:MM:SS[Z]`, recurrence rules as ordered key/value
//! data, UTC offsets as `+HH:MM`.

use super::js_parse_int;

/// A jCal value.
#[derive(Debug, Clone, PartialEq)]
pub enum PValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// RRULE data: lowercase keys in order, values already converted.
    Recur(Vec<(String, RecurVal)>),
    /// Period `[start, end-or-duration]`.
    Period(String, String),
    /// Structured value (`;`-separated parts).
    Structured(Vec<PValue>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecurVal {
    Str(String),
    Int(i64),
    List(Vec<RecurVal>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamVal {
    One(String),
    Many(Vec<String>),
}

impl ParamVal {
    pub fn first(&self) -> &str {
        match self {
            ParamVal::One(s) => s,
            ParamVal::Many(v) => v.first().map_or("", String::as_str),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub name: String,
    pub params: Vec<(String, ParamVal)>,
    pub vtype: String,
    pub values: Vec<PValue>,
}

impl Property {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.first())
    }

    pub fn set_param(&mut self, name: &str, value: &str) {
        match self.params.iter_mut().find(|(k, _)| k == name) {
            Some(slot) => slot.1 = ParamVal::One(value.to_owned()),
            None => self.params.push((name.to_owned(), ParamVal::One(value.to_owned()))),
        }
    }

    /// First value as a string (jCal form), like `getFirstValue()` for undecorated types.
    pub fn first_str(&self) -> Option<String> {
        match self.values.first()? {
            PValue::Str(s) => Some(s.clone()),
            PValue::Int(i) => Some(i.to_string()),
            PValue::Float(f) => Some(f.to_string()),
            PValue::Bool(b) => Some(b.to_string()),
            PValue::Period(a, _) => Some(a.clone()),
            PValue::Recur(_) | PValue::Structured(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Component {
    pub name: String,
    pub props: Vec<Property>,
    pub comps: Vec<Component>,
}

impl Component {
    pub fn new(name: &str) -> Self {
        Component { name: name.to_owned(), props: Vec::new(), comps: Vec::new() }
    }

    pub fn subcomponents<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Component> + 'a {
        self.comps.iter().filter(move |c| c.name == name)
    }

    pub fn first_subcomponent(&self, name: &str) -> Option<&Component> {
        self.comps.iter().find(|c| c.name == name)
    }

    pub fn has(&self, name: &str) -> bool {
        self.props.iter().any(|p| p.name == name)
    }

    pub fn prop(&self, name: &str) -> Option<&Property> {
        self.props.iter().find(|p| p.name == name)
    }

    pub fn props_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> + 'a {
        self.props.iter().filter(move |p| p.name == name)
    }

    /// `getFirstPropertyValue` for string-valued properties.
    pub fn first_value(&self, name: &str) -> Option<String> {
        self.prop(name).and_then(Property::first_str)
    }

    pub fn remove_all(&mut self, name: &str) {
        self.props.retain(|p| p.name != name);
    }

    /// `updatePropertyWithValue(name, text)` for a text value.
    pub fn update_text(&mut self, name: &str, value: &str) {
        match self.props.iter_mut().find(|p| p.name == name) {
            Some(p) => p.values = vec![PValue::Str(value.to_owned())],
            None => self.props.push(Property {
                name: name.to_owned(),
                params: Vec::new(),
                vtype: default_type(name).to_owned(),
                values: vec![PValue::Str(value.to_owned())],
            }),
        }
    }
}

// ── design set ───────────────────────────────────────────────────────────────────────

/// Default value type of an icalendar property (`unknown` for unknown properties).
pub fn default_type(name: &str) -> &'static str {
    match name {
        "categories" | "version" | "uid" | "action" | "calscale" | "class" | "comment" | "contact" | "description"
        | "location" | "method" | "prodid" | "related-to" | "resources" | "request-status" | "status" | "summary"
        | "transp" | "tzid" | "tzname" => "text",
        "url" | "attach" | "tzurl" => "uri",
        "attendee" | "organizer" => "cal-address",
        "completed" | "created" | "dtstamp" | "last-modified" => "date-time",
        "dtend" | "dtstart" | "due" | "recurrence-id" | "exdate" | "rdate" => "date-time",
        "duration" | "trigger" => "duration",
        "exrule" | "rrule" => "recur",
        "freebusy" => "period",
        "geo" => "float",
        "percent-complete" | "priority" | "repeat" | "sequence" => "integer",
        "tzoffsetfrom" | "tzoffsetto" => "utc-offset",
        _ => "unknown",
    }
}

fn is_known(name: &str) -> bool {
    default_type(name) != "unknown"
}

fn multi_value(name: &str) -> Option<&'static str> {
    matches!(name, "categories" | "resources" | "exdate" | "rdate" | "freebusy").then_some(",")
}

fn structured_value(name: &str) -> Option<&'static str> {
    matches!(name, "request-status" | "geo").then_some(";")
}

fn param_multi(name: &str) -> bool {
    matches!(name, "delegated-from" | "delegated-to" | "member")
}

/// Text unescape (`\\`, `\;`, `\,`, `\n`/`\N`).
pub fn unescape_text(v: &str) -> String {
    if !v.contains('\\') {
        return v.to_owned();
    }
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                }
                Some(';') => {
                    chars.next();
                    out.push(';');
                }
                Some(',') => {
                    chars.next();
                    out.push(',');
                }
                Some('n' | 'N') => {
                    chars.next();
                    out.push('\n');
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn escape_text(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 4);
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn slice(s: &str, a: usize, b: usize) -> &str {
    let len = s.len();
    let (a, b) = (a.min(len), b.min(len));
    if a >= b || !s.is_char_boundary(a) || !s.is_char_boundary(b) { "" } else { &s[a..b] }
}

pub fn date_from_ical(v: &str) -> String {
    if v.len() >= 15 {
        return date_time_from_ical(v);
    }
    format!("{}-{}-{}", slice(v, 0, 4), slice(v, 4, 6), slice(v, 6, 8))
}

pub fn date_time_from_ical(v: &str) -> String {
    if v.len() == 8 {
        return date_from_ical(v);
    }
    let mut r = format!(
        "{}-{}-{}T{}:{}:{}",
        slice(v, 0, 4),
        slice(v, 4, 6),
        slice(v, 6, 8),
        slice(v, 9, 11),
        slice(v, 11, 13),
        slice(v, 13, 15)
    );
    if v.as_bytes().get(15) == Some(&b'Z') {
        r.push('Z');
    }
    r
}

pub fn date_to_ical(v: &str) -> String {
    match v.len() {
        10 => format!("{}{}{}", slice(v, 0, 4), slice(v, 5, 7), slice(v, 8, 10)),
        n if n >= 19 => date_time_to_ical(v),
        _ => v.to_owned(),
    }
}

pub fn date_time_to_ical(v: &str) -> String {
    match v.len() {
        10 => date_to_ical(v),
        n if n >= 19 => {
            let mut r = format!(
                "{}{}{}{}{}",
                slice(v, 0, 4),
                slice(v, 5, 7),
                slice(v, 8, 13),
                slice(v, 14, 16),
                slice(v, 17, 19)
            );
            if v.as_bytes().get(19) == Some(&b'Z') {
                r.push('Z');
            }
            r
        }
        _ => v.to_owned(),
    }
}

fn utc_offset_from_ical(v: &str) -> String {
    if v.len() < 6 {
        format!("{}:{}", slice(v, 0, 3), slice(v, 3, 5))
    } else {
        format!("{}:{}:{}", slice(v, 0, 3), slice(v, 3, 5), slice(v, 5, 7))
    }
}

fn utc_offset_to_ical(v: &str) -> String {
    if v.len() < 7 {
        format!("{}{}", slice(v, 0, 3), slice(v, 4, 6))
    } else {
        format!("{}{}{}", slice(v, 0, 3), slice(v, 4, 6), slice(v, 7, 9))
    }
}

fn is_duration_string(s: &str) -> bool {
    s.as_bytes().first() == Some(&b'P') || s.as_bytes().get(1) == Some(&b'P')
}

// ── RRULE text ↔ data (Recur._stringToData with fmtIcal = true) ──────────────────────

const DAY_NAMES: [&str; 7] = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];

/// `Recur.icalDayToNumericDay(day, wkst)`.
pub fn ical_day_to_numeric(day: &str, week_start: i32) -> i32 {
    let first = if week_start == 0 { 1 } else { week_start };
    let idx = DAY_NAMES.iter().position(|d| *d == day).map_or(0, |i| i as i32 + 1);
    ((idx - first + 7) % 7) + 1
}

/// `Recur.numericDayToIcalDay(num, wkst)`.
pub fn numeric_to_ical_day(num: i32, week_start: i32) -> &'static str {
    let first = if week_start == 0 { 1 } else { week_start };
    let mut dow = num + first - 1;
    if dow > 7 {
        dow -= 7;
    }
    DAY_NAMES.get((dow - 1).clamp(0, 6) as usize).copied().unwrap_or("SU")
}

fn valid_byday(v: &str) -> bool {
    // ^([+-])?(5[0-3]|[1-4][0-9]|[1-9])?(SU|MO|TU|WE|TH|FR|SA)$
    let b = v.as_bytes();
    if b.len() < 2 {
        return false;
    }
    let (num, day) = v.split_at(v.len() - 2);
    if !DAY_NAMES.contains(&day) {
        return false;
    }
    let num = num.strip_prefix(['+', '-']).unwrap_or(num);
    if num.is_empty() {
        return true;
    }
    match num.parse::<u32>() {
        Ok(n) => num.len() <= 2 && !num.starts_with('0') && (1..=53).contains(&n),
        Err(_) => false,
    }
}

fn numeric_part(kind: &str, min: i64, max: i64, value: &str) -> Result<i64, String> {
    let digits = value.strip_prefix('+').unwrap_or(value);
    let result = js_parse_int(digits).ok_or_else(|| format!("Could not extract integer from \"{digits}\""))?;
    // JS compares the original string numerically: "+5" < 0 is false, "abc" < 0 is false.
    let as_num = value.trim().parse::<f64>().ok();
    if as_num.is_some_and(|n| n < min as f64) || as_num.is_some_and(|n| n > max as f64) {
        return Err(format!("{kind}: invalid value \"{value}\""));
    }
    Ok(result)
}

/// Parse an RRULE value into ordered data. Errors match the cases where ical.js throws.
pub fn parse_recur(s: &str) -> Result<Vec<(String, RecurVal)>, String> {
    let mut dict: Vec<(String, RecurVal)> = Vec::new();
    let set =
        |dict: &mut Vec<(String, RecurVal)>, k: String, v: RecurVal| match dict.iter_mut().find(|(kk, _)| *kk == k) {
            Some(slot) => slot.1 = v,
            None => dict.push((k, v)),
        };
    for part in s.split(';') {
        let mut kv = part.split('=');
        let key = kv.next().unwrap_or("");
        let value = kv.next();
        let uc = key.to_uppercase();
        let lc = key.to_lowercase();
        let by: Option<(i64, i64)> = match uc.as_str() {
            "BYSECOND" => Some((0, 60)),
            "BYMINUTE" => Some((0, 59)),
            "BYHOUR" => Some((0, 23)),
            "BYMONTHDAY" => Some((-31, 31)),
            "BYYEARDAY" => Some((-366, 366)),
            "BYWEEKNO" => Some((-53, 53)),
            "BYMONTH" => Some((1, 12)),
            "BYSETPOS" => Some((-366, 366)),
            _ => None,
        };
        if uc == "BYDAY" || by.is_some() {
            let value = value.ok_or_else(|| format!("invalid {uc} value"))?;
            let mut items: Vec<RecurVal> = Vec::new();
            for p in value.split(',') {
                let item = if uc == "BYDAY" {
                    if !valid_byday(p) {
                        return Err(format!("invalid BYDAY value \"{p}\""));
                    }
                    RecurVal::Str(p.to_owned())
                } else {
                    let (min, max) = by.unwrap_or((0, 0));
                    RecurVal::Int(numeric_part(&uc, min, max, p)?)
                };
                if !items.contains(&item) {
                    items.push(item);
                }
            }
            let v = if items.len() == 1 { items.remove(0) } else { RecurVal::List(items) };
            set(&mut dict, lc, v);
            continue;
        }
        match uc.as_str() {
            "FREQ" => {
                let v = value.unwrap_or("");
                if !["SECONDLY", "MINUTELY", "HOURLY", "DAILY", "WEEKLY", "MONTHLY", "YEARLY"].contains(&v) {
                    return Err(format!("invalid frequency \"{v}\""));
                }
                set(&mut dict, "freq".into(), RecurVal::Str(v.to_owned()));
            }
            "COUNT" => {
                let v = value.and_then(js_parse_int).ok_or("Could not extract integer from COUNT")?;
                set(&mut dict, "count".into(), RecurVal::Int(v));
            }
            "INTERVAL" => {
                let v = value.and_then(js_parse_int).ok_or("Could not extract integer from INTERVAL")?;
                set(&mut dict, "interval".into(), RecurVal::Int(v.max(1)));
            }
            "UNTIL" => {
                let v = value.unwrap_or("");
                let u = if v.len() > 10 { date_time_from_ical(v) } else { date_from_ical(v) };
                set(&mut dict, "until".into(), RecurVal::Str(u));
            }
            "WKST" => {
                let v = value.unwrap_or("");
                if !DAY_NAMES.contains(&v) {
                    return Err(format!("invalid WKST value \"{v}\""));
                }
                set(&mut dict, "wkst".into(), RecurVal::Int(i64::from(ical_day_to_numeric(v, 1))));
            }
            _ => set(&mut dict, lc, RecurVal::Str(value.unwrap_or("undefined").to_owned())),
        }
    }
    Ok(dict)
}

fn recur_val_to_string(v: &RecurVal) -> String {
    match v {
        RecurVal::Str(s) => s.clone(),
        RecurVal::Int(i) => i.to_string(),
        RecurVal::List(items) => items.iter().map(recur_val_to_string).collect::<Vec<_>>().join(","),
    }
}

/// `design.icalendar.value.recur.toICAL(data)`.
pub fn recur_to_ical(data: &[(String, RecurVal)]) -> String {
    data.iter()
        .map(|(k, v)| {
            let val = match (k.as_str(), v) {
                ("until", RecurVal::Str(s)) => {
                    if s.len() > 10 {
                        date_time_to_ical(s)
                    } else {
                        date_to_ical(s)
                    }
                }
                ("wkst", RecurVal::Int(n)) => numeric_to_ical_day(*n as i32, 1).to_owned(),
                _ => recur_val_to_string(v),
            };
            format!("{}={val}", k.to_uppercase())
        })
        .collect::<Vec<_>>()
        .join(";")
}

// ── parser ───────────────────────────────────────────────────────────────────────────

fn rfc6868_escape(v: &str) -> String {
    if !v.contains('^') {
        return v.to_owned();
    }
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '^' {
            match chars.peek() {
                Some('\'') => {
                    chars.next();
                    out.push('"');
                }
                Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('^') => {
                    chars.next();
                    out.push('^');
                }
                _ => out.push('^'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unescaped_index_of(buf: &str, search: &str, from: usize) -> Option<usize> {
    let mut pos = from;
    while let Some(rel) = buf.get(pos..).and_then(|s| s.find(search)) {
        let at = pos + rel;
        if at > 0 && buf.as_bytes()[at - 1] == b'\\' {
            pos = at + 1;
        } else {
            return Some(at);
        }
    }
    None
}

fn parse_value(value: &str, vtype: &str) -> Result<PValue, String> {
    Ok(match vtype {
        "text" => PValue::Str(unescape_text(value)),
        "date" => PValue::Str(date_from_ical(value)),
        "date-time" => PValue::Str(date_time_from_ical(value)),
        "utc-offset" => PValue::Str(utc_offset_from_ical(value)),
        "boolean" => PValue::Bool(value == "TRUE"),
        "integer" => PValue::Int(js_parse_int(value).unwrap_or(0)),
        "float" => PValue::Float(super::js_parse_float(value).unwrap_or(0.0)),
        "recur" => PValue::Recur(parse_recur(value)?),
        "period" => {
            let mut parts = value.splitn(2, '/');
            let a = date_time_from_ical(parts.next().unwrap_or(""));
            let b = parts.next().unwrap_or("");
            let b = if is_duration_string(b) { b.to_owned() } else { date_time_from_ical(b) };
            PValue::Period(a, b)
        }
        "time" => {
            if value.len() < 6 {
                PValue::Str(value.to_owned())
            } else {
                let mut r = format!("{}:{}:{}", slice(value, 0, 2), slice(value, 2, 4), slice(value, 4, 6));
                if value.as_bytes().get(6) == Some(&b'Z') {
                    r.push('Z');
                }
                PValue::Str(r)
            }
        }
        _ => PValue::Str(value.to_owned()),
    })
}

fn parse_multi(buf: &str, delim: &str, vtype: &str) -> Result<Vec<PValue>, String> {
    let mut out = Vec::new();
    let mut last = 0;
    while let Some(pos) = unescaped_index_of(buf, delim, last) {
        out.push(parse_value(&buf[last..pos], vtype)?);
        last = pos + delim.len();
    }
    out.push(parse_value(&buf[last..], vtype)?);
    Ok(out)
}

fn param_value(raw: &str, name: &str) -> ParamVal {
    let value = rfc6868_escape(raw);
    if param_multi(name) {
        // `"a","b"` style lists; the surrounding quotes were already stripped.
        let parts: Vec<String> = value.split("\",\"").map(str::to_owned).collect();
        if parts.len() == 1 {
            return ParamVal::One(parts.into_iter().next().unwrap_or_default());
        }
        return ParamVal::Many(parts);
    }
    ParamVal::One(unescape_text(&value))
}

/// `parse._parseParameters` → (params, last value byte length, last value position).
fn parse_parameters(line: &str) -> Result<(Vec<(String, ParamVal)>, usize, Option<usize>), String> {
    let b = line.as_bytes();
    let mut result: Vec<(String, ParamVal)> = Vec::new();
    let mut last_param = 0usize;
    let mut pos = 0usize;
    let mut value_pos: Option<usize> = None;
    let mut last_len = 0usize;
    loop {
        let Some(eq) = line.get(pos + 1..).and_then(|s| s.find('=')).map(|i| i + pos + 1) else { break };
        let name = line.get(last_param + 1..eq).unwrap_or("");
        if name.is_empty() {
            return Err(format!("Empty parameter name in '{line}'"));
        }
        let lc = name.to_lowercase();
        let raw_value;
        let mut stop = false;
        if b.get(eq + 1) == Some(&b'"') {
            let vp = eq + 2;
            let Some(mut close) = line.get(vp..).and_then(|s| s.find('"')).map(|i| i + vp) else {
                return Err(format!("invalid line (no matching double quote) \"{line}\""));
            };
            if param_multi(&lc) {
                while b.get(close + 1) == Some(&b',') && b.get(close + 2) == Some(&b'"') {
                    match line.get(close + 3..).and_then(|s| s.find('"')) {
                        Some(i) => close = close + 3 + i,
                        None => return Err(format!("invalid line (no matching double quote) \"{line}\"")),
                    }
                }
            }
            raw_value = &line[vp..close];
            value_pos = Some(vp);
            let next_param = line[close..].find(';').map(|i| i + close);
            let prop_value = line[close..].find(':').map(|i| i + close);
            match next_param {
                Some(np) if prop_value.is_none_or(|pv| np < pv) => {
                    last_param = np;
                    pos = close;
                }
                _ => stop = true,
            }
        } else {
            let vp = eq + 1;
            let next = line[vp..].find(';').map(|i| i + vp);
            let prop_value = line[vp..].find(':').map(|i| i + vp);
            let end;
            match (next, prop_value) {
                (Some(n), Some(pv)) if n > pv => {
                    end = pv;
                    stop = true;
                }
                (None, pv) => {
                    end = pv.unwrap_or(line.len());
                    stop = true;
                }
                (Some(n), _) => {
                    end = n;
                    last_param = n;
                    pos = n;
                }
            }
            raw_value = &line[vp..end];
            value_pos = Some(vp);
        }
        let before = raw_value.len();
        let escaped = rfc6868_escape(raw_value);
        if let Some(vp) = value_pos.as_mut() {
            *vp += before - escaped.len().min(before);
        }
        last_len = escaped.len();
        let value = param_value(raw_value, &lc);
        match result.iter_mut().find(|(k, _)| *k == lc) {
            Some(slot) if param_multi(&lc) => {
                let mut list = match std::mem::replace(&mut slot.1, ParamVal::Many(Vec::new())) {
                    ParamVal::One(s) => vec![s],
                    ParamVal::Many(v) => v,
                };
                match value {
                    ParamVal::One(s) => list.push(s),
                    ParamVal::Many(v) => list.extend(v),
                }
                slot.1 = ParamVal::Many(list);
            }
            Some(slot) => slot.1 = value,
            None => result.push((lc, value)),
        }
        if stop {
            break;
        }
    }
    Ok((result, last_len, value_pos))
}

enum Step {
    Begin(String),
    End,
    Prop(Property),
}

fn handle_line(line: &str) -> Result<Step, String> {
    let value_pos = line.find(':');
    let mut param_pos = line.find(';');
    if let (Some(p), Some(v)) = (param_pos, value_pos)
        && p > v
    {
        param_pos = None;
    }
    let (name, mut params, value) = if let Some(pp) = param_pos {
        let name = line[..pp].to_lowercase();
        let (params, last_len, vpos) = parse_parameters(&line[pp..])?;
        let Some(vpos) = vpos else { return Err(format!("Invalid parameters in '{line}'")) };
        let last_index = last_len + vpos + pp;
        let rest = line.get(last_index..).unwrap_or("");
        let Some(colon) = rest.find(':') else { return Err(format!("Missing parameter value in '{line}'")) };
        (name, params, line[last_index + colon + 1..].to_owned())
    } else if let Some(vp) = value_pos {
        let name = line[..vp].to_lowercase();
        let value = line[vp + 1..].to_owned();
        if name == "begin" {
            return Ok(Step::Begin(value.to_lowercase()));
        }
        if name == "end" {
            return Ok(Step::End);
        }
        (name, Vec::new(), value)
    } else {
        return Err(format!("invalid line (no token \";\" or \":\") \"{line}\""));
    };
    let mut vtype = if !value.is_empty() && name == "rdate" {
        Some(
            if value.contains('/') {
                "period"
            } else if value.contains('T') {
                "date-time"
            } else {
                "date"
            }
            .to_owned(),
        )
    } else {
        None
    };
    if vtype.is_none() {
        vtype = Some(match params.iter().find(|(k, _)| k == "value") {
            Some((_, v)) => v.first().to_lowercase(),
            None => default_type(&name).to_owned(),
        });
    }
    let vtype = vtype.unwrap_or_default();
    params.retain(|(k, _)| k != "value");
    let known = is_known(&name);
    let values = match (known.then(|| multi_value(&name)).flatten(), known.then(|| structured_value(&name)).flatten()) {
        (Some(delim), _) => parse_multi(&value, delim, &vtype)?,
        (None, Some(delim)) => vec![PValue::Structured(parse_multi(&value, delim, &vtype)?)],
        (None, None) => vec![parse_value(&value, &vtype)?],
    };
    Ok(Step::Prop(Property { name, params, vtype, values }))
}

/// Logical lines (`parse._eachLine`): unfold continuation lines, skip empty lines, trim the last.
fn each_line(buffer: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let start = buffer.find(|c| c != ' ' && c != '\t').unwrap_or(buffer.len());
    let mut current: Option<String> = None;
    for raw in buffer[start..].split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let first = raw.chars().next();
        if matches!(first, Some(' ' | '\t')) {
            match current.as_mut() {
                Some(c) => c.push_str(&raw[1..]),
                None => current = Some(raw[1..].to_owned()),
            }
        } else {
            if let Some(c) = current.take()
                && !c.is_empty()
            {
                lines.push(c);
            }
            current = Some(raw.to_owned());
        }
    }
    if let Some(c) = current {
        let t = c.trim();
        if !t.is_empty() {
            lines.push(t.to_owned());
        }
    }
    lines
}

/// `ICAL.parse`: top-level components. Errors where ical.js throws.
pub fn parse(input: &str) -> Result<Vec<Component>, String> {
    let mut roots: Vec<Component> = Vec::new();
    let mut stack: Vec<Component> = Vec::new();
    // ical.js pops its root on a stray END: that is tolerated at the very end, but any later
    // line then fails.
    let mut root_popped = false;
    for line in each_line(input) {
        let step = handle_line(&line)?;
        if root_popped {
            return Err("content after the root component ended".into());
        }
        match step {
            Step::Begin(name) => stack.push(Component::new(&name)),
            Step::End => match stack.pop() {
                Some(done) => match stack.last_mut() {
                    Some(parent) => parent.comps.push(done),
                    None => roots.push(done),
                },
                None => root_popped = true,
            },
            Step::Prop(p) => match stack.last_mut() {
                Some(c) => c.props.push(p),
                None => return Err("property outside of a component".into()),
            },
        }
    }
    if !stack.is_empty() {
        return Err("invalid ical body. component began but did not end".into());
    }
    Ok(roots)
}

/// `new ICAL.Component(ICAL.parse(raw))` for a single top-level component.
pub fn parse_component(input: &str) -> Result<Component, String> {
    let mut roots = parse(input)?;
    if roots.len() == 1 { Ok(roots.remove(0)) } else { Ok(Component::default()) }
}

// ── serialiser ───────────────────────────────────────────────────────────────────────

fn rfc6868_unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '"' => out.push_str("^'"),
            '\n' => out.push_str("^n"),
            '^' => out.push_str("^^"),
            _ => out.push(c),
        }
    }
    out
}

fn quote_param(v: &str, force: bool) -> String {
    if !force && !v.contains([',', ':', ';']) { v.to_owned() } else { format!("\"{v}\"") }
}

fn value_to_ical(v: &PValue, vtype: &str) -> String {
    match v {
        PValue::Str(s) => match vtype {
            "text" => escape_text(s),
            "date" => date_to_ical(s),
            "date-time" => date_time_to_ical(s),
            "utc-offset" => utc_offset_to_ical(s),
            "time" if s.len() >= 8 => {
                let mut r = format!("{}{}{}", slice(s, 0, 2), slice(s, 3, 5), slice(s, 6, 8));
                if s.as_bytes().get(8) == Some(&b'Z') {
                    r.push('Z');
                }
                r
            }
            _ => s.clone(),
        },
        PValue::Int(i) => i.to_string(),
        PValue::Float(f) => {
            if f.fract() == 0.0 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        PValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        PValue::Recur(d) => recur_to_ical(d),
        PValue::Period(a, b) => {
            let a = if a.len() == 10 { date_to_ical(a) } else { date_time_to_ical(a) };
            let b = if is_duration_string(b) {
                b.clone()
            } else if b.len() == 10 {
                date_to_ical(b)
            } else {
                date_time_to_ical(b)
            };
            format!("{a}/{b}")
        }
        PValue::Structured(parts) => parts.iter().map(|p| value_to_ical(p, vtype)).collect::<Vec<_>>().join(";"),
    }
}

/// `helpers.foldline`: fold at 75 UTF-8 octets.
fn fold_line(line: &str) -> String {
    let mut result = String::new();
    let mut rest = line;
    while !rest.is_empty() {
        let mut bytes = 0;
        let mut cut = rest.len();
        for (i, c) in rest.char_indices() {
            bytes += c.len_utf8();
            if bytes >= 76 {
                cut = i;
                break;
            }
        }
        result.push_str("\r\n ");
        result.push_str(&rest[..cut]);
        rest = &rest[cut..];
    }
    result.get(3..).unwrap_or("").to_owned()
}

pub fn stringify_property(p: &Property) -> String {
    let mut line = p.name.to_uppercase();
    for (k, v) in &p.params {
        let value = match v {
            ParamVal::Many(items) if param_multi(k) => {
                items.iter().map(|i| quote_param(&rfc6868_unescape(i), true)).collect::<Vec<_>>().join(",")
            }
            other => quote_param(&rfc6868_unescape(other.first()), false),
        };
        line.push(';');
        line.push_str(&k.to_uppercase());
        line.push('=');
        line.push_str(&value);
    }
    if p.values.is_empty() {
        return format!("{line}:");
    }
    let is_default = if is_known(&p.name) { p.vtype == default_type(&p.name) } else { p.vtype == "unknown" };
    if !is_default {
        line.push_str(";VALUE=");
        line.push_str(&p.vtype.to_uppercase());
    }
    line.push(':');
    let delim = if is_known(&p.name) { multi_value(&p.name) } else { None };
    let body = match delim {
        Some(d) => p.values.iter().map(|v| value_to_ical(v, &p.vtype)).collect::<Vec<_>>().join(d),
        None => p.values.first().map(|v| value_to_ical(v, &p.vtype)).unwrap_or_default(),
    };
    line.push_str(&body);
    fold_line(&line)
}

/// `stringify.component` (no trailing line ending).
pub fn stringify_component(c: &Component) -> String {
    let name = c.name.to_uppercase();
    let mut out = format!("BEGIN:{name}\r\n");
    for p in &c.props {
        out.push_str(&stringify_property(p));
        out.push_str("\r\n");
    }
    for sub in &c.comps {
        out.push_str(&stringify_component(sub));
        out.push_str("\r\n");
    }
    out.push_str(&format!("END:{name}"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serialises() {
        let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Hello\\, world\r\nDTSTART;TZID=\"Europe/Berlin\":20260105T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20260301T000000Z\r\nEXDATE;VALUE=DATE:20260107,20260109\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let cal = parse_component(raw).unwrap();
        let ev = cal.first_subcomponent("vevent").unwrap();
        assert_eq!(ev.first_value("summary").as_deref(), Some("Hello, world"));
        let dt = ev.prop("dtstart").unwrap();
        assert_eq!(dt.param("tzid"), Some("Europe/Berlin"));
        assert_eq!(dt.first_str().as_deref(), Some("2026-01-05T10:00:00"));
        assert_eq!(ev.prop("exdate").unwrap().values.len(), 2);
        let out = stringify_component(&cal);
        assert!(out.contains("SUMMARY:Hello\\, world\r\n"));
        assert!(out.contains("DTSTART;TZID=Europe/Berlin:20260105T100000\r\n"));
        assert!(out.contains("RRULE:FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20260301T000000Z\r\n"));
        assert!(out.contains("EXDATE;VALUE=DATE:20260107,20260109\r\n"));
        assert!(out.ends_with("END:VCALENDAR"));
    }

    #[test]
    fn rejects_what_ical_js_rejects() {
        assert!(parse("BEGIN:VEVENT\r\nRRULE:FREQ=NOTAREALFREQUENCY\r\nEND:VEVENT").is_err());
        assert!(parse("BEGIN:VEVENT\r\nATTENDEE;RSVP:mailto:x\r\nEND:VEVENT").is_err());
        assert!(parse("BEGIN:VEVENT\r\nnonsense\r\nEND:VEVENT").is_err());
        assert!(parse("BEGIN:VEVENT\r\nUID:x").is_err());
    }
}
