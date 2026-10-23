//! Fetch results in imapflow's shapes: envelope, body structure (with imapflow's part numbering
//! and RFC 2231/2047 parameter decoding), flags and body sections.

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset, Utc};
use imap_proto::{AttributeValue, BodyStructure as Bs, ContentEncoding, MessageSection, SectionPath};
use inboxora_mail::body::BodyStructure;
use inboxora_mail::headers::{decode_charset, decode_mime_words};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Address {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, Default)]
pub struct Envelope {
    /// Parsed `Date` header (imapflow's `new Date(value)`), or the raw text when unparsable.
    pub date: Option<DateTime<Utc>>,
    pub date_raw: Option<String>,
    pub subject: Option<String>,
    pub from: Vec<Address>,
    pub sender: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub in_reply_to: Option<String>,
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Message {
    pub seq: u32,
    pub uid: Option<u32>,
    pub flags: Option<Vec<String>>,
    pub modseq: Option<u64>,
    pub size: Option<u32>,
    pub envelope: Option<Envelope>,
    pub body_structure: Option<BodyStructure>,
    pub internal_date: Option<DateTime<Utc>>,
    /// `BODY[HEADER]` / `BODY[HEADER.FIELDS (…)]`.
    pub headers: Option<Vec<u8>>,
    /// `BODY[]` (or a partial of it).
    pub source: Option<Vec<u8>>,
    /// `BODY[<part>]` by part number.
    pub body_parts: HashMap<String, Vec<u8>>,
    pub email_id: Option<String>,
    pub thread_id: Option<String>,
    pub labels: Option<Vec<String>>,
}

impl Message {
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.as_ref().is_some_and(|f| f.iter().any(|x| x == flag))
    }

    pub(crate) fn absorb(&mut self, attrs: Vec<AttributeValue<'_>>) {
        for attr in attrs {
            match attr {
                AttributeValue::Uid(u) => self.uid = Some(u),
                AttributeValue::Flags(f) => self.flags = Some(f.into_iter().map(|s| s.into_owned()).collect()),
                AttributeValue::ModSeq(m) => self.modseq = Some(m),
                AttributeValue::Rfc822Size(s) => self.size = Some(s),
                AttributeValue::Envelope(e) => self.envelope = Some(envelope(&e)),
                AttributeValue::BodyStructure(b) => self.body_structure = Some(structure(&b, &mut Vec::new())),
                AttributeValue::InternalDate(d) => self.internal_date = parse_internal_date(&d),
                AttributeValue::Rfc822(d) => self.source = Some(d.map(|d| d.into_owned()).unwrap_or_default()),
                AttributeValue::Rfc822Header(d) => self.headers = Some(d.map(|d| d.into_owned()).unwrap_or_default()),
                AttributeValue::BodySection { section, data, .. } => {
                    let bytes = data.map(|d| d.into_owned()).unwrap_or_default();
                    match section {
                        None => self.source = Some(bytes),
                        Some(SectionPath::Full(MessageSection::Header)) => self.headers = Some(bytes),
                        Some(SectionPath::Full(_)) => {}
                        Some(SectionPath::Part(path, None)) => {
                            let key = path.iter().map(u32::to_string).collect::<Vec<_>>().join(".");
                            self.body_parts.insert(key, bytes);
                        }
                        Some(SectionPath::Part(path, Some(MessageSection::Header))) if path.is_empty() => {
                            self.headers = Some(bytes)
                        }
                        Some(SectionPath::Part(..)) => {}
                    }
                }
                AttributeValue::GmailMsgId(id) => self.email_id = Some(id.to_string()),
                AttributeValue::GmailThrId(id) => self.thread_id = Some(id.to_string()),
                AttributeValue::EmailId(id) => self.email_id = Some(id.into_owned()),
                AttributeValue::ThreadId(id) => self.thread_id = id.map(|i| i.into_owned()),
                AttributeValue::GmailLabels(l) => self.labels = Some(l.into_iter().map(|s| s.into_owned()).collect()),
                _ => {}
            }
        }
    }
}

/// `INTERNALDATE` (`17-Jul-1996 02:44:25 -0700`).
pub fn parse_internal_date(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    DateTime::parse_from_str(s, "%d-%b-%Y %H:%M:%S %z")
        .or_else(|_| DateTime::parse_from_str(&format!("0{s}"), "%d-%b-%Y %H:%M:%S %z"))
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| parse_mail_date(s))
}

