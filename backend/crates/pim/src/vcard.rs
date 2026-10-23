//! vCard 3.0 parsing, generation and merging (port of `utils/vcard.ts`).

use serde::Serialize;
use serde_json::Value;

use inboxora_mail::text::js_trim;
use inboxora_mail::util::safe_image_type;

fn escape_param(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\r' | '\n' | ';')).collect()
}

/// A contact-date label: trimmed, non-empty, no control characters, quotes or backslashes.
pub fn normalize_contact_date_label(value: &str) -> Option<String> {
    let label = js_trim(value);
    (!label.is_empty() && !label.chars().any(|c| c.is_control() || c == '"' || c == '\\')).then(|| label.to_owned())
}

pub fn escape_value(s: &str) -> String {
    s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n").replace('\r', "")
}

pub fn unescape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&n) = chars.peek()
            && matches!(n, '\\' | ';' | ',' | 'n' | 'N')
        {
            chars.next();
            out.push(if n == 'n' || n == 'N' { '\n' } else { n });
            continue;
        }
        out.push(c);
    }
    out
}

fn split_escaped(value: &str, delimiter: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut escaped = false;
    for c in value.chars() {
        if c == delimiter && !escaped {
            parts.push(std::mem::take(&mut part));
        } else {
            part.push(c);
        }
        escaped = if c == '\\' { !escaped } else { false };
    }
    parts.push(part);
    parts
}

fn is_http_url(v: &str) -> bool {
    url::Url::parse(v).is_ok_and(|u| u.scheme() == "http" || u.scheme() == "https")
}

/// `YYYY-MM-DD`, `YYYYMMDD` or (with `allow_partial`) `--MM-DD`, validated as a real date.
pub fn normalize_vcard_date(value: &str, allow_partial: bool) -> Option<String> {
    let date = unescape_value(value);
    let date = js_trim(&date);
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    // Every accepted form is ASCII digits and dashes; checking first keeps slicing safe.
    if !date.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return None;
    }
    if let Some(rest) = date.strip_prefix("--") {
        let (m, d) = if rest.len() == 5 && rest.as_bytes()[2] == b'-' {
            (&rest[..2], &rest[3..])
        } else if rest.len() == 4 {
            (&rest[..2], &rest[2..])
        } else {
            ("", "")
        };
        if digits(m) && digits(d) {
            if !allow_partial {
                return None;
            }
            return normalize_vcard_date(&format!("2000-{m}-{d}"), false).map(|_| format!("--{m}-{d}"));
        }
    }
    let (y, m, d) = if date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-' {
        (&date[..4], &date[5..7], &date[8..10])
    } else if date.len() == 8 {
        (&date[..4], &date[4..6], &date[6..8])
    } else {
        return None;
    };
    if !(digits(y) && digits(m) && digits(d)) {
        return None;
    }
    let (year, month, day): (i32, u32, u32) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let dim = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if year < 1 || !(1..=12).contains(&month) || day < 1 || day > dim {
        return None;
    }
    Some(format!("{y}-{m}-{d}"))
}

/// Fold a content line at 75 octets (74 after the leading space), never splitting UTF-8.
pub fn fold_line(line: &str) -> String {
    let bytes = line.as_bytes();
    if bytes.len() <= 75 {
        return format!("{line}\r\n");
    }
    let mut parts = Vec::new();
    let mut offset = 0;
    let mut first = true;
    while offset < bytes.len() {
        let max = if first { 75 } else { 74 };
        let mut end = offset + max;
        if end >= bytes.len() {
            end = bytes.len();
        } else {
            while end > offset && (bytes[end] & 0xC0) == 0x80 {
                end -= 1;
            }
            if end == offset {
                end = offset + max;
                while end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
                    end += 1;
                }
            }
        }
        let chunk = String::from_utf8_lossy(&bytes[offset..end]);
        parts.push(if first { chunk.into_owned() } else { format!(" {chunk}") });
        offset = end;
        first = false;
    }
    format!("{}\r\n", parts.join("\r\n"))
}

pub fn unfold(raw: &str) -> String {
    raw.replace("\r\n ", "").replace("\r\n\t", "").replace("\n ", "").replace("\n\t", "")
}

fn split_lines(s: &str) -> impl Iterator<Item = &str> {
    s.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l))
}

