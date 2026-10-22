//! Small pure helpers shared by routes (ports of `utils/*.ts`).

use std::sync::LazyLock;

use regex::Regex;

use crate::html::{self, Event};
use crate::sanitize::encode_uri_component;

/// Attachment filename safe for headers and file systems (`safeFilename`).
pub fn safe_filename(name: Option<&str>) -> String {
    let Some(name) = name.filter(|n| !n.is_empty()) else { return "attachment".into() };
    let cleaned: String = name
        .replace(['/', '\\'], "_")
        .chars()
        .filter(|c| {
            let code = *c as u32;
            // charCodeAt(0) of a non-BMP character is its high surrogate (> 0x1f), so it is kept.
            code > 0x1f && code != 0x7f
        })
        .filter(|c| !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200f}' | '\u{061c}'))
        .collect();
    let trimmed = crate::text::js_trim(&cleaned);
    let out = crate::text::js_substring(trimmed, 255);
    if out.is_empty() { "attachment".into() } else { out }
}

fn rfc5987(s: &str) -> String {
    encode_uri_component(s).replace('\'', "%27").replace('(', "%28").replace(')', "%29").replace('*', "%2A")
}

/// `Content-Disposition: attachment` with an ASCII fallback and RFC 5987 UTF-8 name.
pub fn attachment_disposition(raw_name: &str) -> String {
    let safe = safe_filename(Some(raw_name));
    // JS replaces per UTF-16 code unit: a non-BMP character becomes two underscores.
    let ascii: String =
        safe.chars()
            .map(|c| {
                if (' '..='~').contains(&c) && c != '"' && c != '\\' {
                    c.to_string()
                } else {
                    "_".repeat(c.len_utf16())
                }
            })
            .collect();
    let ascii = if ascii.is_empty() { "attachment".to_owned() } else { ascii };
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", rfc5987(&safe))
}

/// An inline image extracted from compose HTML.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineAttachment {
    pub filename: String,
    pub content: Vec<u8>,
    pub cid: String,
    pub content_type: String,
}

fn mime_to_extension(sub: &str) -> String {
    let sub = sub.to_lowercase();
    match sub.as_str() {
        "jpeg" => "jpg".into(),
        "svg+xml" => "svg".into(),
        _ => {
            let s: String = sub
                .chars()
                .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-'))
                .collect();
            if s.is_empty() { "png".into() } else { s }
        }
    }
}

/// Replace `<img src="data:image/…;base64,…">` with `cid:` references and return the images
/// as inline attachments (`embedInlineDataImages`).
pub fn embed_inline_data_images(html_in: &str) -> (String, Vec<InlineAttachment>) {
    static IMG_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<img\b([^>]*)>").expect("regex"));
    static DATA_SRC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)\ssrc=["'](data:image/([^;]+);base64,([^"']+))["']"#).expect("regex"));
    let mut attachments = Vec::new();
    let rewritten = IMG_TAG.replace_all(html_in, |c: &regex::Captures| {
        let attrs = &c[1];
        let Some(m) = DATA_SRC.captures(attrs) else { return c[0].to_owned() };
        let index = attachments.len();
        let cid = format!("img-{}-{index}@mailflow", hex_random(8));
        attachments.push(InlineAttachment {
            filename: format!("image-{index}.{}", mime_to_extension(&m[2])),
            content: crate::headers::base64_lenient(&m[3]),
            cid: cid.clone(),
            content_type: format!("image/{}", &m[2]),
        });
        let new_attrs = DATA_SRC.replace(attrs, regex::NoExpand(&format!(" src=\"cid:{cid}\"")));
        format!("<img{new_attrs}>")
    });
    (rewritten.into_owned(), attachments)
}

