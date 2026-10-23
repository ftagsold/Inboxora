//! Contact field validation and import/export (ports of `contactFields.ts` and
//! `contactTransfer.ts`).

use serde_json::{Map, Value, json};

use inboxora_mail::text::{js_len, js_trim};

const MAX_VALUE_LENGTH: usize = 2048;

/// `text(value)`: trimmed string within the length limit; `None` for non-strings or too long.
fn text(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?;
    let t = js_trim(s);
    (js_len(t) <= MAX_VALUE_LENGTH).then(|| t.to_owned())
}

fn or_default<'a>(v: Option<&'a Value>, d: &'a Value) -> Option<&'a Value> {
    match v {
        None | Some(Value::Null) => Some(d),
        other => other,
    }
}

fn text_or_empty(v: Option<&Value>) -> Option<String> {
    text(or_default(v, &Value::String(String::new())))
}

fn text_or_other(v: Option<&Value>) -> Option<String> {
    text(or_default(v, &Value::String("other".into())))
}

fn typed_values(values: &Value, valid: impl Fn(&str) -> bool) -> Option<Vec<Value>> {
    let arr = values.as_array()?;
    let mut out = Vec::new();
    for v in arr {
        let obj = v.as_object()?;
        let value = text(obj.get("value"))?;
        let kind = text_or_other(obj.get("type"))?;
        if !value.is_empty() && !valid(&value) {
            return None;
        }
        if !value.is_empty() {
            out.push(json!({ "value": value, "type": if kind.is_empty() { "other".to_owned() } else { kind } }));
        }
    }
    Some(out)
}

const ADDRESS_FIELDS: [&str; 7] = ["pobox", "extended", "street", "locality", "region", "postalCode", "country"];

fn addresses(values: &Value) -> Option<Vec<Value>> {
    let arr = values.as_array()?;
    let mut out = Vec::new();
    for v in arr {
        let obj = v.as_object()?;
        let kind = text_or_other(obj.get("type"))?;
        let mut a = Map::new();
        a.insert("type".into(), Value::String(if kind.is_empty() { "other".into() } else { kind }));
        let mut any = false;
        for f in ADDRESS_FIELDS {
            let val = text_or_empty(obj.get(f))?;
            any |= !val.is_empty();
            a.insert(f.into(), Value::String(val));
        }
        if any {
            out.push(Value::Object(a));
        }
    }
    Some(out)
}

fn array_or_empty(v: Option<&Value>) -> Value {
    match v {
        None | Some(Value::Null) => Value::Array(Vec::new()),
        Some(v) => v.clone(),
    }
}

fn is_http(v: &str) -> bool {
    v.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        || v.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))
}

/// `normalizeRichContactFields(body)`: `None` when malformed. Keys: title, role, nickname,
/// urls, instantMessages, addresses, categories.
pub fn normalize_rich_fields(body: &Value) -> Option<Map<String, Value>> {
    let obj = body.as_object()?;
    let mut out = Map::new();
    for f in ["title", "role", "nickname"] {
        let v = text_or_empty(obj.get(f))?;
        out.insert(f.into(), if v.is_empty() { Value::Null } else { Value::String(v) });
    }
    let urls = typed_values(&array_or_empty(obj.get("urls")), is_http)?;
    let ims = typed_values(&array_or_empty(obj.get("instantMessages")), |_| true)?;
    let addrs = addresses(&array_or_empty(obj.get("addresses")))?;
    let cats_in = obj.get("categories")?.as_array()?;
    let mut cats = Vec::new();
    for c in cats_in {
        let v = text(Some(c))?;
        if !v.is_empty() {
            cats.push(Value::String(v));
        }
    }
    out.insert("urls".into(), Value::Array(urls));
    out.insert("instantMessages".into(), Value::Array(ims));
    out.insert("addresses".into(), Value::Array(addrs));
    out.insert("categories".into(), Value::Array(cats));
    Some(out)
}