/// Index of the `:` separating property name/params from the value (quotes respected).
pub fn find_property_separator(line: &str) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if c == '"' && !escaped {
            quoted = !quoted;
        }
        if c == ':' && !quoted {
            return Some(i);
        }
        escaped = c == '\\' && !escaped;
        if c != '\\' {
            escaped = false;
        }
    }
    None
}

/// Position just after the first `TYPE=`/`LABEL=` parameter at a parameter boundary.
fn label_param_start(params: &str, quoted_only: bool) -> Option<usize> {
    let lower = params.to_ascii_lowercase();
    let mut pos = 0;
    while pos <= lower.len() {
        let at_boundary = pos == 0 || lower.as_bytes()[pos - 1] == b';';
        if at_boundary {
            for key in ["type=", "label="] {
                if lower[pos..].starts_with(key) {
                    let after = pos + key.len();
                    if !quoted_only || lower[after..].starts_with('"') {
                        return Some(after + usize::from(quoted_only));
                    }
                }
            }
        }
        match lower[pos..].find(';') {
            Some(i) => pos += i + 1,
            None => break,
        }
    }
    None
}

fn date_label_from_params(params: &str, fallback: &str) -> Option<String> {
    let Some(start) = label_param_start(params, false) else { return Some(fallback.to_owned()) };
    let value = &params[start..];
    if !value.starts_with('"') {
        return Some(value.split(';').next().unwrap_or("").to_owned());
    }
    for (i, c) in value.char_indices().skip(1) {
        if c == '\\' {
            return None;
        }
        if c == '"' {
            let rest = &value[i + 1..];
            return (rest.starts_with(';') || i == value.len() - 1).then(|| value[1..i].to_owned());
        }
    }
    None
}

fn has_unterminated_date_label_param(raw: &str) -> bool {
    split_lines(&unfold(raw)).any(|line| {
        let first = line.split(';').next().unwrap_or("").to_uppercase();
        let property = first.rsplit('.').next().unwrap_or("");
        if !matches!(property, "BDAY" | "ANNIVERSARY" | "X-ABDATE") {
            return false;
        }
        label_param_start(line, true).is_some_and(|start| !line[start..].contains('"'))
    })
}

/// `params.match(/TYPE=([^;]+)/i)?.[1]` — the first `TYPE=` (anywhere) followed by a value.
fn type_param(params: &str) -> Option<String> {
    let lower = params.to_ascii_lowercase();
    lower.match_indices("type=").find_map(|(i, _)| {
        let v = params[i + 5..].split(';').next().unwrap_or("");
        (!v.is_empty()).then(|| v.to_owned())
    })
}

