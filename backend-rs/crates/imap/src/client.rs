//! An async IMAP4rev1 client with imapflow's session behaviour: STARTTLS when offered, PLAIN /
//! LOGIN / XOAUTH2 / OAUTHBEARER, NAMESPACE, COMPRESS=DEFLATE, `ENABLE CONDSTORE UTF8=ACCEPT`,
//! namespace-prefixed paths, UIDPLUS results, MOVE fallback and IDLE.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use imap_proto::rfc4315::UidSetMember;
use imap_proto::{MailboxDatum, NameAttribute, Response, ResponseCode, Status, StatusAttribute};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::transport::{self, BoxIo, Deflate};
use crate::types::Message;
use crate::{special_use, utf7};

#[derive(Debug, thiserror::Error, Clone)]
pub enum Error {
    /// A socket failure.
    #[error("{0}")]
    Io(String),
    /// No data within the command timeout.
    #[error("Socket timeout")]
    Timeout,
    /// The connection attempt did not finish in time.
    #[error("{0} timeout ({1}ms)")]
    ConnectTimeout(String, u128),
    /// A tagged NO/BAD: `text` is the server's human-readable text.
    #[error("{text}")]
    Command { status: CmdStatus, code: Option<String>, text: String },
    #[error("Authentication failed")]
    Auth { text: String, code: Option<String> },
    #[error("Connection closed")]
    Closed,
    #[error("{0}")]
    Protocol(String),
}

