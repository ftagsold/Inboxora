//! Body-structure walking and part decoding for the engine (imapManager's `walkStructure`,
//! `decodeBody`, attachment decoding and text-payload sniffing).

use std::sync::LazyLock;

use inboxora_mail::body::{BodyStructure, decode_quoted_printable};
use inboxora_mail::headers::{base64_lenient, decode_charset};
use regex::Regex;
use serde_json::{Value, json};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PartRef {
    pub part: String,
    pub mime_type: String,
    pub encoding: String,
    pub charset: String,
    pub cid: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttachmentRef {
    pub part: String,
    pub filename: String,
    pub mime_type: String,
    pub encoding: String,
    pub size: u64,
    pub disposition: Option<String>,
}

impl AttachmentRef {
    /// The attachment JSON stored in `messages.attachments`.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "part": self.part,
            "filename": self.filename,
            "type": self.mime_type,
            "encoding": self.encoding,
            "size": self.size,
        });
        if let Some(d) = &self.disposition {
            v["disposition"] = json!(d);
        }
        v
    }
}

#[derive(Debug, Clone, Default)]
pub struct Collected {
    pub text_parts: Vec<PartRef>,
    pub attachments: Vec<AttachmentRef>,
    pub calendar_parts: Vec<PartRef>,
    pub inline_images: Vec<PartRef>,
}

fn strip_bidi(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200F}' | '\u{061C}'))
        .collect()
}

fn part_no(node: &BodyStructure) -> String {
    if node.part.is_empty() { "1".into() } else { node.part.clone() }
}

pub fn walk_structure(node: &BodyStructure, out: &mut Collected) {
    if !node.children.is_empty() {
        for child in &node.children {
            walk_structure(child, out);
        }
        return;
    }
    let ty = node.mime_type.to_lowercase();
    let disposition = node.disposition.clone().unwrap_or_default().to_lowercase();
    let raw_name =
        node.filename.clone().filter(|f| !f.is_empty()).or_else(|| node.name.clone().filter(|n| !n.is_empty()));
    let filename = raw_name.map(|n| {
        let s = strip_bidi(&n);
        let t = s.trim();
        if t.is_empty() { "attachment".to_owned() } else { t.to_owned() }
    });
    let charset = node.charset.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| "utf-8".into());
    let size = node.disposition_size.or(node.size).unwrap_or(0);
    if matches!(ty.as_str(), "text/calendar" | "application/ics" | "application/ical" | "application/calendar") {
        out.calendar_parts.push(PartRef {
            part: part_no(node),
            mime_type: ty.clone(),
            encoding: node.encoding.clone(),
            charset,
            cid: None,
        });
        out.attachments.push(AttachmentRef {
            part: part_no(node),
            filename: filename.unwrap_or_else(|| "invitation.ics".into()),
            mime_type: ty,
            encoding: node.encoding.clone(),
            size: node.size.unwrap_or(0),
            disposition: None,
        });
        return;
    }
    let mime = if node.mime_type.is_empty() { "application/octet-stream".to_owned() } else { node.mime_type.clone() };
    let enc_or_b64 = if node.encoding.is_empty() { "base64".to_owned() } else { node.encoding.clone() };
    if disposition == "attachment" {
        out.attachments.push(AttachmentRef {
            part: part_no(node),
            filename: filename.unwrap_or_else(|| "attachment".into()),
            mime_type: mime,
            encoding: enc_or_b64,
            size,
            disposition: Some(disposition),
        });
    } else if ty == "text/html" || ty == "application/xhtml+xml" || ty == "text/plain" {
        out.text_parts.push(PartRef {
            part: part_no(node),
            mime_type: if ty == "application/xhtml+xml" { "text/html".into() } else { ty },
            encoding: node.encoding.clone(),
            charset,
            cid: None,
        });
    } else if ty.starts_with("image/") && node.id.is_some() {
        let id = node.id.clone().unwrap_or_default();
        let cid = id.strip_prefix('<').unwrap_or(&id);
        let cid = cid.strip_suffix('>').unwrap_or(cid).to_owned();
        out.inline_images.push(PartRef {
            part: part_no(node),
            mime_type: if node.mime_type.is_empty() { "image/png".into() } else { node.mime_type.clone() },
            encoding: enc_or_b64,
            charset: String::new(),
            cid: Some(cid),
        });
    } else if let Some(filename) = filename {
        out.attachments.push(AttachmentRef {
            part: part_no(node),
            filename,
            mime_type: mime,
            encoding: enc_or_b64,
            size,
            disposition: Some(disposition),
        });
    }
}

pub fn should_fallback_to_text_part(c: &Collected) -> bool {
    c.text_parts.is_empty() && c.calendar_parts.is_empty()
}

fn decode_bytes(raw: &[u8], charset: &str) -> String {
    let cs = charset.trim().trim_matches(|c| c == '"' || c == '\'').to_lowercase();
    let cs = if cs.is_empty() || cs == "us-ascii" || cs == "ascii" { "utf-8".to_owned() } else { cs };
    decode_charset(raw, &cs)
}

