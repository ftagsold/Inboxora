//! Header parsing helpers (port of the header half of `messageParser.ts`).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// Lenient base64 (Node `Buffer.from(s, 'base64')`): ignores invalid characters and
/// missing padding, accepts the URL-safe alphabet too.
pub fn base64_lenient(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Decode bytes with a charset label (WHATWG `TextDecoder` labels); unknown labels fall back
/// to UTF-8. A BOM for the selected encoding is removed, as `TextDecoder` does.
pub fn decode_charset(bytes: &[u8], charset: &str) -> String {
    let label = charset.trim().trim_matches(|c| c == '"' || c == '\'');
    let label = label.split('*').next().unwrap_or(label);
    let enc = match label.to_ascii_lowercase().as_str() {
        "" | "us-ascii" | "ascii" => encoding_rs::UTF_8,
        l => encoding_rs::Encoding::for_label(l.as_bytes()).unwrap_or(encoding_rs::UTF_8),
    };
    enc.decode_with_bom_removal(bytes).0.into_owned()
}

static ENCODED_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"=\?([^?]+)\?([BQbq])\?([^?]*)\?=").expect("regex"));
static ADJACENT_WORDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(=\?[^?]+\?[BQbq]\?[^?]*\?=)\s+(=\?[^?]+\?[BQbq]\?[^?]*\?=)").expect("regex"));

fn word_bytes(enc: &str, text: &str) -> Vec<u8> {
    if enc.eq_ignore_ascii_case("q") {
        let b = text.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'_' => out.push(b' '),
                b'=' if i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() => {
                    let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00");
                    out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                    i += 3;
                    continue;
                }
                c => out.push(c),
            }
            i += 1;
        }
        out
    } else {
        base64_lenient(text)
    }
}

/// Decode RFC 2047 encoded words. Words separated only by whitespace are joined
/// (RFC 2047 §6.2); consecutive words in the same charset are decoded as one byte sequence so
/// a multi-byte character split across words survives.
pub fn decode_mime_words(s: &str) -> String {
    if !s.contains("=?") {
        return s.to_owned();
    }
    let mut joined = s.to_owned();
    loop {
        let next = ADJACENT_WORDS.replace_all(&joined, "$1$2").into_owned();
        if next == joined {
            break;
        }
        joined = next;
    }
    let mut out = String::with_capacity(joined.len());
    let mut last = 0;
    let mut pending: Option<(String, Vec<u8>)> = None;
    let flush = |out: &mut String, pending: &mut Option<(String, Vec<u8>)>| {
        if let Some((cs, bytes)) = pending.take() {
            out.push_str(&decode_charset(&bytes, &cs));
        }
    };
    for caps in ENCODED_WORD.captures_iter(&joined) {
        let m = caps.get(0).expect("match");
        if m.start() > last {
            flush(&mut out, &mut pending);
            out.push_str(&joined[last..m.start()]);
        }
        let charset = caps[1].to_ascii_lowercase();
        let bytes = word_bytes(&caps[2], &caps[3]);
        match &mut pending {
            Some((cs, buf)) if *cs == charset => buf.extend_from_slice(&bytes),
            _ => {
                flush(&mut out, &mut pending);
                pending = Some((charset, bytes));
            }
        }
        last = m.end();
    }
    flush(&mut out, &mut pending);
    out.push_str(&joined[last..]);
    out
}

/// Parse a raw header block: names lowercased, folded lines unfolded, repeated headers joined
/// with `\n`. Keys keep first-seen order.
pub fn parse_raw_headers(text: &str) -> Vec<(String, String)> {
    let unfolded = text.replace("\r\n ", " ").replace("\r\n\t", " ").replace("\n ", " ").replace("\n\t", " ");
    let mut out: Vec<(String, String)> = Vec::new();
    for line in unfolded.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(colon) = line.find(':') else { continue };
        if colon < 1 {
            continue;
        }
        let name = line[..colon].to_lowercase().trim().to_owned();
        let val = line[colon + 1..].trim().to_owned();
        if name.is_empty() {
            continue;
        }
        match out.iter_mut().find(|(k, _)| *k == name) {
            Some((_, v)) if !v.is_empty() => {
                v.push('\n');
                v.push_str(&val);
            }
            Some((_, v)) => *v = val,
            None => out.push((name, val)),
        }
    }
    out
}

/// Header map helper.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Headers(pub Vec<(String, String)>);

impl Headers {
    pub fn parse(text: &str) -> Self {
        Self(parse_raw_headers(text))
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
    }

    pub fn to_json(&self) -> Value {
        Value::Object(self.0.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect())
    }