// ── export ───────────────────────────────────────────────────────────────────────────

fn js_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(v) => inboxora_mail::text::js_string(v),
    }
}

fn csv_escape(value: &str) -> String {
    let mut t = value.to_owned();
    if t.starts_with(['=', '+', '-', '@']) {
        t = format!("'{t}");
    }
    if t.contains(['"', ',', '\r', '\n']) { format!("\"{}\"", t.replace('"', "\"\"")) } else { t }
}

fn csv_row(cols: &[String]) -> String {
    cols.iter().map(|c| csv_escape(c)).collect::<Vec<_>>().join(",")
}

fn entries(c: &Value, key: &str) -> Vec<Value> {
    c.get(key).and_then(Value::as_array).cloned().unwrap_or_default()
}

fn first_value(list: &[Value]) -> String {
    list.iter()
        .filter_map(|e| e.get("value").and_then(Value::as_str))
        .map(js_trim)
        .find(|v| !v.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn non_empty(c: &Value, key: &str) -> Option<String> {
    c.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn contact_name(c: &Value) -> String {
    if let Some(d) = non_empty(c, "display_name") {
        return d;
    }
    let joined =
        [non_empty(c, "first_name"), non_empty(c, "last_name")].into_iter().flatten().collect::<Vec<_>>().join(" ");
    if !joined.is_empty() {
        return joined;
    }
    first_value(&entries(c, "emails"))
}

/// `normalizeType`.
pub fn normalize_type(v: Option<&Value>, fallback: &str) -> String {
    let raw = match v {
        Some(v) if inboxora_mail::text_truthy(v) => inboxora_mail::text::js_string(v),
        _ => fallback.to_owned(),
    };
    let t = js_trim(&raw).to_lowercase();
    match t.as_str() {
        "cell" | "iphone" | "mobile" | "komórka" | "komorkowy" => "mobile".into(),
        "work" | "służbowy" | "sluzbowy" => "work".into(),
        "home" | "dom" => "home".into(),
        "" => fallback.to_owned(),
        other => other.to_owned(),
    }
}

fn csv_type(v: Option<&Value>) -> String {
    let t = normalize_type(v, "other");
    let mut chars = t.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn vcard_escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n").replace(';', "\\;").replace(',', "\\,")
}

pub fn contacts_to_google_csv(contacts: &[Value]) -> String {
    let header: Vec<String> = [
        "Name",
        "Given Name",
        "Family Name",
        "Organization 1 - Name",
        "Organization 1 - Title",
        "E-mail 1 - Type",
        "E-mail 1 - Value",
        "Phone 1 - Type",
        "Phone 1 - Value",
        "Notes",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let mut rows = vec![csv_row(&header)];
    for c in contacts {
        let email = entries(c, "emails").into_iter().next().unwrap_or(json!({}));
        let phone = entries(c, "phones").into_iter().next().unwrap_or(json!({}));
        let s = |k: &str| non_empty(c, k).unwrap_or_default();
        rows.push(csv_row(&[
            contact_name(c),
            s("first_name"),
            s("last_name"),
            s("organization"),
            s("title"),
            csv_type(email.get("type")),
            non_empty(&email, "value").unwrap_or_default(),
            csv_type(phone.get("type")),
            non_empty(&phone, "value").unwrap_or_default(),
            s("notes"),
        ]));
    }
    rows.join("\r\n")
}

pub fn contacts_to_outlook_csv(contacts: &[Value]) -> String {
    let header: Vec<String> = [
        "First Name",
        "Middle Name",
        "Last Name",
        "Title",
        "Company",
        "E-mail Address",
        "Business Phone",
        "Mobile Phone",
        "Notes",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let mut rows = vec![csv_row(&header)];
    for c in contacts {
        let phones = entries(c, "phones");
        let find = |t: &str| {
            phones
                .iter()
                .find(|p| normalize_type(p.get("type"), "other") == t)
                .and_then(|p| non_empty(p, "value"))
                .unwrap_or_default()
        };
        let s = |k: &str| non_empty(c, k).unwrap_or_default();
        rows.push(csv_row(&[
            s("first_name"),
            String::new(),
            s("last_name"),
            s("title"),
            s("organization"),
            first_value(&entries(c, "emails")),
            find("work"),
            find("mobile"),
            s("notes"),
        ]));
    }
    rows.join("\r\n")
}

pub fn contacts_to_vcard(contacts: &[Value]) -> String {
    contacts
        .iter()
        .map(|c| {
            let mut lines = vec![
                "BEGIN:VCARD".to_owned(),
                "VERSION:3.0".to_owned(),
                format!("UID:{}", vcard_escape(&js_str(c.get("uid")))),
                format!("FN:{}", vcard_escape(&contact_name(c))),
                format!(
                    "N:{};{};;;",
                    vcard_escape(&js_str(c.get("last_name"))),
                    vcard_escape(&js_str(c.get("first_name")))
                ),
            ];
            for e in entries(c, "emails") {
                if let Some(v) = non_empty(&e, "value") {
                    lines.push(format!(
                        "EMAIL;TYPE={}:{}",
                        normalize_type(e.get("type"), "other").to_uppercase(),
                        vcard_escape(&v)
                    ));
                }
            }
            for p in entries(c, "phones") {
                if let Some(v) = non_empty(&p, "value") {
                    lines.push(format!(
                        "TEL;TYPE={}:{}",
                        normalize_type(p.get("type"), "other").to_uppercase(),
                        vcard_escape(&v)
                    ));
                }
            }
            for (k, prop) in [("organization", "ORG"), ("title", "TITLE"), ("notes", "NOTE")] {
                if let Some(v) = non_empty(c, k) {
                    lines.push(format!("{prop}:{}", vcard_escape(&v)));
                }
            }
            lines.push("END:VCARD".into());
            lines.push(String::new());
            lines.join("\r\n")
        })
        .collect()
}

// ── Google CSV import ────────────────────────────────────────────────────────────────

fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let chars: Vec<char> = text.chars().collect();
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut value = String::new();
    let mut quoted = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if quoted && c == '"' && chars.get(i + 1) == Some(&'"') {
            value.push('"');
            i += 1;
        } else if c == '"' {
            quoted = !quoted;
        } else if !quoted && c == ',' {
            row.push(std::mem::take(&mut value));
        } else if !quoted && (c == '\n' || c == '\r') {
            if c == '\r' && chars.get(i + 1) == Some(&'\n') {
                i += 1;
            }
            row.push(std::mem::take(&mut value));
            rows.push(std::mem::take(&mut row));
        } else {
            value.push(c);
        }
        i += 1;
    }
    if !value.is_empty() || !row.is_empty() {
        row.push(value);
        rows.push(row);
    }
    rows
}

fn valid_ymd(v: &str) -> Option<String> {
    let b = v.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || !v.bytes().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return None;
    }
    let (y, m, d) = (v[..4].parse::<i32>().ok()?, v[5..7].parse::<u32>().ok()?, v[8..].parse::<u32>().ok()?);
    if (0..=99).contains(&y) {
        return None;
    }
    chrono::NaiveDate::from_ymd_opt(y, m, d).map(|_| v.to_owned())
}

/// `parseGoogleCsv`: contacts as JSON objects with the Node field names.
pub fn parse_google_csv(text: &str) -> Vec<Value> {
    let mut rows = parse_csv(text).into_iter();
    let header = rows.next().unwrap_or_default();
    static DASH: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\s+[–—]\s+").expect("regex"));
    static LABEL_SEP: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\s+:::\s+").expect("regex"));
    let norm_dash = |s: &str| DASH.replace_all(js_trim(s), " - ").into_owned();
    let columns: Vec<(String, usize)> = header.iter().enumerate().map(|(i, n)| (norm_dash(n), i)).collect();
    let col = |name: &str| columns.iter().rev().find(|(n, _)| n == name).map(|(_, i)| *i);
    let get = |row: &[String], names: &[String]| -> String {
        for n in names {
            if let Some(v) = col(n).and_then(|i| row.get(i)).map(|v| js_trim(v)).filter(|v| !v.is_empty()) {
                return v.to_owned();
            }
        }
        String::new()
    };
    let indexed = |field: &str| -> Vec<u64> {
        let prefix = format!("{field} ");
        let mut nums: Vec<u64> = Vec::new();
        for (name, _) in &columns {
            if let Some(rest) = name.strip_prefix(&prefix) {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if !digits.is_empty() && rest[digits.len()..].starts_with(" - ") {
                    // `.filter(Boolean)` keeps "0" (a non-empty string).
                    let n: u64 = digits.parse().unwrap_or(0);
                    if !nums.contains(&n) {
                        nums.push(n);
                    }
                }
            }
        }
        nums.sort_unstable();
        nums
    };
    let s = |v: String| vec![v];
    let mut out = Vec::new();
    for row in rows {
        let entries_of = |field: &str| -> Vec<Value> {
            let mut vals = Vec::new();
            for n in indexed(field) {
                let value = get(&row, &s(format!("{field} {n} - Value")));
                if !value.is_empty() {
                    let t = get(&row, &[format!("{field} {n} - Type"), format!("{field} {n} - Label")]);
                    vals.push(json!({ "value": value, "type": normalize_type(Some(&Value::String(t)), "other") }));
                }
            }
            vals
        };
        let emails: Vec<Value> = entries_of("E-mail")
            .into_iter()
            .enumerate()
            .map(|(i, mut e)| {
                let v = e["value"].as_str().unwrap_or("").to_lowercase();
                e["value"] = Value::String(v);
                e["primary"] = Value::Bool(i == 0);
                e
            })
            .collect();
        let phones = entries_of("Phone");
        let first = get(&row, &["Given Name".into(), "First Name".into()]);
        let last = get(&row, &["Family Name".into(), "Last Name".into()]);
        let middle = get(&row, &["Additional Name".into(), "Middle Name".into()]);
        let mut display = get(&row, &s("Name".into()));
        if display.is_empty() {
            display = [
                get(&row, &s("Name Prefix".into())),
                first.clone(),
                middle,
                last.clone(),
                get(&row, &s("Name Suffix".into())),
            ]
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        }
        let addresses: Vec<Value> = indexed("Address")
            .into_iter()
            .map(|n| {
                let g = |suffix: &str| get(&row, &s(format!("Address {n} - {suffix}")));
                let mut a = Map::new();
                a.insert(
                    "type".into(),
                    Value::String(normalize_type(
                        Some(&Value::String(get(
                            &row,
                            &[format!("Address {n} - Type"), format!("Address {n} - Label")],
                        ))),
                        "other",
                    )),
                );
                a.insert("pobox".into(), Value::String(g("PO Box")));
                a.insert("extended".into(), Value::String(g("Extended Address")));
                a.insert(
                    "street".into(),
                    Value::String(get(&row, &[format!("Address {n} - Street"), format!("Address {n} - Formatted")])),
                );
                a.insert("locality".into(), Value::String(g("City")));
                a.insert("region".into(), Value::String(g("Region")));
                a.insert("postalCode".into(), Value::String(g("Postal Code")));
                a.insert("country".into(), Value::String(g("Country")));
                Value::Object(a)
            })
            .filter(|a| {
                a.as_object()
                    .is_some_and(|m| m.iter().any(|(k, v)| k != "type" && v.as_str().is_some_and(|x| !x.is_empty())))
            })
            .collect();
        let urls: Vec<Value> = indexed("Website")
            .into_iter()
            .map(|n| {
                json!({ "value": get(&row, &s(format!("Website {n} - Value"))), "type": normalize_type(Some(&Value::String(get(&row, &[format!("Website {n} - Type"), format!("Website {n} - Label")]))), "other") })
            })
            .filter(|u| is_http(u["value"].as_str().unwrap_or("")))
            .collect();
        let ims: Vec<Value> = indexed("IM")
            .into_iter()
            .map(|n| {
                json!({ "value": get(&row, &s(format!("IM {n} - Value"))), "type": normalize_type(Some(&Value::String(get(&row, &[format!("IM {n} - Service"), format!("IM {n} - Type"), format!("IM {n} - Label")]))), "other") })
            })
            .filter(|u| u["value"].as_str().is_some_and(|v| !v.is_empty()))
            .collect();
        let birthday = valid_ymd(&get(&row, &s("Birthday".into())));
        let mut dates: Vec<Value> = indexed("Event")
            .into_iter()
            .filter_map(|n| {
                let value = valid_ymd(&get(&row, &s(format!("Event {n} - Value"))))?;
                let raw_label = get(&row, &[format!("Event {n} - Label"), format!("Event {n} - Type")]);
                let label = crate::vcard::normalize_contact_date_label(if raw_label.is_empty() {
                    "Other"
                } else {
                    &raw_label
                })?;
                Some(json!({ "label": label, "value": value }))
            })
            .collect();
        if let Some(b) = &birthday
            && !dates.iter().any(|d| {
                d["label"].as_str().is_some_and(|l| l.to_lowercase() == "birthday") && d["value"].as_str() == Some(b)
            })
        {
            dates.insert(0, json!({ "label": "Birthday", "value": b }));
        }
        let mut source = Map::new();
        for (i, name) in header.iter().enumerate() {
            let v = row.get(i).map(|v| js_trim(v).to_owned()).unwrap_or_default();
            if !v.is_empty() {
                source.insert(js_trim(name).to_owned(), Value::String(v));
            }
        }
        if display.is_empty() && emails.is_empty() && phones.is_empty() && addresses.is_empty() && source.is_empty() {
            continue;
        }
        let opt = |v: String| if v.is_empty() { Value::Null } else { Value::String(v) };
        let label_text = get(&row, &["Labels".into(), "Group Membership".into()]);
        let labels: Vec<Value> = LABEL_SEP
            .split(&label_text)
            .map(|l| js_trim(l).to_owned())
            .filter(|l| !l.is_empty())
            .map(Value::String)
            .collect();
        let anniversary = dates
            .iter()
            .find(|d| d["label"].as_str().is_some_and(|l| l.to_lowercase() == "anniversary"))
            .and_then(|d| d["value"].as_str().map(str::to_owned));
        let mut m = Map::new();
        m.insert("displayName".into(), Value::String(display));
        m.insert("firstName".into(), opt(first));
        m.insert("lastName".into(), opt(last));
        m.insert("emails".into(), Value::Array(emails));
        m.insert("phones".into(), Value::Array(phones));
        m.insert("organization".into(), opt(get(&row, &["Organization 1 - Name".into(), "Organization Name".into()])));
        m.insert("title".into(), opt(get(&row, &["Organization 1 - Title".into(), "Organization Title".into()])));
        m.insert("role".into(), opt(get(&row, &s("Organization Department".into()))));
        m.insert("nickname".into(), opt(get(&row, &s("Nickname".into()))));
        m.insert("birthday".into(), birthday.map_or(Value::Null, Value::String));
        m.insert("anniversary".into(), anniversary.map_or(Value::Null, Value::String));
        m.insert("contactDates".into(), Value::Array(dates));
        m.insert("urls".into(), Value::Array(urls));
        m.insert("instantMessages".into(), Value::Array(ims));
        m.insert("addresses".into(), Value::Array(addresses));
        m.insert("categories".into(), Value::Array(labels));
        m.insert("notes".into(), opt(get(&row, &s("Notes".into()))));
        m.insert("sourceFields".into(), Value::Object(source));
        out.push(Value::Object(m));
    }
    out
}