fn hex_random(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("OS random source");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

const RICH_TAGS: &str = "a|abbr|address|article|aside|b|big|blockquote|body|br|center|cite|code|dd|del|dfn|div|dl|dt|em|figcaption|figure|font|footer|h[1-6]|head|header|hr|html|i|img|ins|kbd|li|main|mark|meta|nav|ol|o:p|p|pre|q|s|samp|section|small|span|strike|strong|style|sub|sup|table|tbody|td|tfoot|th|thead|tr|tt|u|ul|var";

/// Whether a calendar/contact description looks like HTML.
pub fn is_html_description(value: &str) -> bool {
    static MARKUP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(r"(?i)<\s*(?:{RICH_TAGS})(?:\s[^<>]*)?/?>|<\s*/\s*(?:{RICH_TAGS})\s*>")).expect("regex")
    });
    static ENTITY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)&(?:nbsp|amp|lt|gt|quot|#\d{2,5}|#x[0-9a-f]{2,5});").expect("regex"));
    !value.is_empty() && (MARKUP.is_match(value) || ENTITY.is_match(value))
}

/// Sanitize a description only when it is HTML.
pub fn sanitize_description_html(value: &str) -> String {
    if is_html_description(value) { crate::sanitize::sanitize_compose_body(value) } else { value.to_owned() }
}

/// Trimmed, sanitized description or `None` when empty.
pub fn normalize_description(value: &str) -> Option<String> {
    let trimmed = crate::text::js_trim(value);
    if trimmed.is_empty() { None } else { Some(sanitize_description_html(trimmed)) }
}

/// Plain text from HTML: block ends and `<br>` become newlines, script/style dropped.
pub fn html_to_plain_text(input: &str) -> String {
    static TRAILING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+\n").expect("regex"));
    static MANY_NL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").expect("regex"));
    if input.is_empty() {
        return String::new();
    }
    let mut suppressed = 0u32;
    let mut text = String::new();
    html::parse(input, |ev| match ev {
        Event::Open { name, .. } => {
            if name == "script" || name == "style" {
                suppressed += 1;
            }
            if suppressed == 0 && name == "br" {
                text.push('\n');
            }
        }
        Event::Text(t) => {
            if suppressed == 0 {
                text.push_str(&t);
            }
        }
        Event::Close { name, .. } => {
            if name == "script" || name == "style" {
                suppressed = suppressed.saturating_sub(1);
            }
            if suppressed == 0
                && matches!(
                    name.as_str(),
                    "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" | "pre"
                )
            {
                text.push('\n');
            }
        }
    });
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = TRAILING.replace_all(&text, "\n");
    let text = MANY_NL.replace_all(&text, "\n\n");
    crate::text::js_trim(&text).to_owned()
}

/// ICS `DESCRIPTION` (+ `X-ALT-DESC` for HTML) content lines.
pub fn description_content_lines(description: Option<&str>, escape_text: impl Fn(&str) -> String) -> Vec<String> {
    let Some(d) = description.filter(|d| !d.is_empty()) else { return Vec::new() };
    if !is_html_description(d) {
        return vec![format!("DESCRIPTION:{}", escape_text(d))];
    }
    vec![
        format!("DESCRIPTION:{}", escape_text(&html_to_plain_text(d))),
        format!("X-ALT-DESC;FMTTYPE=text/html:{}", escape_text(d)),
    ]
}

const MAX_MESSAGE_ID_LENGTH: usize = 998;

/// Canonical `<id>` form, or `None` when the value is not a usable Message-ID.
pub fn normalize_message_id(value: &str) -> Option<String> {
    let text = value.replace(['\r', '\n'], " ");
    let text = crate::text::js_trim(&text);
    if text.is_empty() {
        return None;
    }
    let inner = text.strip_prefix('<').and_then(|t| t.strip_suffix('>')).filter(|t| !t.contains(['<', '>']));
    let candidate = crate::text::js_trim(inner.unwrap_or(text));
    if candidate.is_empty() || crate::text::js_len(candidate) > MAX_MESSAGE_ID_LENGTH {
        return None;
    }
    if candidate.chars().any(|c| crate::text::is_js_space(c) || !('\x21'..='\x7e').contains(&c) || c == '<' || c == '>')
    {
        return None;
    }
    Some(format!("<{candidate}>"))
}