fn clean_type(v: &str) -> String {
    v.to_lowercase().replace(['"', '\''], "")
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Email {
    pub value: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Typed {
    pub value: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Address {
    #[serde(rename = "type")]
    pub kind: String,
    pub pobox: String,
    pub extended: String,
    pub street: String,
    pub locality: String,
    pub region: String,
    pub postal_code: String,
    pub country: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ContactDate {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedVCard {
    pub uid: Option<String>,
    pub display_name: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub emails: Vec<Email>,
    pub phones: Vec<Typed>,
    pub organization: Option<String>,
    pub notes: Option<String>,
    pub photo_data: Option<String>,
    pub birthday: Option<String>,
    pub anniversary: Option<String>,
    pub title: Option<String>,
    pub role: Option<String>,
    pub nickname: Option<String>,
    pub urls: Vec<Typed>,
    pub instant_messages: Vec<Typed>,
    pub categories: Vec<String>,
    pub addresses: Vec<Address>,
    pub contact_dates: Vec<ContactDate>,
    pub invalid_dates: Vec<String>,
    pub invalid_date_labels: Vec<String>,
}

fn non_empty(s: &str) -> Option<String> {
    let t = js_trim(s);
    (!t.is_empty()).then(|| t.to_owned())
}

/// Parse a vCard into the fields Inboxora uses; unknown properties are ignored.
pub fn parse(raw: &str) -> ParsedVCard {
    let text = unfold(raw);
    let mut r = ParsedVCard::default();
    if has_unterminated_date_label_param(raw) {
        r.invalid_date_labels.push("unterminated parameter".into());
    }
    let add_date = |r: &mut ParsedVCard, label: Option<String>, value: &str| {
        let normalized = normalize_vcard_date(value, true);
        let Some(clean) = label.as_deref().and_then(normalize_contact_date_label) else {
            r.invalid_date_labels.push(label.unwrap_or_default());
            return;
        };
        let Some(normalized) = normalized else {
            let raw_value = unescape_value(value);
            let t = js_trim(&raw_value);
            if !t.is_empty() {
                r.invalid_dates.push(t.to_owned());
            }
            return;
        };
        let lower = clean.to_lowercase();
        if !r.contact_dates.iter().any(|d| d.label.to_lowercase() == lower && d.value == normalized) {
            r.contact_dates.push(ContactDate { label: clean, value: normalized });
        }
    };

    // Apple-style grouped labels: `item1.X-ABLABEL:Wedding`.
    let mut grouped: Vec<(String, String)> = Vec::new();
    for line in split_lines(&text) {
        let trimmed = js_trim(line);
        let Some(colon) = find_property_separator(trimmed) else { continue };
        let property = trimmed[..colon].split(';').next().unwrap_or("");
        let Some(dot) = property.find('.') else { continue };
        if property[dot + 1..].to_uppercase() != "X-ABLABEL" {
            continue;
        }
        let key = property[..dot].to_lowercase();
        let value = js_trim(&unescape_value(&trimmed[colon + 1..])).to_owned();
        match grouped.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => grouped.push((key, value)),
        }
    }

    let mut preferred: Option<usize> = None;
    let mut preferred_rank = u32::MAX;
    for line in split_lines(&text) {
        let trimmed = js_trim(line);
        if trimmed.is_empty() || trimmed == "BEGIN:VCARD" || trimmed == "END:VCARD" {
            continue;
        }
        let Some(colon) = find_property_separator(trimmed) else { continue };
        let raw_name = &trimmed[..colon];
        let value = &trimmed[colon + 1..];
        let property = raw_name.split(';').next().unwrap_or("");
        let group = property.find('.').map(|d| property[..d].to_lowercase());
        let mut name = property.to_uppercase();
        if let Some(d) = name.rfind('.') {
            name = name[d + 1..].to_owned();
        }
        let params = raw_name.find(';').map_or("", |i| &raw_name[i + 1..]);
        match name.as_str() {
            "UID" => r.uid = Some(js_trim(&unescape_value(value)).to_owned()),
            "FN" => r.display_name = non_empty(&unescape_value(value)),
            "N" => {
                let parts: Vec<String> =
                    split_escaped(value, ';').iter().map(|p| js_trim(&unescape_value(p)).to_owned()).collect();
                r.last_name = parts.first().filter(|s| !s.is_empty()).cloned();
                r.first_name = parts.get(1).filter(|s| !s.is_empty()).cloned();
            }
            "EMAIL" => {
                let email = js_trim(&unescape_value(value)).to_lowercase();
                if !email.is_empty() {
                    let kind = type_param(params).map_or_else(|| "other".to_owned(), |t| clean_type(&t));
                    let rank = pref_rank(params);
                    if preferred.is_none() || rank < preferred_rank {
                        preferred = Some(r.emails.len());
                        preferred_rank = rank;
                    }
                    let kind = kind.split(',').filter(|p| *p != "pref").collect::<Vec<_>>().join(",");
                    r.emails.push(Email {
                        value: email,
                        kind: if kind.is_empty() { "other".into() } else { kind },
                        primary: false,
                    });
                }
            }
            "TEL" => {
                let v = unescape_value(value);
                let v = js_trim(&v);
                let phone = if v.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("tel:")) { &v[4..] } else { v };
                if !phone.is_empty() {
                    let kind = type_param(params).map_or_else(|| "other".to_owned(), |t| clean_type(&t));
                    r.phones.push(Typed { value: phone.to_owned(), kind });
                }
            }
            "ORG" => r.organization = non_empty(&unescape_value(&split_escaped(value, ';')[0])),
            "NOTE" => r.notes = non_empty(&unescape_value(value)),
            "BDAY" => {
                if r.birthday.is_none() {
                    r.birthday = normalize_vcard_date(value, false);
                }
                add_date(&mut r, date_label_from_params(params, "Birthday"), value);
            }
            "ANNIVERSARY" => {
                if r.anniversary.is_none() {
                    r.anniversary = normalize_vcard_date(value, false);
                }
                add_date(&mut r, date_label_from_params(params, "Anniversary"), value);
            }
            "X-ABDATE" => {
                let grouped_label = group
                    .as_ref()
                    .and_then(|g| grouped.iter().find(|(k, _)| k == g))
                    .map(|(_, v)| v.clone())
                    .filter(|v| !v.is_empty());
                let label = grouped_label.or_else(|| date_label_from_params(params, "Other"));
                add_date(&mut r, label, value);
            }
            "X-ANDROID-CUSTOM" => {
                let parts: Vec<String> = split_escaped(value, ';').iter().map(|p| unescape_value(p)).collect();
                if parts.first().map(String::as_str) == Some("vnd.android.cursor.item/contact_event") {
                    let get = |i: usize| parts.get(i).cloned().unwrap_or_default();
                    let date_first = normalize_vcard_date(&get(1), true).is_some();
                    let (label, v) = if date_first { (get(3), get(1)) } else { (get(2), get(3)) };
                    add_date(&mut r, parts.get(if date_first { 3 } else { 2 }).map(|_| label), &v);
                }
            }
            "TITLE" => r.title = non_empty(&unescape_value(value)),
            "ROLE" => r.role = non_empty(&unescape_value(value)),
            "NICKNAME" => r.nickname = non_empty(&unescape_value(value)),
            "URL" => {
                let url = js_trim(&unescape_value(value)).to_owned();
                if is_http_url(&url) {
                    r.urls.push(Typed {
                        value: url,
                        kind: clean_type(&type_param(params).unwrap_or_else(|| "other".into())),
                    });
                }
            }
            "IMPP" => {
                let im = js_trim(&unescape_value(value)).to_owned();
                if !im.is_empty() {
                    let scheme = im.split(':').next().unwrap_or("").to_owned();
                    let kind = type_param(params).unwrap_or(if scheme.is_empty() { "other".into() } else { scheme });
                    r.instant_messages.push(Typed { value: im, kind: clean_type(&kind) });
                }
            }
            "CATEGORIES" => {
                r.categories.extend(
                    split_escaped(value, ',')
                        .iter()
                        .map(|p| js_trim(&unescape_value(p)).to_owned())
                        .filter(|s| !s.is_empty()),
                );
            }
            "ADR" => {
                let parts: Vec<String> =
                    split_escaped(value, ';').iter().map(|p| js_trim(&unescape_value(p)).to_owned()).collect();
                let get = |i: usize| parts.get(i).cloned().unwrap_or_default();
                r.addresses.push(Address {
                    kind: clean_type(&type_param(params).unwrap_or_else(|| "other".into())),
                    pobox: get(0),
                    extended: get(1),
                    street: get(2),
                    locality: get(3),
                    region: get(4),
                    postal_code: get(5),
                    country: get(6),
                });
            }
            "PHOTO" => {
                let v = js_trim(value);
                if v.is_empty() {
                    continue;
                }
                if let Some(data) = v.strip_prefix("data:") {
                    let mime = data.split([';', ',']).next().unwrap_or("");
                    r.photo_data = safe_image_type(Some(mime)).map(|_| v.to_owned());
                } else if v.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("http://"))
                    || v.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))
                {
                    r.photo_data = None;
                } else {
                    let raw_type = type_param(params)
                        .map_or_else(|| "JPEG".to_owned(), |t| t.replace(['"', '\''], "").to_uppercase());
                    let mime = match raw_type.as_str() {
                        "PNG" => "image/png",
                        "GIF" => "image/gif",
                        "WEBP" => "image/webp",
                        _ => "image/jpeg",
                    };
                    r.photo_data = Some(format!("data:{mime};base64,{v}"));
                }
            }
            _ => {}
        }
    }
    if let Some(i) = preferred {
        r.emails[i].primary = true;
    }
    r
}