impl Error {
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Command { code, .. } | Error::Auth { code, .. } => code.as_deref(),
            _ => None,
        }
    }
    /// Server text for command failures (imapflow's `responseText`).
    pub fn response_text(&self) -> Option<&str> {
        match self {
            Error::Command { text, .. } | Error::Auth { text, .. } => Some(text),
            _ => None,
        }
    }
    /// The socket is unusable after this error.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Error::Io(_) | Error::Timeout | Error::Closed | Error::Protocol(_) | Error::ConnectTimeout(..))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::TimedOut => Error::Timeout,
            std::io::ErrorKind::UnexpectedEof => Error::Closed,
            _ => Error::Io(e.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdStatus {
    No,
    Bad,
}

#[derive(Debug, Clone)]
pub enum Auth {
    Password { user: String, pass: Option<String> },
    OAuth { user: String, token: String },
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Pre-validated addresses (DNS-rebinding safe); tried in RFC 8305 order.
    pub addresses: Vec<IpAddr>,
    /// Host name for TLS (SNI and verification).
    pub servername: String,
    pub port: u16,
    pub secure: bool,
    pub verify_tls: bool,
    pub auth: Auth,
    pub command_timeout: Duration,
    pub compress: bool,
}

/// Unsolicited mailbox changes, as imapflow's `exists`/`expunge`/`flags`/`close` events.
#[derive(Debug, Clone)]
pub enum Event {
    Exists { path: String, count: u32, prev_count: u32 },
    Expunge { path: String, seq: u32 },
    Vanished { path: String, uids: Vec<u32> },
    Flags { path: String, seq: u32, uid: Option<u32>, flags: Vec<String> },
    Closed,
}

#[derive(Debug, Clone, Default)]
pub struct Mailbox {
    pub path: String,
    pub exists: u32,
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub highest_modseq: Option<u64>,
    pub no_modseq: bool,
    pub read_only: bool,
    pub flags: Vec<String>,
    pub permanent_flags: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Namespace {
    pub prefix: String,
    pub delimiter: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ListEntry {
    pub path: String,
    pub path_as_listed: String,
    pub name: String,
    pub delimiter: Option<String>,
    pub parent_path: String,
    pub flags: HashSet<String>,
    pub special_use: Option<String>,
    pub subscribed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Status_ {
    pub messages: Option<u32>,
    pub unseen: Option<u32>,
    pub uid_next: Option<u32>,
    pub uid_validity: Option<u32>,
    pub highest_modseq: Option<u64>,
}

/// What to FETCH (imapflow's query object).
#[derive(Debug, Clone, Default)]
pub struct FetchQuery {
    pub uid: bool,
    pub flags: bool,
    pub envelope: bool,
    pub body_structure: bool,
    pub internal_date: bool,
    pub size: bool,
    pub headers: bool,
    /// `BODY.PEEK[HEADER.FIELDS (…)]`.
    pub header_fields: Vec<String>,
    pub body_parts: Vec<String>,
    /// `BODY.PEEK[]<start.max>`; `Some((0, None))` = the whole source.
    pub source: Option<(u64, Option<u64>)>,
    pub thread_id: bool,
    pub labels: bool,
}

/// UIDPLUS result of COPY/MOVE: source uid → destination uid.
#[derive(Debug, Clone, Default)]
pub struct CopyResult {
    pub uid_validity: Option<u32>,
    pub uid_map: Option<HashMap<u32, u32>>,
}

#[derive(Debug, Clone)]
pub struct CreateResult {
    pub path: String,
    pub created: bool,
}

enum Frame {
    Done { tag: String, status: Status, code: Option<String>, parsed_code: Option<ResponseCode<'static>>, text: String },
    Continue(#[allow(dead_code)] String),
    Untagged(Response<'static>),
    Unparsed,
}

pub struct Client {
    io: Option<BoxIo>,
    buf: Vec<u8>,
    start: usize,
    tag: u32,
    caps: HashSet<String>,
    enabled: HashSet<String>,
    namespace: Option<Namespace>,
    selected: Option<Mailbox>,
    events: Option<mpsc::UnboundedSender<Event>>,
    timeout: Duration,
    servername: String,
    port: u16,
    closed: bool,
    idling: bool,
}

fn quote(s: &str) -> String {
    if !s.is_empty() && s.len() < 1024 && !s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0 || b >= 0x80) {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        return out;
    }
    if s.is_empty() {
        return "\"\"".into();
    }
    // Non-ASCII (UTF8=ACCEPT): a literal is always valid.
    format!("{{{}+}}\r\n{s}", s.len())
}

fn find_crlf(b: &[u8], from: usize) -> Option<usize> {
    b[from..].windows(2).position(|w| w == b"\r\n").map(|p| p + from)
}

/// `{123}` / `{123+}` immediately before a line end.
fn literal_len(line: &[u8]) -> Option<usize> {
    if line.last() != Some(&b'}') {
        return None;
    }
    let open = line.iter().rposition(|&b| b == b'{')?;
    let inner = &line[open + 1..line.len() - 1];
    let inner = inner.strip_suffix(b"+").unwrap_or(inner);
    if inner.is_empty() || !inner.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(inner).ok()?.parse().ok()
}

fn bracket_code(text: &str) -> (Option<String>, String) {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let code = rest[..end].split_whitespace().next().unwrap_or("").to_uppercase();
        return (Some(code).filter(|c| !c.is_empty()), rest[end + 1..].trim().to_owned());
    }
    (None, t.to_owned())
}

fn code_name(c: &ResponseCode<'_>) -> String {
    match c {
        ResponseCode::Alert => "ALERT",
        ResponseCode::BadCharset(_) => "BADCHARSET",
        ResponseCode::Capabilities(_) => "CAPABILITY",
        ResponseCode::HighestModSeq(_) => "HIGHESTMODSEQ",
        ResponseCode::Parse => "PARSE",
        ResponseCode::PermanentFlags(_) => "PERMANENTFLAGS",
        ResponseCode::ReadOnly => "READ-ONLY",
        ResponseCode::ReadWrite => "READ-WRITE",
        ResponseCode::TryCreate => "TRYCREATE",
        ResponseCode::UidNext(_) => "UIDNEXT",
        ResponseCode::UidValidity(_) => "UIDVALIDITY",
        ResponseCode::Unseen(_) => "UNSEEN",
        ResponseCode::AppendUid(..) => "APPENDUID",
        ResponseCode::CopyUid(..) => "COPYUID",
        ResponseCode::UidNotSticky => "UIDNOTSTICKY",
        _ => "",
    }
    .to_owned()
}

pub fn expand_uid_set(set: &[UidSetMember]) -> Vec<u32> {
    let mut out = Vec::new();
    for m in set {
        match m {
            UidSetMember::Uid(u) => out.push(*u),
            UidSetMember::UidRange(r) => {
                let (a, b) = (*r.start(), *r.end());
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                if hi - lo <= 1_000_000 {
                    out.extend(lo..=hi);
                }
            }
        }
    }
    out
}

/// Compact a uid list into an IMAP sequence set (`1:3,7,9:10`).
pub fn uid_set(uids: &[u32]) -> String {
    let mut v: Vec<u32> = uids.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut out = String::new();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        let mut end = start;
        while i + 1 < v.len() && v[i + 1] == end + 1 {
            i += 1;
            end = v[i];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}:{end}"));
        }
        i += 1;
    }
    out
}

impl Client {
    /// Connect, authenticate and prepare the session (imapflow's `connect()`).
    pub async fn connect(cfg: &Config) -> Result<Client> {
        let tcp = transport::connect_tcp(&cfg.addresses, cfg.port, cfg.command_timeout).await?;
        let io: BoxIo =
            if cfg.secure { transport::tls(tcp, &cfg.servername, cfg.verify_tls).await? } else { Box::new(tcp) };
        let mut c = Client {
            io: Some(io),
            buf: Vec::with_capacity(64 * 1024),
            start: 0,
            tag: 0,
            caps: HashSet::new(),
            enabled: HashSet::new(),
            namespace: None,
            selected: None,
            events: None,
            timeout: cfg.command_timeout,
            servername: cfg.servername.clone(),
            port: cfg.port,
            closed: false,
            idling: false,
        };
        c.greeting().await?;
        if c.caps.is_empty() {
            c.capability().await?;
        }
        if !cfg.secure && c.caps.contains("STARTTLS") {
            c.run("STARTTLS").await?;
            if c.start < c.buf.len() {
                return Err(Error::Protocol("Unexpected data after STARTTLS".into()));
            }
            let io = c.io.take().ok_or(Error::Closed)?;
            c.io = Some(transport::tls(io, &cfg.servername, cfg.verify_tls).await?);
            c.caps.clear();
            c.capability().await?;
        }
        c.authenticate(&cfg.auth).await?;
        if !c.caps.iter().any(|x| x.starts_with("IMAP4REV1") || x.starts_with("IMAP4")) || c.caps.len() < 3 {
            c.capability().await?;
        }
        if c.caps.contains("NAMESPACE") {
            match c.run("NAMESPACE").await {
                Ok((_, _, raw)) => c.namespace = raw.iter().find_map(|l| parse_namespace(l)),
                Err(Error::Command { status: CmdStatus::Bad, text, .. })
                    if text.to_lowercase().contains("authenticated but not connected") =>
                {
                    return Err(Error::Auth { text, code: None });
                }
                Err(e) if e.is_fatal() => return Err(e),
                Err(_) => {}
            }
        }
        if c.namespace.is_none() {
            c.namespace = Some(Namespace::default());
        }
        if cfg.compress && c.caps.contains("COMPRESS=DEFLATE") && c.run("COMPRESS DEFLATE").await.is_ok() {
            let io = c.io.take().ok_or(Error::Closed)?;
            c.io = Some(Box::new(Deflate::new(io)));
        }
        if c.caps.contains("ENABLE") {
            let mut want = Vec::new();
            for ext in ["CONDSTORE", "UTF8=ACCEPT"] {
                if c.caps.contains(ext) {
                    want.push(ext);
                }
            }
            if !want.is_empty()
                && let Ok((_, _, lines)) = c.run(&format!("ENABLE {}", want.join(" "))).await
            {
                for l in lines {
                    let up = String::from_utf8_lossy(&l).to_uppercase();
                    if let Some(rest) = up.strip_prefix("* ENABLED") {
                        c.enabled.extend(rest.split_whitespace().map(str::to_owned));
                    }
                }
            }
        }
        Ok(c)
    }

    pub fn set_events(&mut self, tx: mpsc::UnboundedSender<Event>) {
        self.events = Some(tx);
    }

    pub fn has_capability(&self, cap: &str) -> bool {
        self.caps.contains(&cap.to_uppercase())
    }

    pub fn capabilities(&self) -> Vec<String> {
        let mut v: Vec<String> = self.caps.iter().cloned().collect();
        v.sort();
        v
    }

    pub fn is_enabled(&self, ext: &str) -> bool {
        self.enabled.contains(&ext.to_uppercase())
    }

    pub fn namespace(&self) -> Option<&Namespace> {
        self.namespace.as_ref()
    }

    pub fn mailbox(&self) -> Option<&Mailbox> {
        self.selected.as_ref()
    }

    pub fn is_usable(&self) -> bool {
        !self.closed && self.io.is_some()
    }

    fn utf8(&self) -> bool {
        self.enabled.contains("UTF8=ACCEPT")
    }

    /// imapflow `normalizePath`: INBOX is case-insensitive and the personal namespace prefix
    /// is applied to bare names.
    pub fn normalize_path(&self, path: &str) -> String {
        if path.eq_ignore_ascii_case("INBOX") {
            return "INBOX".into();
        }
        match &self.namespace {
            Some(ns) if !ns.prefix.is_empty() && !path.starts_with(&ns.prefix) => format!("{}{path}", ns.prefix),
            _ => path.to_owned(),
        }
    }

    fn encode_path(&self, path: &str) -> String {
        if self.utf8() { quote(path) } else { quote(&utf7::encode(path)) }
    }

    fn decode_path(&self, raw: &str) -> String {
        if self.utf8() { raw.to_owned() } else { utf7::decode(raw) }
    }

    // ── framing ──────────────────────────────────────────────────────────────────────────

    async fn fill(&mut self, wait: Option<Duration>) -> Result<()> {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > 256 * 1024 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        let io = self.io.as_mut().ok_or(Error::Closed)?;
        self.buf.reserve(32 * 1024);
        let wait = wait.unwrap_or(self.timeout);
        let n = match tokio::time::timeout(wait, io.read_buf(&mut self.buf)).await {
            Ok(r) => r?,
            Err(_) => return Err(Error::Timeout),
        };
        if n == 0 {
            return Err(Error::Closed);
        }
        Ok(())
    }

    /// One complete response (with literals inlined), including its final CRLF.
    async fn read_frame(&mut self, wait: Option<Duration>) -> Result<Vec<u8>> {
        let mut scan = self.start;
        loop {
            if let Some(eol) = find_crlf(&self.buf, scan) {
                if let Some(n) = literal_len(&self.buf[scan..eol]) {
                    let need = eol + 2 + n;
                    while self.buf.len() < need {
                        self.fill(wait).await?;
                    }
                    scan = need;
                    continue;
                }
                let frame = self.buf[self.start..eol + 2].to_vec();
                self.start = eol + 2;
                return Ok(frame);
            }
            let before = self.buf.len() - self.start;
            self.fill(wait).await?;
            scan = self.start + before.saturating_sub(1).min(self.buf.len() - self.start);
            scan = scan.max(self.start);
        }
    }

    async fn write(&mut self, data: &[u8]) -> Result<()> {
        let io = self.io.as_mut().ok_or(Error::Closed)?;
        let t = self.timeout;
        let fut = async {
            io.write_all(data).await?;
            io.flush().await
        };
        match tokio::time::timeout(t, fut).await {
            Ok(r) => r.map_err(Error::from),
            Err(_) => Err(Error::Timeout),
        }
    }

    fn parse(frame: &[u8]) -> Frame {
        match Response::parse(frame) {
            Ok((_, resp)) => match resp {
                Response::Done { tag, status, outcome } => {
                    let info = outcome.information.as_deref().unwrap_or("").to_owned();
                    let (code, text) = match &outcome.code {
                        Some(c) => (Some(code_name(c)).filter(|s| !s.is_empty()), info),
                        None => bracket_code(&info),
                    };
                    Frame::Done { tag: tag.0, status, code, parsed_code: outcome.code.map(|c| c.into_owned()), text }
                }
                Response::Continue(o) => Frame::Continue(o.information.as_deref().unwrap_or("").to_owned()),
                other => Frame::Untagged(other.into_owned()),
            },
            Err(_) => {
                // Lenient fallback for responses the strict grammar rejects.
                let line = String::from_utf8_lossy(frame);
                let line = line.trim_end();
                if let Some(rest) = line.strip_prefix('+') {
                    return Frame::Continue(rest.trim().to_owned());
                }
                let mut it = line.splitn(3, ' ');
                let (tag, word, rest) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
                if tag != "*" && !tag.is_empty() {
                    let status = match word.to_uppercase().as_str() {
                        "OK" => Status::Ok,
                        "NO" => Status::No,
                        "BAD" => Status::Bad,
                        _ => return Frame::Unparsed,
                    };
                    let (code, text) = bracket_code(rest);
                    return Frame::Done { tag: tag.to_owned(), status, code, parsed_code: None, text };
                }
                Frame::Unparsed
            }
        }
    }

    fn emit(&self, ev: Event) {
        if let Some(tx) = &self.events {
            let _ = tx.send(ev);
        }
    }

    /// Mailbox-state bookkeeping for unsolicited data (shared by every command and IDLE).
    fn track(&mut self, resp: &Response<'static>, in_fetch: bool) {
        let Some(mb) = self.selected.as_mut() else { return };
        match resp {
            Response::MailboxData(MailboxDatum::Exists(n)) => {
                let prev = mb.exists;
                mb.exists = *n;
                if *n != prev {
                    let path = mb.path.clone();
                    self.emit(Event::Exists { path, count: *n, prev_count: prev });
                }
            }
            Response::Expunge(seq) => {
                mb.exists = mb.exists.saturating_sub(1);
                let path = mb.path.clone();
                self.emit(Event::Expunge { path, seq: *seq });
            }
            Response::Vanished { earlier: false, uids } => {
                let path = mb.path.clone();
                let list: Vec<u32> = uids.iter().flat_map(|r| r.clone()).collect();
                mb.exists = mb.exists.saturating_sub(list.len() as u32);
                self.emit(Event::Vanished { path, uids: list });
            }
            Response::Fetch(seq, attrs) if !in_fetch => {
                let mut uid = None;
                let mut flags = None;
                for a in attrs {
                    match a {
                        imap_proto::AttributeValue::Uid(u) => uid = Some(*u),
                        imap_proto::AttributeValue::Flags(f) => flags = Some(f.iter().map(|s| s.to_string()).collect()),
                        imap_proto::AttributeValue::ModSeq(m) => {
                            if mb.highest_modseq.is_some_and(|h| h < *m) {
                                mb.highest_modseq = Some(*m);
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(flags) = flags {
                    let path = mb.path.clone();
                    self.emit(Event::Flags { path, seq: *seq, uid, flags });
                }
            }
            Response::Data { status: Status::Bye, .. } => {}
            _ => {}
        }
    }

    fn mark_closed(&mut self) {
        if !self.closed {
            self.closed = true;
            self.io = None;
            self.emit(Event::Closed);
        }
    }

    /// Send a tagged command; returns (tagged code, text, raw untagged lines) and routes parsed
    /// untagged data to `on`.
    async fn exec(
        &mut self,
        cmd: &str,
        mut on: impl FnMut(&mut Self, Response<'static>),
    ) -> Result<(Option<ResponseCode<'static>>, String, Vec<Vec<u8>>)> {
        if self.closed {
            return Err(Error::Closed);
        }
        self.tag += 1;
        let tag = format!("A{}", self.tag);
        let r = self.exec_inner(&tag, cmd, &mut on).await;
        if let Err(e) = &r
            && e.is_fatal()
        {
            self.mark_closed();
        }
        r
    }

    async fn exec_inner(
        &mut self,
        tag: &str,
        cmd: &str,
        on: &mut impl FnMut(&mut Self, Response<'static>),
    ) -> Result<(Option<ResponseCode<'static>>, String, Vec<Vec<u8>>)> {
        // Literals in the command: send synchronising ones piecewise, LITERAL+ ones inline.
        let full = format!("{tag} {cmd}\r\n");
        let bytes = full.as_bytes();
        let plus = self.caps.contains("LITERAL+") || self.caps.contains("LITERAL-");
        let mut pos = 0;
        let mut send_from = 0;
        while let Some(eol) = find_crlf(bytes, pos) {
            if eol + 2 >= bytes.len() {
                break;
            }
            let line = &bytes[pos..eol];
            if let Some(n) = literal_len(line) {
                let sync = !line.ends_with(b"+}");
                if sync || !plus {
                    let mut head = bytes[send_from..eol].to_vec();
                    if !sync {
                        // Server lacks LITERAL+: downgrade to a synchronising literal.
                        let l = head.len();
                        head.remove(l - 2);
                    }
                    head.extend_from_slice(b"\r\n");
                    self.write(&head).await?;
                    self.await_continue(tag).await?;
                    send_from = eol + 2;
                }
                pos = eol + 2 + n;
            } else {
                pos = eol + 2;
            }
        }
        self.write(&bytes[send_from..]).await?;
        self.collect(tag, on).await
    }

    async fn await_continue(&mut self, tag: &str) -> Result<()> {
        loop {
            let frame = self.read_frame(None).await?;
            match Self::parse(&frame) {
                Frame::Continue(_) => return Ok(()),
                Frame::Done { tag: t, status, code, text, .. } if t == tag => {
                    return Err(match status {
                        Status::Bad => Error::Command { status: CmdStatus::Bad, code, text },
                        _ => Error::Command { status: CmdStatus::No, code, text },
                    });
                }
                Frame::Untagged(r) => {
                    let r2 = r;
                    self.track(&r2, false);
                }
                _ => {}
            }
        }
    }

    async fn collect(
        &mut self,
        tag: &str,
        on: &mut impl FnMut(&mut Self, Response<'static>),
    ) -> Result<(Option<ResponseCode<'static>>, String, Vec<Vec<u8>>)> {
        let mut raw = Vec::new();
        loop {
            let frame = self.read_frame(None).await?;
            match Self::parse(&frame) {
                Frame::Done { tag: t, status, code, parsed_code, text } if t == tag => {
                    return match status {
                        Status::Ok => Ok((parsed_code, text, raw)),
                        Status::Bad => Err(Error::Command { status: CmdStatus::Bad, code, text }),
                        _ => Err(Error::Command { status: CmdStatus::No, code, text }),
                    };
                }
                Frame::Done { .. } | Frame::Continue(_) => {}
                Frame::Untagged(r) => {
                    if let Response::Data { status: Status::Bye, outcome } = &r {
                        let text = outcome.information.as_deref().unwrap_or("").to_owned();
                        // A BYE before the tagged reply ends the session; keep reading for
                        // the tag (LOGOUT) but remember the reason.
                        raw.push(frame.clone());
                        if !text.is_empty() {
                            tracing::debug!("IMAP BYE: {text}");
                        }
                    } else {
                        // imap-proto also reports `* ENABLED …` as Capabilities.
                        if let Response::Capabilities(c) = &r
                            && frame.len() > 12
                            && frame[..12].eq_ignore_ascii_case(b"* CAPABILITY")
                        {
                            self.caps = c.iter().filter_map(cap_name).collect();
                        }
                        if let Response::Data { outcome, .. } = &r
                            && let Some(ResponseCode::Capabilities(c)) = &outcome.code
                        {
                            self.caps = c.iter().filter_map(cap_name).collect();
                        }
                        raw.push(frame.clone());
                    }
                    on(self, r);
                }
                Frame::Unparsed => raw.push(frame),
            }
        }
    }

    /// Run a command whose untagged data only feeds mailbox tracking.
    pub async fn run(&mut self, cmd: &str) -> Result<(Option<ResponseCode<'static>>, String, Vec<Vec<u8>>)> {
        self.exec(cmd, |c, r| c.track(&r, false)).await
    }

    async fn greeting(&mut self) -> Result<()> {
        let frame = self.read_frame(None).await?;
        match Self::parse(&frame) {
            Frame::Untagged(Response::Data { status: Status::Bye, outcome }) => Err(Error::Command {
                status: CmdStatus::No,
                code: None,
                text: outcome.information.unwrap_or_default().into_owned(),
            }),
            Frame::Untagged(Response::Data { outcome, .. }) => {
                if let Some(ResponseCode::Capabilities(c)) = &outcome.code {
                    self.caps = c.iter().filter_map(cap_name).collect();
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn capability(&mut self) -> Result<()> {
        self.run("CAPABILITY").await?;
        Ok(())
    }

    async fn authenticate(&mut self, auth: &Auth) -> Result<()> {
        let to_auth = |e: Error| match e {
            Error::Command { text, code, .. } => Error::Auth { text, code },
            other => other,
        };
        let result = match auth {
            Auth::OAuth { user, token } => {
                let (mech, payload) = if self.caps.contains("AUTH=OAUTHBEARER") {
                    (
                        "OAUTHBEARER",
                        format!(
                            "n,a={user},\x01host={}\x01port={}\x01auth=Bearer {token}\x01\x01",
                            self.servername, self.port
                        ),
                    )
                } else {
                    ("XOAUTH2", format!("user={user}\x01auth=Bearer {token}\x01\x01"))
                };
                let cmd = format!("AUTHENTICATE {mech} {}", B64.encode(payload));
                self.sasl(&cmd, if mech == "OAUTHBEARER" { "AQ==" } else { "" }).await
            }
            Auth::Password { user, pass } => {
                let pass = pass.clone().unwrap_or_default();
                if self.caps.contains("AUTH=PLAIN") {
                    let payload = B64.encode(format!("\0{user}\0{pass}"));
                    self.sasl(&format!("AUTHENTICATE PLAIN {payload}"), "").await
                } else {
                    self.run(&format!("LOGIN {} {}", quote(user), quote(&pass))).await.map(|(c, _, _)| c)
                }
            }
        };
        match result {
            Ok(code) => {
                if let Some(ResponseCode::Capabilities(c)) = code {
                    self.caps = c.iter().filter_map(cap_name).collect();
                } else {
                    self.caps.clear();
                    self.capability().await?;
                }
                Ok(())
            }
            Err(e) => Err(to_auth(e)),
        }
    }

    /// SASL with an initial response; a `+` challenge (an error payload) is answered with
    /// `breaker` so the server sends its tagged NO.
    async fn sasl(&mut self, cmd: &str, breaker: &str) -> Result<Option<ResponseCode<'static>>> {
        if self.closed {
            return Err(Error::Closed);
        }
        self.tag += 1;
        let tag = format!("A{}", self.tag);
        self.write(format!("{tag} {cmd}\r\n").as_bytes()).await?;
        loop {
            let frame = self.read_frame(None).await?;
            match Self::parse(&frame) {
                Frame::Continue(_) => self.write(format!("{breaker}\r\n").as_bytes()).await?,
                Frame::Done { tag: t, status, code, parsed_code, text } if t == tag => {
                    return match status {
                        Status::Ok => Ok(parsed_code),
                        Status::Bad => Err(Error::Command { status: CmdStatus::Bad, code, text }),
                        _ => Err(Error::Command { status: CmdStatus::No, code, text }),
                    };
                }
                Frame::Untagged(Response::Capabilities(c)) => self.caps = c.iter().filter_map(cap_name).collect(),
                _ => {}
            }
        }
    }

    // ── mailboxes ────────────────────────────────────────────────────────────────────────

    /// SELECT (imapflow `mailboxOpen`); a no-op when the path is already selected.
    pub async fn select(&mut self, path: &str) -> Result<&Mailbox> {
        let path = self.normalize_path(path);
        if self.selected.as_ref().is_some_and(|m| m.path == path) {
            return Ok(self.selected.as_ref().expect("selected"));
        }
        let condstore = self.caps.contains("CONDSTORE") && !self.enabled.contains("CONDSTORE");
        let cmd = format!("SELECT {}{}", self.encode_path(&path), if condstore { " (CONDSTORE)" } else { "" });
        let mut mb = Mailbox { path: path.clone(), ..Default::default() };
        // Unsolicited data for the old mailbox must not leak into the new one.
        self.selected = None;
        let res = self
            .exec(&cmd, |_, r| match r {
                Response::MailboxData(MailboxDatum::Exists(n)) => mb.exists = n,
                Response::MailboxData(MailboxDatum::Flags(f)) => {
                    mb.flags = f.into_iter().map(|s| s.into_owned()).collect()
                }
                Response::Data { outcome, .. } => match outcome.code {
                    Some(ResponseCode::UidValidity(v)) => mb.uid_validity = Some(v),
                    Some(ResponseCode::UidNext(v)) => mb.uid_next = Some(v),
                    Some(ResponseCode::HighestModSeq(v)) => mb.highest_modseq = Some(v),
                    Some(ResponseCode::PermanentFlags(f)) => {
                        mb.permanent_flags = f.into_iter().map(|s| s.into_owned()).collect()
                    }
                    None => {
                        if outcome.information.as_deref().is_some_and(|t| t.to_uppercase().contains("[NOMODSEQ]")) {
                            mb.no_modseq = true;
                        }
                    }
                    _ => {}
                },
                _ => {}
            })
            .await;
        let (code, _, _) = res?;
        if matches!(code, Some(ResponseCode::ReadOnly)) {
            mb.read_only = true;
        }
        self.selected = Some(mb);
        Ok(self.selected.as_ref().expect("selected"))
    }

    /// Force the next `select` of the same path to re-SELECT (refreshes EXISTS/UIDNEXT).
    pub fn forget_selection(&mut self) {
        self.selected = None;
    }

    pub async fn noop(&mut self) -> Result<()> {
        self.run("NOOP").await.map(|_| ())
    }

    pub async fn status(&mut self, path: &str) -> Result<Status_> {
        let path = self.normalize_path(path);
        let mut items = vec!["MESSAGES", "UIDNEXT", "UIDVALIDITY", "UNSEEN"];
        if self.caps.contains("CONDSTORE") {
            items.push("HIGHESTMODSEQ");
        }
        let mut st = Status_::default();
        self.exec(&format!("STATUS {} ({})", self.encode_path(&path), items.join(" ")), |c, r| {
            if let Response::MailboxData(MailboxDatum::Status { status, .. }) = &r {
                for a in status {
                    match a {
                        StatusAttribute::Messages(n) => st.messages = Some(*n),
                        StatusAttribute::Unseen(n) => st.unseen = Some(*n),
                        StatusAttribute::UidNext(n) => st.uid_next = Some(*n),
                        StatusAttribute::UidValidity(n) => st.uid_validity = Some(*n),
                        StatusAttribute::HighestModSeq(n) => st.highest_modseq = Some(*n),
                        _ => {}
                    }
                }
            } else {
                c.track(&r, false);
            }
        })
        .await?;
        Ok(st)
    }

    /// imapflow `list()`: LIST "" "*" (SPECIAL-USE flags when offered), LSUB for subscriptions,
    /// special-use detection by flag or localized name, and imapflow's ordering.
    pub async fn list(&mut self) -> Result<Vec<ListEntry>> {
        let has_su = self.caps.contains("SPECIAL-USE");
        let xlist = self.caps.contains("XLIST") && !has_su;
        let cmd = if xlist { "XLIST" } else { "LIST" };
        let ext = self.caps.contains("LIST-EXTENDED");
        let mut tries: Vec<String> = Vec::new();
        if !xlist && ext {
            let mut ret = Vec::new();
            if has_su {
                ret.push("SPECIAL-USE");
            }
            ret.push("CHILDREN");
            ret.push("SUBSCRIBED");
            tries.push(format!("{cmd} \"\" \"*\" RETURN ({})", ret.join(" ")));
        }
        tries.push(format!("{cmd} \"\" \"*\""));
        let mut raw_entries: Vec<(Vec<String>, Option<String>, String)> = Vec::new();
        let mut used_subscribed = false;
        for (i, attempt) in tries.iter().enumerate() {
            raw_entries.clear();
            let res = self
                .exec(attempt, |c, r| {
                    if let Response::MailboxData(MailboxDatum::List(l)) = &r {
                        raw_entries.push((
                            l.name_attributes.iter().map(name_attr).collect(),
                            l.delimiter.as_ref().map(|d| d.to_string()),
                            l.name.to_string(),
                        ));
                    } else {
                        c.track(&r, false);
                    }
                })
                .await;
            match res {
                Ok(_) => {
                    used_subscribed = i == 0 && tries.len() > 1;
                    break;
                }
                Err(Error::Command { status: CmdStatus::Bad, .. }) if i + 1 < tries.len() => continue,
                Err(e) => return Err(e),
            }
        }
        let mut entries: Vec<ListEntry> =
            raw_entries.into_iter().map(|(flags, delimiter, name)| self.list_entry(flags, delimiter, &name)).collect();
        let mut matches: HashMap<String, Vec<(usize, &'static str)>> = HashMap::new();
        for (i, e) in entries.iter_mut().enumerate() {
            if e.flags.remove("\\Subscribed") {
                e.subscribed = true;
            }
            if e.flags.contains("\\NonExistent") {
                e.flags.insert("\\Noselect".into());
            }
            if xlist && e.flags.remove("\\Inbox") && e.path != "INBOX" {
                matches.entry("\\Inbox".into()).or_default().push((i, "extension"));
            }
            if e.path.eq_ignore_ascii_case("INBOX") && !e.flags.contains("\\NonExistent") {
                matches.entry("\\Inbox".into()).or_default().push((i, "name"));
            }
            if let Some((flag, source)) = special_use::detect(xlist || has_su, &e.flags, &e.name)
                && (!(source == "name" || source == "name-guess") || !e.flags.contains("\\NonExistent"))
            {
                matches.entry(flag.to_owned()).or_default().push((i, source));
            }
        }
        let any_subscribed = entries.iter().any(|e| e.subscribed);
        let mut subscription_known = used_subscribed && any_subscribed;
        if (!used_subscribed || !any_subscribed)
            && let Ok(subs) = self.lsub().await
        {
            subscription_known = true;
            for (flags, path) in subs {
                if let Some(e) = entries.iter_mut().find(|e| e.path == path) {
                    e.subscribed = true;
                    e.flags.extend(flags);
                    if e.flags.remove("\\Subscribed") {
                        e.subscribed = true;
                    }
                    if e.flags.contains("\\NonExistent") {
                        e.flags.insert("\\Noselect".into());
                    }
                }
            }
        }
        const SOURCES: [&str; 4] = ["user", "extension", "name", "name-guess"];
        for (flag, mut list) in matches {
            list.sort_by(|a, b| {
                let (sa, sb) = (
                    SOURCES.iter().position(|s| *s == a.1).unwrap_or(9),
                    SOURCES.iter().position(|s| *s == b.1).unwrap_or(9),
                );
                sa.cmp(&sb).then_with(|| entries[a.0].path.cmp(&entries[b.0].path))
            });
            let idx = list[0].0;
            if entries[idx].special_use.is_none() {
                entries[idx].special_use = Some(flag);
            }
        }
        if !subscription_known && !entries.iter().any(|e| e.subscribed) {
            for e in entries.iter_mut().filter(|e| !e.flags.contains("\\NonExistent")) {
                e.subscribed = true;
            }
        }
        if let Some(inbox) = entries.iter_mut().find(|e| e.special_use.as_deref() == Some("\\Inbox")) {
            inbox.subscribed = true;
        }
        const ORDER: [&str; 8] =
            ["\\Inbox", "\\Flagged", "\\Sent", "\\Drafts", "\\All", "\\Archive", "\\Junk", "\\Trash"];
        let rank = |e: &ListEntry| e.special_use.as_deref().and_then(|s| ORDER.iter().position(|o| *o == s));
        entries.sort_by(|a, b| match (rank(a), rank(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.path.cmp(&b.path),
        });
        Ok(entries)
    }

    fn list_entry(&self, flags: Vec<String>, delimiter: Option<String>, raw: &str) -> ListEntry {
        let mut path = self.normalize_path(&self.decode_path(raw));
        if let Some(d) = delimiter.as_deref().filter(|d| !d.is_empty())
            && path.starts_with(d)
        {
            path = path[d.len()..].to_owned();
        }
        let (parent_path, name) = match delimiter.as_deref().filter(|d| !d.is_empty()) {
            Some(d) => match path.rfind(d) {
                Some(i) => (path[..i].to_owned(), path[i + d.len()..].to_owned()),
                None => (String::new(), path.clone()),
            },
            None => (String::new(), path.clone()),
        };
        ListEntry {
            path,
            path_as_listed: raw.to_owned(),
            name,
            delimiter,
            parent_path,
            flags: flags.into_iter().collect(),
            special_use: None,
            subscribed: false,
        }
    }

    async fn lsub(&mut self) -> Result<Vec<(Vec<String>, String)>> {
        let mut out = Vec::new();
        self.exec("LSUB \"\" \"*\"", |c, r| {
            if let Response::MailboxData(MailboxDatum::List(l)) = &r {
                let mut path = c.normalize_path(&c.decode_path(&l.name));
                if let Some(d) = l.delimiter.as_deref().filter(|d| !d.is_empty())
                    && path.starts_with(d)
                {
                    path = path[d.len()..].to_owned();
                }
                out.push((l.name_attributes.iter().map(name_attr).collect(), path));
            } else {
                c.track(&r, false);
            }
        })
        .await?;
        Ok(out)
    }

    /// imapflow `mailboxCreate(path.split('/'))`: segments joined with the server delimiter,
    /// ALREADYEXISTS reported as `created: false`.
    pub async fn create(&mut self, segments: &[&str]) -> Result<CreateResult> {
        let delim = self.namespace.as_ref().and_then(|n| n.delimiter.clone()).unwrap_or_default();
        let path = self.normalize_path(&segments.join(&delim));
        match self.run(&format!("CREATE {}", self.encode_path(&path))).await {
            Ok(_) => Ok(CreateResult { path, created: true }),
            Err(Error::Command { code: Some(c), .. }) if c == "ALREADYEXISTS" => {
                Ok(CreateResult { path, created: false })
            }
            Err(e) => Err(e),
        }
    }

    pub async fn delete_mailbox(&mut self, path: &str) -> Result<()> {
        let path = self.normalize_path(path);
        if self.selected.as_ref().is_some_and(|m| m.path == path) {
            let _ = self.run("CLOSE").await;
            self.selected = None;
        }
        self.run(&format!("DELETE {}", self.encode_path(&path))).await.map(|_| ())
    }

    pub async fn rename_mailbox(&mut self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (self.normalize_path(from), self.normalize_path(to));
        if self.selected.as_ref().is_some_and(|m| m.path == from) {
            let _ = self.run("CLOSE").await;
            self.selected = None;
        }
        self.run(&format!("RENAME {} {}", self.encode_path(&from), self.encode_path(&to))).await.map(|_| ())
    }

    // ── messages ─────────────────────────────────────────────────────────────────────────

    fn fetch_items(&self, q: &FetchQuery) -> String {
        let mut items: Vec<String> = Vec::new();
        if q.uid {
            items.push("UID".into());
        }
        if q.flags {
            items.push("FLAGS".into());
        }
        if q.envelope {
            items.push("ENVELOPE".into());
        }
        if q.body_structure {
            items.push("BODYSTRUCTURE".into());
        }
        if q.internal_date {
            items.push("INTERNALDATE".into());
        }
        if q.size {
            items.push("RFC822.SIZE".into());
        }
        if self.caps.contains("CONDSTORE") && (q.flags || q.uid) && self.enabled.contains("CONDSTORE") {
            items.push("MODSEQ".into());
        }
        if q.thread_id {
            if self.caps.contains("X-GM-EXT-1") {
                items.push("X-GM-MSGID".into());
                items.push("X-GM-THRID".into());
            } else if self.caps.contains("OBJECTID") {
                items.push("EMAILID".into());
                items.push("THREADID".into());
            }
        }
        if q.labels && self.caps.contains("X-GM-EXT-1") {
            items.push("X-GM-LABELS".into());
        }
        if q.headers {
            items.push("BODY.PEEK[HEADER]".into());
        } else if !q.header_fields.is_empty() {
            items.push(format!("BODY.PEEK[HEADER.FIELDS ({})]", q.header_fields.join(" ").to_uppercase()));
        }
        for p in &q.body_parts {
            items.push(format!("BODY.PEEK[{p}]"));
        }
        if let Some((start, max)) = q.source {
            match max {
                Some(m) => items.push(format!("BODY.PEEK[]<{start}.{m}>")),
                None if start > 0 => items.push(format!("BODY.PEEK[]<{start}.{}>", u32::MAX)),
                None => items.push("BODY.PEEK[]".into()),
            }
        }
        if items.is_empty() {
            items.push("UID".into());
        }
        format!("({})", items.join(" "))
    }

    /// FETCH (by UID when `by_uid`), optionally `CHANGEDSINCE`. Results keep server order.
    pub async fn fetch(
        &mut self,
        range: &str,
        q: &FetchQuery,
        by_uid: bool,
        changed_since: Option<u64>,
    ) -> Result<Vec<Message>> {
        let mut q = q.clone();
        q.uid = q.uid || by_uid;
        let mut cmd = format!("{}FETCH {range} {}", if by_uid { "UID " } else { "" }, self.fetch_items(&q));
        if let Some(m) = changed_since
            && self.caps.contains("CONDSTORE")
        {
            cmd.push_str(&format!(" (CHANGEDSINCE {m})"));
        }
        let mut out: Vec<Message> = Vec::new();
        let mut index: HashMap<u32, usize> = HashMap::new();
        self.exec(&cmd, |c, r| {
            if let Response::Fetch(seq, attrs) = r {
                let has_uid = attrs.iter().any(|a| matches!(a, imap_proto::AttributeValue::Uid(_)));
                if by_uid && !has_uid {
                    // An unsolicited flag update for a message outside this request.
                    c.track(&Response::Fetch(seq, attrs), false);
                    return;
                }
                let i = *index.entry(seq).or_insert_with(|| {
                    out.push(Message { seq, ..Default::default() });
                    out.len() - 1
                });
                out[i].absorb(attrs);
                if let (Some(m), Some(mb)) = (out[i].modseq, c.selected.as_mut())
                    && mb.highest_modseq.is_none_or(|h| h < m)
                {
                    mb.highest_modseq = Some(m);
                }
                if let (Some(u), Some(mb)) = (out[i].uid, c.selected.as_mut())
                    && mb.uid_next.is_none_or(|n| n <= u)
                {
                    mb.uid_next = Some(u + 1);
                }
            } else {
                c.track(&r, true);
            }
        })
        .await?;
        Ok(out)
    }

    /// `UID SEARCH <criteria>` (criteria in IMAP syntax).
    pub async fn search_uids(&mut self, criteria: &str) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let charset = if criteria.bytes().any(|b| b >= 0x80) && !self.utf8() { "CHARSET UTF-8 " } else { "" };
        self.exec(&format!("UID SEARCH {charset}{criteria}"), |c, r| match r {
            Response::MailboxData(MailboxDatum::Search(v)) => out.extend(v),
            other => c.track(&other, false),
        })
        .await?;
        Ok(out)
    }

    /// `UID STORE <set> +FLAGS.SILENT (…)` / `-FLAGS.SILENT`.
    pub async fn store_flags(&mut self, uids: &str, add: bool, flags: &[&str]) -> Result<()> {
        self.run(&format!("UID STORE {uids} {}FLAGS.SILENT ({})", if add { '+' } else { '-' }, flags.join(" ")))
            .await
            .map(|_| ())
    }

    fn copy_result(code: Option<ResponseCode<'static>>, raw: &[Vec<u8>]) -> CopyResult {
        let mut res = CopyResult::default();
        let mut apply = |c: &ResponseCode<'_>| {
            if let ResponseCode::CopyUid(validity, src, dst) = c {
                let (s, d) = (expand_uid_set(src), expand_uid_set(dst));
                if s.len() == d.len() {
                    let map = res.uid_map.get_or_insert_with(HashMap::new);
                    map.extend(s.into_iter().zip(d));
                }
                res.uid_validity = Some(*validity);
            }
        };
        if let Some(c) = &code {
            apply(c);
        }
        // MOVE reports COPYUID in an untagged OK.
        for line in raw {
            if let Ok((_, Response::Data { outcome, .. })) = Response::parse(line)
                && let Some(c) = &outcome.code
            {
                apply(c);
            }
        }
        res
    }

    pub async fn copy(&mut self, uids: &str, dest: &str) -> Result<CopyResult> {
        let dest = self.normalize_path(dest);
        let (code, _, raw) = self.run(&format!("UID COPY {uids} {}", self.encode_path(&dest))).await?;
        Ok(Self::copy_result(code, &raw))
    }

    /// MOVE, or COPY + \Deleted + (UID) EXPUNGE on servers without it (imapflow `messageMove`).
    pub async fn mv(&mut self, uids: &str, dest: &str) -> Result<CopyResult> {
        let dest = self.normalize_path(dest);
        if self.caps.contains("MOVE") {
            let (code, _, raw) = self.run(&format!("UID MOVE {uids} {}", self.encode_path(&dest))).await?;
            return Ok(Self::copy_result(code, &raw));
        }
        let res = self.copy(uids, &dest).await?;
        self.delete_messages(uids).await?;
        Ok(res)
    }

    /// Flag `\Deleted` and expunge exactly those messages (imapflow `messageDelete`).
    pub async fn delete_messages(&mut self, uids: &str) -> Result<()> {
        self.store_flags(uids, true, &["\\Deleted"]).await?;
        if self.caps.contains("UIDPLUS") {
            self.run(&format!("UID EXPUNGE {uids}")).await?;
        } else {
            self.run("EXPUNGE").await?;
        }
        Ok(())
    }

    /// APPEND; returns the UIDPLUS (uidvalidity, uid) when the server reports it.
    pub async fn append(
        &mut self,
        path: &str,
        flags: &[&str],
        internal_date: Option<&str>,
        content: &[u8],
    ) -> Result<Option<(u32, u32)>> {
        let path = self.normalize_path(path);
        let plus = self.caps.contains("LITERAL+") || (self.caps.contains("LITERAL-") && content.len() <= 4096);
        let mut head = format!("APPEND {}", self.encode_path(&path));
        if !flags.is_empty() {
            head.push_str(&format!(" ({})", flags.join(" ")));
        }
        if let Some(d) = internal_date {
            head.push_str(&format!(" \"{d}\""));
        }
        let binary = content.contains(&0);
        head.push_str(&format!(
            " {}{{{}{}}}",
            if binary && self.caps.contains("BINARY") { "~" } else { "" },
            content.len(),
            if plus { "+" } else { "" }
        ));
        if self.closed {
            return Err(Error::Closed);
        }
        self.tag += 1;
        let tag = format!("A{}", self.tag);
        let run = async {
            self.write(format!("{tag} {head}\r\n").as_bytes()).await?;
            if !plus {
                self.await_continue(&tag).await?;
            }
            let mut body = content.to_vec();
            body.extend_from_slice(b"\r\n");
            self.write(&body).await?;
            self.collect(&tag, &mut |c, r| c.track(&r, false)).await
        };
        match run.await {
            Ok((Some(ResponseCode::AppendUid(validity, uids)), _, _)) => {
                Ok(expand_uid_set(&uids).first().map(|u| (validity, *u)))
            }
            Ok(_) => Ok(None),
            Err(e) => {
                if e.is_fatal() {
                    self.mark_closed();
                }
                Err(e)
            }
        }
    }

    // ── IDLE ─────────────────────────────────────────────────────────────────────────────

    /// IDLE until `stop` resolves or `max` elapses, then DONE. Mailbox events are emitted on
    /// the event channel as they arrive. Falls back to NOOP polling without IDLE support.
    pub async fn idle(&mut self, max: Duration, stop: impl std::future::Future<Output = ()>) -> Result<()> {
        if !self.caps.contains("IDLE") {
            tokio::select! {
                _ = tokio::time::sleep(max.min(Duration::from_secs(120))) => {}
                _ = stop => {}
            }
            return self.noop().await;
        }
        if self.closed {
            return Err(Error::Closed);
        }
        self.tag += 1;
        let tag = format!("A{}", self.tag);
        let r = self.idle_inner(&tag, max, stop).await;
        self.idling = false;
        if let Err(e) = &r
            && e.is_fatal()
        {
            self.mark_closed();
        }
        r
    }

    async fn idle_inner(
        &mut self,
        tag: &str,
        max: Duration,
        stop: impl std::future::Future<Output = ()>,
    ) -> Result<()> {
        self.write(format!("{tag} IDLE\r\n").as_bytes()).await?;
        self.await_continue(tag).await?;
        self.idling = true;
        let deadline = tokio::time::Instant::now() + max;
        tokio::pin!(stop);
        loop {
            // Wait for data without the command timeout: an idle socket is silent by design.
            let ready = {
                let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
                if self.start < self.buf.len() && find_crlf(&self.buf, self.start).is_some() {
                    true
                } else {
                    tokio::select! {
                        r = self.fill(Some(wait.max(Duration::from_millis(1)))) => match r {
                            Ok(()) => true,
                            Err(Error::Timeout) => false,
                            Err(e) => return Err(e),
                        },
                        _ = &mut stop => false,
                    }
                }
            };
            if !ready {
                break;
            }
            while self.start < self.buf.len() && find_crlf(&self.buf, self.start).is_some() {
                let frame = self.read_frame(None).await?;
                match Self::parse(&frame) {
                    Frame::Untagged(Response::Data { status: Status::Bye, .. }) => return Err(Error::Closed),
                    Frame::Untagged(r) => self.track(&r, false),
                    Frame::Done { tag: t, status, code, text, .. } if t == tag => {
                        // The server ended IDLE on its own.
                        return match status {
                            Status::Ok => Ok(()),
                            _ => Err(Error::Command { status: CmdStatus::No, code, text }),
                        };
                    }
                    _ => {}
                }
            }
        }
        self.write(b"DONE\r\n").await?;
        self.collect(tag, &mut |c, r| c.track(&r, false)).await.map(|_| ())
    }

    pub async fn logout(&mut self) {
        if self.closed {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.run("LOGOUT")).await;
        self.close();
    }

    /// Drop the socket immediately (imapflow `close()`).
    pub fn close(&mut self) {
        self.mark_closed();
    }
}

fn cap_name(c: &imap_proto::Capability<'_>) -> Option<String> {
    match c {
        imap_proto::Capability::Imap4rev1 => Some("IMAP4REV1".into()),
        imap_proto::Capability::Auth(a) => Some(format!("AUTH={}", a.to_uppercase())),
        imap_proto::Capability::Atom(a) => Some(a.to_uppercase()),
    }
}

fn name_attr(a: &NameAttribute<'_>) -> String {
    match a {
        NameAttribute::NoInferiors => "\\Noinferiors".into(),
        NameAttribute::NoSelect => "\\Noselect".into(),
        NameAttribute::Marked => "\\Marked".into(),
        NameAttribute::Unmarked => "\\Unmarked".into(),
        NameAttribute::All => "\\All".into(),
        NameAttribute::Archive => "\\Archive".into(),
        NameAttribute::Drafts => "\\Drafts".into(),
        NameAttribute::Flagged => "\\Flagged".into(),
        NameAttribute::Junk => "\\Junk".into(),
        NameAttribute::Sent => "\\Sent".into(),
        NameAttribute::Trash => "\\Trash".into(),
        NameAttribute::Extension(s) => {
            // imapflow keeps flags as sent, apart from the case-normalised standard ones.
            let lower = s.to_lowercase();
            match lower.as_str() {
                "\\nonexistent" => "\\NonExistent".into(),
                "\\subscribed" => "\\Subscribed".into(),
                "\\haschildren" => "\\HasChildren".into(),
                "\\hasnochildren" => "\\HasNoChildren".into(),
                "\\important" => "\\Important".into(),
                "\\inbox" => "\\Inbox".into(),
                "\\noselect" => "\\Noselect".into(),
                _ => s.to_string(),
            }
        }
        _ => String::new(),
    }
}

/// `* NAMESPACE (("INBOX." ".")) NIL NIL` → the first personal namespace.
fn parse_namespace(line: &[u8]) -> Option<Namespace> {
    let s = String::from_utf8_lossy(line);
    let rest = s.trim().strip_prefix("* NAMESPACE")?.trim_start();
    if rest.starts_with("NIL") {
        return None;
    }
    let inner = rest.strip_prefix("((")?;
    let (prefix, after) = read_qstring(inner.trim_start())?;
    let after = after.trim_start();
    let delimiter = if after.starts_with("NIL") { None } else { read_qstring(after).map(|(d, _)| d) };
    Some(Namespace { prefix, delimiter })
}

fn read_qstring(s: &str) -> Option<(String, &str)> {
    let s = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                if let Some((_, n)) = chars.next() {
                    out.push(n);
                }
            }
            '"' => return Some((out, &s[i + 1..])),
            _ => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_and_literals() {
        assert_eq!(uid_set(&[5, 1, 2, 3, 9, 10, 7]), "1:3,5,7,9:10");
        assert_eq!(literal_len(b"* 1 FETCH (BODY[] {120}"), Some(120));
        assert_eq!(literal_len(b"A1 APPEND x {12+}"), Some(12));
        assert_eq!(literal_len(b"* OK {fine}"), None);
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(
            bracket_code("[ALREADYEXISTS] Mailbox exists"),
            (Some("ALREADYEXISTS".into()), "Mailbox exists".into())
        );
    }

    #[test]
    fn namespace_lines() {
        let ns = parse_namespace(b"* NAMESPACE ((\"INBOX.\" \".\")) NIL NIL\r\n").unwrap();
        assert_eq!((ns.prefix.as_str(), ns.delimiter.as_deref()), ("INBOX.", Some(".")));
        let ns = parse_namespace(b"* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n").unwrap();
        assert_eq!((ns.prefix.as_str(), ns.delimiter.as_deref()), ("", Some("/")));
    }
}