/// All distinct `<id>` tokens in a References/In-Reply-To style value.
pub fn normalize_message_id_list(text: &str) -> Vec<String> {
    static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^<>\r\n]+>").expect("regex"));
    let mut ids: Vec<String> = Vec::new();
    for m in ID.find_iter(text) {
        if let Some(n) = normalize_message_id(m.as_str())
            && !ids.contains(&n)
        {
            ids.push(n);
        }
    }
    ids
}

/// Image types served directly (`safeImageType`).
pub fn safe_image_type(t: Option<&str>) -> Option<&'static str> {
    let base = t.unwrap_or("").split(';').next().unwrap_or("").trim().to_lowercase();
    match base.as_str() {
        "image/jpg" | "image/jpeg" => Some("image/jpeg"),
        "image/png" => Some("image/png"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

/// `UUID_RE`.
pub fn is_uuid(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() == 36
        && b.iter()
            .enumerate()
            .all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

/// OIDC claim name validation (`normalizeClaimName`).
pub fn normalize_claim_name(v: &str) -> Option<String> {
    let c = v.trim();
    let ok = (1..=64).contains(&c.len())
        && c.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
    (ok && !matches!(c, "__proto__" | "constructor" | "prototype")).then(|| c.to_owned())
}

/// `redactEmail`.
pub fn redact_email(email: &str) -> String {
    match email.find('@') {
        Some(at) if !email.is_empty() => {
            let first = email.chars().next().map(String::from).unwrap_or_default();
            format!("{first}***{}", &email[at..])
        }
        _ => "[email]".into(),
    }
}

/// First 32 hex chars of SHA-256 (`hrefHash`).
pub fn href_hash(href: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(href.as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// What to do when the user deletes a message.
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteStrategy {
    NoTrash,
    Expunge,
    Move(String),
}

/// `getDeleteStrategy`.
pub fn delete_strategy(message_folder: &str, trash_path: Option<&str>, all_trash: Option<&[String]>) -> DeleteStrategy {
    let Some(trash) = trash_path.filter(|t| !t.is_empty()) else { return DeleteStrategy::NoTrash };
    let in_trash = match all_trash {
        Some(all) => all.iter().any(|p| p == message_folder),
        None => message_folder == trash,
    };
    if in_trash { DeleteStrategy::Expunge } else { DeleteStrategy::Move(trash.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispositions_and_filenames() {
        assert_eq!(
            attachment_disposition("ré\u{202e}port (1).pdf"),
            "attachment; filename=\"r_port (1).pdf\"; filename*=UTF-8''r%C3%A9port%20%281%29.pdf"
        );
        assert_eq!(safe_filename(Some("../a/b")), ".._a_b");
        assert_eq!(safe_filename(Some("\u{1}")), "attachment");
    }

    #[test]
    fn message_ids() {
        assert_eq!(normalize_message_id(" <a@b> ").as_deref(), Some("<a@b>"));
        assert_eq!(normalize_message_id("a@b").as_deref(), Some("<a@b>"));
        assert_eq!(normalize_message_id("<a b>"), None);
        assert_eq!(normalize_message_id("<é@b>"), None);
        assert_eq!(normalize_message_id_list("<a@b> <c@d>\n<a@b>"), vec!["<a@b>", "<c@d>"]);
    }

    #[test]
    fn inline_images_become_cid_attachments() {
        let (html, att) = embed_inline_data_images("<p><img alt=x src=\"data:image/png;base64,iVBO\"></p>");
        assert_eq!(att.len(), 1);
        assert!(html.contains(&format!("src=\"cid:{}\"", att[0].cid)));
        assert_eq!(att[0].filename, "image-0.png");
    }

    #[test]
    fn plain_text_from_html() {
        assert_eq!(html_to_plain_text("<p>a</p><div>b<br>c</div><style>x</style>  \n\n\n d"), "a\nb\nc\n\n d");
    }
}