    /// `headersToRawString`.
    pub fn to_raw_string(&self) -> String {
        self.0
            .iter()
            .flat_map(|(k, v)| v.split('\n').map(move |line| format!("{k}: {line}")))
            .collect::<Vec<_>>()
            .join("\r\n")
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Mailbox {
    pub name: String,
    pub email: String,
}

fn parse_single_mailbox(s: &str) -> Mailbox {
    static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+?)\s*<([^>]+)>\s*$").expect("regex"));
    static BARE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*<([^>]+)>\s*$").expect("regex"));
    let trimmed = decode_mime_words(s.trim());
    if let Some(m) = NAMED.captures(&trimmed) {
        let name = m[1].strip_prefix('"').unwrap_or(&m[1]);
        let name = name.strip_suffix('"').unwrap_or(name);
        return Mailbox { name: name.trim().to_owned(), email: m[2].trim().to_lowercase() };
    }
    if let Some(m) = BARE.captures(&trimmed) {
        return Mailbox { name: String::new(), email: m[1].trim().to_lowercase() };
    }
    if trimmed.contains('@') {
        return Mailbox { name: String::new(), email: trimmed.to_lowercase() };
    }
    Mailbox { name: trimmed, email: String::new() }
}

/// Parse a comma-separated address list (quotes respected); entries without an address are dropped.
pub fn parse_mailbox_list(value: &str) -> Vec<Mailbox> {
    let mut results = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for c in value.chars() {
        if c == '"' {
            in_quote = !in_quote;
        }
        if c == ',' && !in_quote {
            if !current.trim().is_empty() {
                results.push(parse_single_mailbox(&current));
            }
            current.clear();
            continue;
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        results.push(parse_single_mailbox(&current));
    }
    results.into_iter().filter(|r| !r.email.is_empty()).collect()
}

const DELIVERY_HEADERS: [&str; 4] = ["delivered-to", "x-delivered-to", "x-original-to", "envelope-to"];
const MAX_DELIVERY_ADDRESSES: usize = 50;

/// Addresses an MTA recorded as the actual delivery target (BCC, catch-all, alias).
pub fn parse_delivery_addresses(h: &Headers) -> Vec<String> {
    let mut emails: Vec<String> = Vec::new();
    for header in DELIVERY_HEADERS {
        let Some(value) = h.get(header) else { continue };
        for line in value.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            for mb in parse_mailbox_list(line) {
                let normalized = mb.email.trim().to_lowercase();
                if !normalized.is_empty() && !emails.contains(&normalized) {
                    emails.push(normalized);
                }
                if emails.len() >= MAX_DELIVERY_ADDRESSES {
                    return emails;
                }
            }
        }
    }
    emails
}

pub fn detect_bulk(h: &Headers) -> bool {
    if h.get("list-unsubscribe").is_some() || h.get("list-id").is_some() || h.get("list-post").is_some() {
        return true;
    }
    matches!(h.get("precedence").map(str::to_lowercase).as_deref(), Some("bulk" | "list"))
}

/// `'newsletter' | 'promotion' | 'automated'` from header signals, or `None`.
pub fn detect_category(h: &Headers) -> Option<&'static str> {
    static NOREPLY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:^|[\s<,])(?:noreply|no[-.]reply|donotreply|do[-.]not[-.]reply)@").expect("regex")
    });
    let any = |names: &[&str]| names.iter().any(|n| h.get(n).is_some());
    if any(&[
        "x-github-reason",
        "x-github-sender",
        "x-github-delivery",
        "x-gitlab-project-id",
        "x-gitlab-pipeline-id",
        "x-gitlab-noteable-type",
        "x-linear-team-id",
        "x-linear-issue-id",
        "x-jira-fingerprint",
        "x-atlassian-token",
        "x-phabricator-sent-this-message",
        "x-bugzilla-component",
        "x-sentry-reply-to",
    ]) {
        return Some("automated");
    }
    let ct = h.get("content-type").unwrap_or("").to_lowercase();
    if ct.contains("text/calendar") || ct.contains("application/ics") {
        return Some("automated");
    }
    if NOREPLY.is_match(h.get("from").unwrap_or("")) {
        return Some("automated");
    }
    if any(&["list-id", "list-unsubscribe", "list-post"]) {
        return Some("newsletter");
    }
    if matches!(h.get("precedence").map(str::to_lowercase).as_deref(), Some("bulk" | "list")) {
        return Some("newsletter");
    }
    if any(&[
        "x-campaign-id",
        "x-mailchimp-campaign-id",
        "x-marketo-track",
        "x-salesforce-emailid",
        "x-klaviyo-campaign-id",
        "x-hubspot-email-id",
    ]) {
        return Some("promotion");
    }
    let mailer = h.get("x-mailer").unwrap_or("").to_lowercase();
    if ["mailchimp", "constant contact", "klaviyo", "hubspot", "marketo", "sendgrid"].iter().any(|m| mailer.contains(m))
    {
        return Some("promotion");
    }
    let auto = h.get("auto-submitted").unwrap_or("").trim().to_lowercase();
    if !auto.is_empty() && auto != "no" {
        return Some("automated");
    }
    None
}