fn transfer_decode(raw: &[u8], encoding: &str) -> Vec<u8> {
    match encoding.to_lowercase().as_str() {
        "base64" => {
            let ascii: String = raw.iter().filter(|b| !b.is_ascii_whitespace()).map(|b| *b as char).collect();
            base64_lenient(&ascii)
        }
        "quoted-printable" => decode_quoted_printable(raw),
        _ => raw.to_vec(),
    }
}

fn parse_mime_headers(block: &str) -> Vec<(String, String)> {
    static FOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\r?\n[ \t]+").expect("regex"));
    let unfolded = FOLD.replace_all(block, " ");
    let mut out: Vec<(String, String)> = Vec::new();
    for line in unfolded.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some((k, v)) = line.split_once(':')
            && !k.is_empty()
        {
            let k = k.to_lowercase();
            out.retain(|(x, _)| *x != k);
            out.push((k, v.trim().to_owned()));
        }
    }
    out
}

/// A text part that is really a whole multipart fragment (broken servers): pick its HTML or
/// plain alternative instead of showing boundaries and QP soup.
fn unwrap_embedded_mime_text(decoded: String, depth: usize) -> String {
    static START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^--[^\r\n]+\r?\nContent-").expect("regex"));
    static CHARSET: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)charset=(?:"([^"]+)"|([^;\s]+))"#).expect("regex"));
    if depth >= 5 {
        return decoded;
    }
    let start = decoded.trim_start();
    if !START.is_match(start) {
        return decoded;
    }
    let Some(first_end) = start.find('\n') else { return decoded };
    let marker = start[..first_end].trim();
    let boundary = marker.strip_prefix("--").unwrap_or(marker);
    if boundary.is_empty() {
        return decoded;
    }
    let part_re = Regex::new(&format!(r"(?:^|\r?\n)--{}(?:--)?\r?\n?", regex::escape(boundary))).expect("regex");
    let mut candidates: Vec<(String, String)> = Vec::new();
    for part in part_re.split(start) {
        let trimmed = part.strip_prefix("\r\n").or_else(|| part.strip_prefix('\n')).unwrap_or(part);
        static SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\r?\n\r?\n").expect("regex"));
        let Some(m) = SEP.find(trimmed) else { continue };
        let at = m.start();
        let header_block = &trimmed[..at];
        let rest = &trimmed[at..];
        let payload = rest.strip_prefix("\r\n\r\n").unwrap_or_else(|| rest.get(2..).unwrap_or(""));
        let headers = parse_mime_headers(header_block);
        let get = |k: &str| headers.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
        let Some(ct) = get("content-type") else { continue };
        let (ty, params) = ct.split_once(';').map_or((ct, ""), |(a, b)| (a, b));
        let ty = ty.trim().to_lowercase();
        if ty != "text/html" && ty != "text/plain" {
            continue;
        }
        let charset = CHARSET
            .captures(params)
            .and_then(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_owned()))
            .unwrap_or_else(|| "utf-8".into());
        let enc = get("content-transfer-encoding").unwrap_or("");
        let text = match enc.to_lowercase().as_str() {
            "base64" | "quoted-printable" => decode_bytes(&transfer_decode(payload.as_bytes(), enc), &charset),
            _ => decode_bytes(payload.as_bytes(), &charset),
        };
        candidates.push((ty, text));
    }
    let best = candidates
        .iter()
        .find(|(t, _)| t == "text/html")
        .or_else(|| candidates.iter().find(|(t, _)| t == "text/plain"));
    match best {
        Some((_, text)) => unwrap_embedded_mime_text(text.clone(), depth + 1),
        None => decoded,
    }
}

/// `decodeBody(buf, encoding, charset)`.
pub fn decode_body(raw: &[u8], encoding: &str, charset: &str) -> String {
    unwrap_embedded_mime_text(decode_bytes(&transfer_decode(raw, encoding), charset), 0)
}

/// Raw attachment bytes with the transfer encoding removed.
pub fn decode_attachment(raw: &[u8], encoding: &str) -> Vec<u8> {
    transfer_decode(raw, encoding)
}