/// JavaScript `new Date(text)` for mail dates: RFC 2822 with the lenient variants seen in the
/// wild (missing weekday, single-digit day, named zones, trailing comments, ISO 8601).
pub fn parse_mail_date(s: &str) -> Option<DateTime<Utc>> {
    let mut v = s.trim().to_owned();
    if let Some(i) = v.find('(') {
        v.truncate(i);
    }
    let v = v.trim();
    if let Ok(d) = DateTime::parse_from_rfc2822(v) {
        return Some(d.with_timezone(&Utc));
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(v) {
        return Some(d.with_timezone(&Utc));
    }
    // Drop a leading weekday, normalise named zones, then retry a few shapes.
    let body = v.split_once(',').map_or(v, |(_, rest)| rest).trim();
    let mut parts: Vec<String> = body.split_whitespace().map(str::to_owned).collect();
    if let Some(last) = parts.last_mut() {
        let zone = match last.to_ascii_uppercase().as_str() {
            "UT" | "UTC" | "GMT" | "Z" => Some("+0000"),
            "EST" => Some("-0500"),
            "EDT" => Some("-0400"),
            "CST" => Some("-0600"),
            "CDT" => Some("-0500"),
            "MST" => Some("-0700"),
            "MDT" => Some("-0600"),
            "PST" => Some("-0800"),
            "PDT" => Some("-0700"),
            _ => None,
        };
        if let Some(z) = zone {
            *last = z.to_owned();
        }
    }
    if parts.len() == 4 {
        parts.push("+0000".into());
    }
    let joined = parts.join(" ");
    for fmt in ["%d %b %Y %H:%M:%S %z", "%d %b %Y %H:%M %z", "%d %B %Y %H:%M:%S %z", "%b %d %Y %H:%M:%S %z"] {
        if let Ok(d) = DateTime::<FixedOffset>::parse_from_str(&joined, fmt) {
            return Some(d.with_timezone(&Utc));
        }
    }
    None
}

fn text(b: &Option<std::borrow::Cow<'_, [u8]>>) -> Option<String> {
    b.as_ref().map(|v| decode_charset(v, "utf-8"))
}

fn addresses(list: &Option<Vec<imap_proto::Address<'_>>>) -> Vec<Address> {
    let Some(list) = list else { return Vec::new() };
    list.iter()
        .filter_map(|a| {
            let name = text(&a.name).map(|n| decode_mime_words(&n)).unwrap_or_default();
            let mailbox = text(&a.mailbox).unwrap_or_default();
            let host = text(&a.host).unwrap_or_default();
            let addr = if host.is_empty() {
                // RFC 9051 7.5.2 group syntax: the mailbox is the group name.
                let name = if name.is_empty() && !mailbox.is_empty() { decode_mime_words(&mailbox) } else { name };
                Address { name, address: String::new() }
            } else {
                Address { name, address: format!("{mailbox}@{host}") }
            };
            (!addr.name.is_empty() || !addr.address.is_empty()).then_some(addr)
        })
        .collect()
}

fn envelope(e: &imap_proto::Envelope<'_>) -> Envelope {
    let date_raw = text(&e.date).filter(|d| !d.is_empty());
    Envelope {
        date: date_raw.as_deref().and_then(parse_mail_date),
        date_raw,
        subject: text(&e.subject).filter(|s| !s.is_empty()).map(|s| decode_mime_words(&s)),
        from: addresses(&e.from),
        sender: addresses(&e.sender),
        reply_to: addresses(&e.reply_to),
        to: addresses(&e.to),
        cc: addresses(&e.cc),
        bcc: addresses(&e.bcc),
        in_reply_to: text(&e.in_reply_to).filter(|s| !s.is_empty()),
        message_id: text(&e.message_id).filter(|s| !s.is_empty()),
    }
}

/// imapflow's `getStructuredParams`: RFC 2047 words, RFC 2231 continuations and charsets.
pub fn structured_params(pairs: &[(String, String)]) -> Map<String, Value> {
    let mut plain: Vec<(String, String)> = Vec::new();
    for (k, v) in pairs {
        let key = k.to_lowercase();
        if key == "__proto__" {
            continue;
        }
        let value = decode_mime_words(v);
        if let Some(slot) = plain.iter_mut().find(|(pk, _)| *pk == key) {
            slot.1 = value;
        } else {
            plain.push((key, value));
        }
    }
    // A plain `filename` carrying `charset'lang'value` is really `filename*`.
    if let Some((_, f)) = plain.iter().find(|(k, _)| k == "filename")
        && !plain.iter().any(|(k, _)| k == "filename*")
    {
        let mut it = f.splitn(3, '\'');
        if let (Some(cs), Some(_), Some(rest)) = (it.next(), it.next(), it.next())
            && !cs.is_empty()
            && cs.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && !rest.is_empty()
            && rest.bytes().all(|b| b > 0x1f && b < 0x80 && b != b'\'')
            && encoding_known(cs)
        {
            let v = format!("{cs}''{rest}");
            plain.push(("filename*".into(), v));
        }
    }
    struct Group {
        charset: Option<String>,
        values: Vec<(u32, String)>,
    }
    let mut out: Vec<(String, Value)> = Vec::new();
    let mut groups: Vec<(String, Group)> = Vec::new();
    for (key, value) in plain {
        let Some((base, nr, star)) = continuation(&key) else {
            out.push((key, Value::String(value)));
            continue;
        };
        if base == "__proto__" {
            continue;
        }
        let idx = match groups.iter().position(|(k, _)| *k == base) {
            Some(i) => i,
            None => {
                groups.push((base.clone(), Group { charset: None, values: Vec::new() }));
                groups.len() - 1
            }
        };
        let mut value = value;
        if nr == 0 && star {
            let mut it = value.splitn(3, '\'');
            if let (Some(cs), Some(_), Some(rest)) = (it.next(), it.next(), it.next()) {
                groups[idx].1.charset = Some(if cs.is_empty() { "utf-8".into() } else { cs.to_owned() });
                value = rest.to_owned();
            }
        }
        groups[idx].1.values.push((nr, value));
        out.retain(|(k, _)| *k != base);
    }
    for (base, mut g) in groups {
        g.values.sort_by_key(|(n, _)| *n);
        let joined: String = g.values.into_iter().map(|(_, v)| v).collect();
        let decoded = match g.charset {
            Some(cs) => decode_percent(&joined, &cs),
            None => decode_mime_words(&joined),
        };
        out.retain(|(k, _)| *k != base);
        out.push((base, Value::String(decoded)));
    }
    out.into_iter().collect()
}

/// Charsets imapflow's `resolveCharset` recognises (the common subset).
fn encoding_known(cs: &str) -> bool {
    let lower = cs.to_ascii_lowercase();
    matches!(lower.as_str(), "utf-8" | "utf8" | "us-ascii" | "ascii" | "iso-8859-1" | "latin1" | "windows-1252")
        || lower.starts_with("iso-8859-")
        || lower.starts_with("windows-")
        || lower.starts_with("koi8")
        || matches!(
            lower.as_str(),
            "shift_jis" | "euc-jp" | "iso-2022-jp" | "gb2312" | "gbk" | "gb18030" | "big5" | "euc-kr"
        )
}

/// `name*N`, `name*N*` or `name*` → (name, N, value carries charset).
fn continuation(key: &str) -> Option<(String, u32, bool)> {
    let star = key.ends_with('*');
    let trimmed = key.strip_suffix('*').unwrap_or(key);
    if let Some(i) = trimmed.rfind('*') {
        let digits = &trimmed[i + 1..];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return Some((trimmed[..i].to_lowercase(), digits.parse().unwrap_or(0), star));
        }
    }
    if star {
        return Some((trimmed.to_lowercase(), 0, true));
    }
    None
}

