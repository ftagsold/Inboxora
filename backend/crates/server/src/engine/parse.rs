//! `parseMessage` / `enrichParsedMetadata`: a fetched message into the fields the ingest
//! paths store.

use chrono::{DateTime, Utc};
use inboxora_imap::{Address, Message};
use inboxora_mail::body::{decode_body_part, detect_attachments, find_snippet_part};
use inboxora_mail::headers::{Headers, decode_mime_words, detect_bulk, parse_delivery_addresses, parse_mailbox_list};
use inboxora_mail::snippet::{build_snippet_from_html, snippet_from_body};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Addr {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub uid: u32,
    pub email_id: Option<String>,
    pub thread_id: Option<String>,
    pub message_id: Option<String>,
    pub subject: String,
    pub from_name: String,
    pub from_email: String,
    pub sender_name: Option<String>,
    pub sender_email: Option<String>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub reply_to: Vec<Addr>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub headers: Headers,
    pub delivery_addresses: Vec<String>,
    pub date: Option<DateTime<Utc>>,
    pub snippet: String,
    pub is_read: bool,
    pub is_starred: bool,
    pub has_attachments: bool,
    pub flags: Vec<String>,
    pub is_bulk: bool,
}

fn addrs(list: &[Address]) -> Vec<Addr> {
    list.iter().map(|a| Addr { name: a.name.clone(), email: a.address.clone() }).collect()
}

/// `resolveSubject`: the decoded envelope subject, else the header, else `(no subject)`.
fn resolve_subject(envelope: Option<&str>, headers: &Headers) -> String {
    let from_env = envelope.map(|s| decode_mime_words(s).trim().to_owned()).unwrap_or_default();
    if !from_env.is_empty() {
        return from_env;
    }
    let from_header = headers.get("subject").map(|s| decode_mime_words(s).trim().to_owned()).unwrap_or_default();
    if !from_header.is_empty() { from_header } else { "(no subject)".into() }
}

pub fn parse_message(msg: &Message) -> Parsed {
    let env = msg.envelope.clone().unwrap_or_default();
    let flags = msg.flags.clone().unwrap_or_default();
    let from = env.from.first().cloned().unwrap_or_default();
    let from_email = from.address.clone();
    let from_name =
        if !from.name.is_empty() { from.name.clone() } else { from_email.split('@').next().unwrap_or("").to_owned() };
    let sender = env.sender.first().cloned().unwrap_or_default();
    let distinct_sender = !sender.address.is_empty() && sender.address.to_lowercase() != from_email.to_lowercase();

    let mut snippet = String::new();
    if !msg.body_parts.is_empty() {
        let info = msg.body_structure.as_ref().and_then(find_snippet_part);
        let (raw, encoding, charset, is_html) = match &info {
            Some(p) if msg.body_parts.contains_key(&p.part) => {
                (msg.body_parts.get(&p.part), p.encoding.clone(), p.charset.clone(), p.mime_type == "text/html")
            }
            _ => (msg.body_parts.values().next(), String::new(), "utf-8".to_owned(), false),
        };
        if let Some(raw) = raw {
            let text = decode_body_part(raw, &encoding, &charset);
            let fallback = info
                .as_ref()
                .and_then(|p| p.html_fallback.as_deref())
                .filter(|_| !is_html)
                .and_then(|f| msg.body_parts.get(&f.part).map(|b| decode_body_part(b, &f.encoding, &f.charset)));
            snippet =
                if is_html { build_snippet_from_html(&text) } else { snippet_from_body(&text, fallback.as_deref()) };
        }
    }
    let has_attachments = msg.body_structure.as_ref().is_some_and(detect_attachments);
    let headers = msg.headers.as_ref().map(|h| Headers::parse(&String::from_utf8_lossy(h))).unwrap_or_default();
    let references = headers.get("references").map(str::to_owned);
    Parsed {
        uid: msg.uid.unwrap_or(0),
        email_id: msg.email_id.clone(),
        thread_id: msg.thread_id.clone(),
        message_id: env.message_id.clone(),
        subject: resolve_subject(env.subject.as_deref(), &headers),
        from_name,
        from_email,
        sender_name: distinct_sender.then(|| sender.name.clone()),
        sender_email: distinct_sender.then(|| sender.address.clone()),
        to: addrs(&env.to),
        cc: addrs(&env.cc),
        reply_to: addrs(&env.reply_to),
        in_reply_to: env.in_reply_to.clone(),
        references,
        delivery_addresses: parse_delivery_addresses(&headers),
        is_bulk: detect_bulk(&headers),
        headers,
        date: msg.internal_date.or(env.date),
        snippet,
        is_read: flags.iter().any(|f| f == "\\Seen"),
        is_starred: flags.iter().any(|f| f == "\\Flagged"),
        has_attachments,
        flags,
    }
}