/// EMAIL preference rank: `PREF=n` (1–100), a bare `PREF` parameter/type counts as 1.
fn pref_rank(params: &str) -> u32 {
    let lower = params.to_ascii_lowercase();
    // (?:^|;)PREF="?(\d+)
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("pref=") {
        let at = pos + i;
        if at == 0 || lower.as_bytes()[at - 1] == b';' {
            let rest = lower[at + 5..].trim_start_matches('"');
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse::<u32>() {
                return if (1..=100).contains(&n) { n } else { bare_pref(&lower) };
            }
        }
        pos = at + 5;
    }
    bare_pref(&lower)
}

fn bare_pref(lower: &str) -> u32 {
    let bytes = lower.as_bytes();
    let standalone = lower.match_indices("pref").any(|(i, _)| {
        let before_ok = i == 0 || matches!(bytes[i - 1], b';' | b',');
        let after = bytes.get(i + 4);
        before_ok && after.is_none_or(|b| matches!(b, b',' | b';'))
    });
    // TYPE=[^;]*\bPREF\b
    let in_type = lower.match_indices("type=").any(|(i, _)| {
        let seg = lower[i + 5..].split(';').next().unwrap_or("");
        seg.match_indices("pref").any(|(j, _)| {
            let sb = seg.as_bytes();
            let wb = |b: Option<&u8>| b.is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'));
            wb(if j == 0 { None } else { sb.get(j - 1) }) && wb(sb.get(j + 4))
        })
    });
    if standalone || in_type { 1 } else { u32::MAX }
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty())
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// Generate a vCard 3.0 from a contact (`generateVCard`).
pub fn generate(contact: &Value) -> String {
    let mut lines = vec!["BEGIN:VCARD".to_owned(), "VERSION:3.0".to_owned()];
    lines.push(format!("UID:{}", escape_value(s(contact, "uid").unwrap_or(""))));
    let (first, last) = (s(contact, "firstName"), s(contact, "lastName"));
    let emails = arr(contact, "emails");
    let fn_ = s(contact, "displayName").map(str::to_owned).or_else(|| {
        (first.is_some() || last.is_some()).then(|| [first, last].into_iter().flatten().collect::<Vec<_>>().join(" "))
    });
    let fn_ = fn_.unwrap_or_else(|| {
        emails.first().and_then(|e| e.get("value")).and_then(Value::as_str).unwrap_or("").to_owned()
    });
    lines.push(format!("FN:{}", escape_value(&fn_)));
    if first.is_some() || last.is_some() {
        lines.push(format!("N:{};{};;;", escape_value(last.unwrap_or("")), escape_value(first.unwrap_or(""))));
    }
    for e in emails {
        let kind = escape_param(&s(e, "type").unwrap_or("other").to_uppercase());
        let pref = if truthy(e.get("primary")) { ",PREF" } else { "" };
        lines.push(format!("EMAIL;TYPE={kind}{pref}:{}", escape_value(s(e, "value").unwrap_or(""))));
    }
    for p in arr(contact, "phones") {
        let kind = escape_param(&s(p, "type").unwrap_or("voice").to_uppercase());
        lines.push(format!("TEL;TYPE={kind}:{}", escape_value(s(p, "value").unwrap_or(""))));
    }
    if let Some(o) = s(contact, "organization") {
        lines.push(format!("ORG:{}", escape_value(o)));
    }
    if let Some(n) = s(contact, "notes") {
        lines.push(format!("NOTE:{}", escape_value(n)));
    }
    match contact.get("contactDates").and_then(Value::as_array) {
        Some(dates) => {
            let mut seen: Vec<(String, String)> = Vec::new();
            for d in dates {
                let Some(value) = d.get("value").and_then(Value::as_str).and_then(|v| normalize_vcard_date(v, true))
                else {
                    continue;
                };
                let Some(label) = d.get("label").and_then(Value::as_str).and_then(normalize_contact_date_label) else {
                    continue;
                };
                let key = (label.to_lowercase(), value.clone());
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                let lower = label.to_lowercase();
                let property = match lower.as_str() {
                    "birthday" => "BDAY",
                    "anniversary" => "ANNIVERSARY",
                    _ => "X-ABDATE",
                };
                let encoded =
                    if label.contains([',', ';', ':', '"']) { format!("\"{label}\"") } else { escape_param(&label) };
                lines.push(format!("{property};TYPE={encoded}:{value}"));
            }
        }
        None => {
            let full = |v: Option<&str>| {
                v.is_some_and(|d| {
                    d.len() == 10
                        && d.as_bytes()[4] == b'-'
                        && d.as_bytes()[7] == b'-'
                        && d.bytes().enumerate().all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
                })
            };
            if full(s(contact, "birthday")) {
                lines.push(format!("BDAY:{}", s(contact, "birthday").unwrap_or("")));
            }
            if full(s(contact, "anniversary")) {
                lines.push(format!("ANNIVERSARY:{}", s(contact, "anniversary").unwrap_or("")));
            }
        }
    }
    for (key, prop) in [("title", "TITLE"), ("role", "ROLE"), ("nickname", "NICKNAME")] {
        if let Some(v) = s(contact, key) {
            lines.push(format!("{prop}:{}", escape_value(v)));
        }
    }
    for (key, prop) in [("urls", "URL"), ("instantMessages", "IMPP")] {
        for u in arr(contact, key) {
            if let Some(v) = s(u, "value") {
                lines.push(format!(
                    "{prop};TYPE={}:{}",
                    escape_param(&s(u, "type").unwrap_or("other").to_uppercase()),
                    escape_value(v)
                ));
            }
        }
    }
    let cats = arr(contact, "categories");
    if !cats.is_empty() {
        lines.push(format!(
            "CATEGORIES:{}",
            cats.iter().map(|c| escape_value(&inboxora_mail::text::js_string(c))).collect::<Vec<_>>().join(",")
        ));
    }
    for a in arr(contact, "addresses") {
        let kind = escape_param(&s(a, "type").unwrap_or("other").to_uppercase());
        let parts: Vec<String> = ["pobox", "extended", "street", "locality", "region", "postalCode", "country"]
            .iter()
            .map(|k| escape_value(s(a, k).unwrap_or("")))
            .collect();
        lines.push(format!("ADR;TYPE={kind}:{}", parts.join(";")));
    }
    lines.push("END:VCARD".into());
    lines.iter().map(|l| fold_line(l)).collect()
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn property_name(line: &str) -> String {
    let Some(colon) = find_property_separator(line) else { return String::new() };
    let name = line[..colon].split(';').next().unwrap_or("").to_uppercase();
    match name.rfind('.') {
        Some(d) => name[d + 1..].to_owned(),
        None => name,
    }
}

/// Merge managed fields into an existing vCard, preserving everything else (`mergeVCard`).
pub fn merge(raw: &str, contact: &Value) -> String {
    let original = unfold(raw);
    let has_line = |want: &str| split_lines(&original).any(|l| l.trim_end().eq_ignore_ascii_case(want));
    if !has_line("BEGIN:VCARD") || !has_line("END:VCARD") {
        return generate(contact);
    }
    let mut managed: Vec<&str> =
        vec!["FN", "N", "EMAIL", "TEL", "ORG", "NOTE", "BDAY", "ANNIVERSARY", "X-ABDATE", "X-ANDROID-CUSTOM"];
    for (field, prop) in [
        ("contactDates", "X-ABDATE"),
        ("title", "TITLE"),
        ("role", "ROLE"),
        ("nickname", "NICKNAME"),
        ("urls", "URL"),
        ("instantMessages", "IMPP"),
        ("categories", "CATEGORIES"),
        ("addresses", "ADR"),
    ] {
        if contact.as_object().is_some_and(|m| m.contains_key(field)) {
            managed.push(prop);
        }
    }
    let uid = s(contact, "uid").map(str::to_owned).or_else(|| parse(&original).uid);
    let mut with_uid = contact.clone();
    if let Some(obj) = with_uid.as_object_mut() {
        obj.insert("uid".into(), uid.map_or(Value::Null, Value::String));
    }
    let generated = unfold(&generate(&with_uid));
    let replacement: Vec<&str> = split_lines(&generated)
        .filter(|l| !l.is_empty() && !matches!(property_name(l).as_str(), "BEGIN" | "VERSION" | "UID" | "END"))
        .collect();
    let preserved: Vec<&str> = split_lines(&original)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            let n = property_name(l);
            !n.is_empty() && n != "BEGIN" && n != "END" && !managed.contains(&n.as_str())
        })
        .collect();
    std::iter::once("BEGIN:VCARD")
        .chain(preserved)
        .chain(replacement)
        .chain(std::iter::once("END:VCARD"))
        .map(fold_line)
        .collect()
}