fn decode_percent(v: &str, charset: &str) -> String {
    let b = v.as_bytes();
    let mut bytes = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            bytes.push(hex(b[i + 1]) * 16 + hex(b[i + 2]));
            i += 3;
            continue;
        }
        bytes.push(b[i]);
        i += 1;
    }
    decode_charset(&bytes, charset)
}

fn params(p: &imap_proto::BodyParams<'_>) -> Vec<(String, String)> {
    p.as_ref().map(|v| v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()).unwrap_or_default()
}

fn encoding_name(e: &ContentEncoding<'_>) -> String {
    match e {
        ContentEncoding::SevenBit => "7bit".into(),
        ContentEncoding::EightBit => "8bit".into(),
        ContentEncoding::Binary => "binary".into(),
        ContentEncoding::Base64 => "base64".into(),
        ContentEncoding::QuotedPrintable => "quoted-printable".into(),
        ContentEncoding::Other(s) => s.to_lowercase(),
    }
}

/// Convert a parsed BODYSTRUCTURE with imapflow's part numbering: the root multipart has no
/// part number, children count from 1, and a message/rfc822 body reuses its wrapper's number.
fn structure(b: &Bs<'_>, path: &mut Vec<u32>) -> BodyStructure {
    let part = path.iter().map(u32::to_string).collect::<Vec<_>>().join(".");
    let (common, single) = match b {
        Bs::Basic { common, other, .. } | Bs::Text { common, other, .. } | Bs::Message { common, other, .. } => {
            (common, Some(other))
        }
        Bs::Multipart { common, .. } => (common, None),
    };
    let ctype = structured_params(&params(&common.ty.params));
    let disp = common.disposition.as_ref();
    let disp_params = disp.map(|d| structured_params(&params(&d.params))).unwrap_or_default();
    let get = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_str).map(str::to_owned);
    let mut node = BodyStructure {
        mime_type: format!("{}/{}", common.ty.ty.to_lowercase(), common.ty.subtype.to_lowercase()),
        part,
        charset: get(&ctype, "charset"),
        name: get(&ctype, "name"),
        disposition: disp.map(|d| d.ty.to_lowercase()).filter(|d| !d.is_empty()),
        filename: get(&disp_params, "filename"),
        disposition_size: disp_params.get("size").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()),
        parameters: ctype,
        ..Default::default()
    };
    if let Some(o) = single {
        node.id = o.id.as_ref().map(|s| s.to_string()).filter(|s| !s.is_empty());
        node.encoding = encoding_name(&o.transfer_encoding);
        node.size = Some(u64::from(o.octets));
    }
    match b {
        Bs::Multipart { bodies, .. } => {
            for (i, child) in bodies.iter().enumerate() {
                path.push(i as u32 + 1);
                node.children.push(structure(child, path));
                path.pop();
            }
        }
        Bs::Message { body, .. } => node.children.push(structure(body, path)),
        _ => {}
    }
    node
}