/// The encoding an attachment part must be decoded with: the declared one, else `fallback`.
pub fn attachment_transfer_encoding(c: &Collected, part: &str, fallback: &str) -> String {
    c.attachments
        .iter()
        .find(|a| a.part == part)
        .map(|a| a.encoding.clone())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

pub fn looks_like_text_payload(buf: &[u8]) -> bool {
    static TAGS: LazyLock<Regex> = LazyLock::new(|| {
        let tags = "a|article|aside|audio|b|blockquote|br|button|caption|code|dd|details|div|dl|dt|em|figure|footer|form|h[1-6]|header|hr|html|head|body|i|img|input|label|li|main|nav|ol|p|pre|section|small|span|strong|style|table|tbody|td|th|thead|tr|u|ul|video";
        Regex::new(&format!(
            r"(?i)(?:<!doctype|<!--|</?(?:{tags})\b|=3c/?(?:{tags})(?:=3e|=20|=09)|&lt;/?(?:{tags})(?:&gt;|\s)|(?:^|[\r\n])Content-(?:Type|Transfer-Encoding):)"
        ))
        .expect("regex")
    });
    if buf.is_empty() {
        return false;
    }
    let png = buf.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    let jpeg = buf.starts_with(&[0xFF, 0xD8, 0xFF]);
    let gif = buf.starts_with(b"GIF87a") || buf.starts_with(b"GIF89a");
    let webp = buf.len() >= 12 && &buf[..4] == b"RIFF" && &buf[8..12] == b"WEBP";
    if png || jpeg || gif || webp {
        return false;
    }
    let head = &buf[..buf.len().min(512)];
    let non_text = head.iter().filter(|&&b| b < 0x09 || (b > 0x0D && b < 0x20) || b > 0x7E).count();
    if non_text * 8 > head.len() {
        return false;
    }
    // Buffer#toString('ascii') masks the high bit.
    let sample: String = head.iter().map(|b| (b & 0x7f) as char).collect();
    TAGS.is_match(&sample)
}

/// Body from an already-fetched message (`extractBodyFromMsg`).
pub fn extract_body(
    structure: Option<&BodyStructure>,
    parts: &std::collections::HashMap<String, Vec<u8>>,
) -> (Option<String>, Option<String>, Vec<AttachmentRef>) {
    let Some(s) = structure else { return (None, None, vec![]) };
    let mut c = Collected::default();
    walk_structure(s, &mut c);
    if should_fallback_to_text_part(&c) {
        let root = s.mime_type.to_lowercase();
        c.text_parts.push(PartRef {
            part: part_no(s),
            mime_type: if root == "text/html" || root == "text/plain" { root } else { "text/plain".into() },
            encoding: s.encoding.clone(),
            charset: String::new(),
            cid: None,
        });
    }
    let (mut html, mut text) = (None, None);
    for p in &c.text_parts {
        let Some(buf) = parts.get(&p.part) else { continue };
        let decoded = decode_body(buf, &p.encoding, &p.charset);
        if p.mime_type == "text/html" && html.is_none() {
            html = Some(decoded);
        } else if p.mime_type == "text/plain" && text.is_none() {
            text = Some(decoded);
        }
    }
    (html, text, c.attachments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(part: &str, ty: &str) -> BodyStructure {
        BodyStructure { part: part.into(), mime_type: ty.into(), encoding: "base64".into(), ..Default::default() }
    }

    #[test]
    fn attachment_classification() {
        let mut html_attached = leaf("2", "text/html");
        html_attached.disposition = Some("attachment".into());
        html_attached.filename = Some("page.html".into());
        let mut ics = leaf("3", "text/calendar");
        ics.size = Some(10);
        let mut img = leaf("4", "image/png");
        img.id = Some("<logo@x>".into());
        let mut named = leaf("5", "application/pdf");
        named.name = Some("\u{202E}fdp.exe".into());
        let root = BodyStructure {
            mime_type: "multipart/mixed".into(),
            children: vec![leaf("1", "text/plain"), html_attached, ics, img, named],
            ..Default::default()
        };
        let mut c = Collected::default();
        walk_structure(&root, &mut c);
        assert_eq!(c.text_parts.len(), 1);
        assert_eq!(c.text_parts[0].mime_type, "text/plain");
        assert_eq!(
            c.attachments.iter().map(|a| a.filename.as_str()).collect::<Vec<_>>(),
            ["page.html", "invitation.ics", "fdp.exe"]
        );
        assert_eq!(c.attachments[1].encoding, "base64");
        assert_eq!(c.calendar_parts.len(), 1);
        assert_eq!(c.inline_images[0].cid.as_deref(), Some("logo@x"));
        assert!(!should_fallback_to_text_part(&c));
    }

    #[test]
    fn decodes_and_unwraps() {
        assert_eq!(decode_body(b"Hall=C3=B6 =E2=80=94", "quoted-printable", "utf-8"), "Hallö —");
        assert_eq!(decode_body(b"SGFsbPY=", "base64", "iso-8859-1"), "Hallö");
        let embedded = "--b1\r\nContent-Type: text/plain\r\n\r\nplain\r\n--b1\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n<p>caf=C3=A9</p>\r\n--b1--\r\n";
        assert_eq!(decode_body(embedded.as_bytes(), "7bit", "utf-8").trim(), "<p>café</p>");
    }

    #[test]
    fn text_payload_sniffing() {
        assert!(looks_like_text_payload(b"<!DOCTYPE html><html>"));
        assert!(looks_like_text_payload(b"=3Cdiv=3Ehello"));
        assert!(!looks_like_text_payload(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0]));
        assert!(!looks_like_text_payload(b"just words"));
    }
}