pub struct EnrichContext<'a> {
    pub account_email: Option<&'a str>,
    pub account_name: Option<&'a str>,
    pub sender_name: Option<&'a str>,
    pub folder_path: &'a str,
    pub sent_folder_path: Option<&'a str>,
}

/// `enrichParsedMetadata`: Sent-folder sender defaults and header fallbacks.
pub fn enrich(p: &mut Parsed, ctx: &EnrichContext<'_>) {
    static SENT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\bsent\b").expect("regex"));
    let is_sent = ctx.sent_folder_path.is_some_and(|s| s == ctx.folder_path) || SENT.is_match(ctx.folder_path);
    if is_sent
        && p.from_email.is_empty()
        && let Some(email) = ctx.account_email.filter(|e| !e.is_empty())
    {
        p.from_email = email.to_owned();
        if p.from_name.is_empty() {
            p.from_name = ctx
                .sender_name
                .filter(|s| !s.is_empty())
                .or(ctx.account_name.filter(|s| !s.is_empty()))
                .unwrap_or("")
                .to_owned();
        }
    }
    if (p.subject.is_empty() || p.subject == "(no subject)")
        && let Some(raw) = p.headers.get("subject")
    {
        let s = decode_mime_words(raw).trim().to_owned();
        if !s.is_empty() {
            p.subject = s;
        }
    }
    let to_addr = |v: Vec<inboxora_mail::headers::Mailbox>| {
        v.into_iter().map(|m| Addr { name: m.name, email: m.email }).collect()
    };
    if p.to.is_empty()
        && let Some(raw) = p.headers.get("to")
    {
        p.to = to_addr(parse_mailbox_list(raw));
    }
    if p.cc.is_empty()
        && let Some(raw) = p.headers.get("cc")
    {
        p.cc = to_addr(parse_mailbox_list(raw));
    }
}

/// Strip NULs PostgreSQL rejects (`sanitizeStr`).
pub fn clean(s: &str) -> String {
    if s.contains('\0') { s.replace('\0', "") } else { s.to_owned() }
}

pub fn clean_opt(s: Option<&str>) -> String {
    s.map(clean).unwrap_or_default()
}

/// `safeDate`: the parsed date, else now.
pub fn safe_date(d: Option<DateTime<Utc>>) -> DateTime<Utc> {
    d.unwrap_or_else(Utc::now)
}

impl Parsed {
    /// The parsed message as Node broadcasts it (`{...parsed, id, accountId, folder}`).
    pub fn to_json(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("uid".into(), json!(self.uid));
        m.insert("attributes".into(), json!({ "emailId": self.email_id, "threadId": self.thread_id }));
        m.insert("messageId".into(), json!(self.message_id));
        m.insert("subject".into(), json!(self.subject));
        m.insert("fromName".into(), json!(self.from_name));
        m.insert("fromEmail".into(), json!(self.from_email));
        m.insert("senderName".into(), json!(self.sender_name));
        m.insert("senderEmail".into(), json!(self.sender_email));
        m.insert("to".into(), json!(self.to));
        m.insert("cc".into(), json!(self.cc));
        m.insert("replyTo".into(), json!(self.reply_to));
        m.insert("inReplyTo".into(), json!(self.in_reply_to));
        m.insert("references".into(), json!(self.references));
        m.insert("parsedHeaders".into(), self.headers.to_json());
        m.insert("deliveryAddresses".into(), json!(self.delivery_addresses));
        m.insert("date".into(), json!(inboxora_core::json::js_iso(&safe_date(self.date))));
        m.insert("snippet".into(), json!(self.snippet));
        m.insert("isRead".into(), json!(self.is_read));
        m.insert("isStarred".into(), json!(self.is_starred));
        m.insert("hasAttachments".into(), json!(self.has_attachments));
        m.insert("flags".into(), json!(self.flags));
        m.insert("isBulk".into(), json!(self.is_bulk));
        m
    }
}