/// The body structure as imapflow's JSON (what the database and API surfaces carry).
pub fn structure_json(s: &BodyStructure) -> Value {
    let mut m = Map::new();
    if !s.part.is_empty() {
        m.insert("part".into(), json!(s.part));
    }
    if !s.children.is_empty() {
        m.insert("childNodes".into(), Value::Array(s.children.iter().map(structure_json).collect()));
    }
    m.insert("type".into(), json!(s.mime_type));
    if !s.parameters.is_empty() {
        m.insert("parameters".into(), Value::Object(s.parameters.clone()));
    }
    if let Some(id) = &s.id {
        m.insert("id".into(), json!(id));
    }
    if !s.encoding.is_empty() {
        m.insert("encoding".into(), json!(s.encoding));
    }
    if let Some(size) = s.size {
        m.insert("size".into(), json!(size));
    }
    if let Some(d) = &s.disposition {
        m.insert("disposition".into(), json!(d));
        let mut dp = Map::new();
        if let Some(f) = &s.filename {
            dp.insert("filename".into(), json!(f));
        }
        if let Some(sz) = s.disposition_size {
            dp.insert("size".into(), json!(sz.to_string()));
        }
        if !dp.is_empty() {
            m.insert("dispositionParameters".into(), Value::Object(dp));
        }
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
    }

    #[test]
    fn rfc2231_continuations_and_charsets() {
        let p = structured_params(&pairs(&[("FILENAME*0*", "utf-8''%C3%A4bc"), ("filename*1", "def.pdf")]));
        assert_eq!(p["filename"], "äbcdef.pdf");
        let p = structured_params(&pairs(&[("name", "=?UTF-8?Q?Gr=C3=BC=C3=9Fe.txt?=")]));
        assert_eq!(p["name"], "Grüße.txt");
        let p = structured_params(&pairs(&[("filename", "utf-8''%E2%82%AC.txt")]));
        assert_eq!(p["filename"], "€.txt");
    }

    #[test]
    fn mail_dates() {
        let d = parse_mail_date("Tue, 1 Jul 2003 10:52:37 +0200").unwrap();
        assert_eq!(d.to_rfc3339(), "2003-07-01T08:52:37+00:00");
        assert!(parse_mail_date("1 Jul 2003 10:52:37 GMT").is_some());
        assert!(parse_mail_date("Tue, 01 Jul 2003 10:52:37 +0200 (CEST)").is_some());
        assert_eq!(
            parse_internal_date("17-Jul-1996 02:44:25 -0700").unwrap().to_rfc3339(),
            "1996-07-17T09:44:25+00:00"
        );
        assert_eq!(
            parse_internal_date(" 7-Jul-1996 02:44:25 -0700").unwrap().to_rfc3339(),
            "1996-07-07T09:44:25+00:00"
        );
    }
}