/// Address entries stored as JSON (`[{name, email}]` or a JSON string of that).
fn address_entries(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::String(s) if !s.is_empty() => {
            serde_json::from_str::<Value>(s).ok().and_then(|p| p.as_array().cloned()).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn format_address_entry(e: &Value) -> String {
    if let Some(s) = e.as_str() {
        return decode_mime_words(s);
    }
    let email = e.get("email").and_then(Value::as_str).unwrap_or("");
    let name = e.get("name").and_then(Value::as_str).unwrap_or("");
    if !name.is_empty() && !email.is_empty() {
        return format!("\"{}\" <{email}>", decode_mime_words(name));
    }
    if !email.is_empty() { email.to_owned() } else { decode_mime_words(name) }
}

/// Best-effort RFC 822 header block from a stored message row (`buildHeadersFromMessage`).
pub fn build_headers_from_message(msg: &Value) -> String {
    let s = |k: &str| msg.get(k).and_then(Value::as_str).unwrap_or("");
    let mut lines: Vec<String> = Vec::new();
    let (from_email, from_name) = (s("from_email"), s("from_name"));
    if !from_email.is_empty() || !from_name.is_empty() {
        lines.push(format!(
            "From: {}",
            format_address_entry(&serde_json::json!({ "name": from_name, "email": from_email }))
        ));
    }
    for (key, label) in [("to_addresses", "To"), ("cc_addresses", "Cc"), ("reply_to", "Reply-To")] {
        let list = msg.get(key).map(address_entries).unwrap_or_default();
        if !list.is_empty() {
            lines.push(format!("{label}: {}", list.iter().map(format_address_entry).collect::<Vec<_>>().join(", ")));
        }
    }
    let subject = s("subject");
    if !subject.is_empty() && subject != "(no subject)" {
        let decoded = decode_mime_words(subject);
        if !decoded.is_empty() {
            lines.push(format!("Subject: {decoded}"));
        }
    }
    if !s("message_id").is_empty() {
        lines.push(format!("Message-ID: {}", s("message_id")));
    }
    if let Some(date) =
        msg.get("date").and_then(Value::as_str).and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
    {
        lines.push(format!("Date: {}", date.with_timezone(&chrono::Utc).format("%a, %d %b %Y %H:%M:%S GMT")));
    }
    if !s("in_reply_to").is_empty() {
        lines.push(format!("In-Reply-To: {}", s("in_reply_to")));
    }
    if !s("thread_references").is_empty() {
        lines.push(format!("References: {}", s("thread_references")));
    }
    lines.join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_encoded_words() {
        assert_eq!(decode_mime_words("=?utf-8?Q?Caf=C3=A9_ok?="), "Café ok");
        assert_eq!(decode_mime_words("=?iso-8859-1?Q?caf=E9?="), "café");
        assert_eq!(decode_mime_words("=?UTF-8?B?w6k=?= =?UTF-8?B?w6k=?="), "éé");
        // A UTF-8 character split across two base64 words.
        assert_eq!(decode_mime_words("=?utf-8?B?ww==?= =?utf-8?B?qQ==?="), "é");
        assert_eq!(decode_mime_words("plain"), "plain");
        assert_eq!(decode_mime_words("Re: =?utf-8?B?SGk=?= there"), "Re: Hi there");
    }

    #[test]
    fn parses_headers_and_mailboxes() {
        let h = Headers::parse("Subject: a\r\n b\r\nTo: \"Doe, J\" <J@X.test>, k@y.test\r\nX-A: 1\r\nx-a: 2\r\n");
        assert_eq!(h.get("subject"), Some("a b"));
        assert_eq!(h.get("x-a"), Some("1\n2"));
        let to = parse_mailbox_list(h.get("to").unwrap());
        assert_eq!(
            to,
            vec![
                Mailbox { name: "Doe, J".into(), email: "j@x.test".into() },
                Mailbox { name: String::new(), email: "k@y.test".into() }
            ]
        );
    }

    #[test]
    fn categorises_by_headers() {
        assert_eq!(
            detect_category(&Headers::parse("From: GitHub <noreply@github.com>\nX-GitHub-Reason: mention")),
            Some("automated")
        );
        assert_eq!(detect_category(&Headers::parse("List-Id: <x.y>")), Some("newsletter"));
        assert_eq!(detect_category(&Headers::parse("X-Mailer: MailChimp Mailer")), Some("promotion"));
        assert_eq!(detect_category(&Headers::parse("From: a@b.test")), None);
    }
}
