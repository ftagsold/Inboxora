//! MIME body parts: transfer decoding and body-structure walking.

use crate::headers::{base64_lenient, decode_charset};

/// A BODYSTRUCTURE node (imapflow's shape).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BodyStructure {
    /// Lowercase `type/subtype`.
    pub mime_type: String,
    /// IMAP part number (`1`, `1.2`, …); empty for the root of a multipart message.
    pub part: String,
    pub encoding: String,
    pub disposition: Option<String>,
    pub charset: Option<String>,
    pub filename: Option<String>,
    pub id: Option<String>,
    pub size: Option<u64>,
    pub children: Vec<BodyStructure>,
}

/// The text part chosen for a snippet, with the HTML alternative as fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct SnippetPart {
    pub part: String,
    pub mime_type: String,
    pub encoding: String,
    pub charset: String,
    pub html_fallback: Option<Box<SnippetPart>>,
}

/// Prefer text/plain, falling back to text/html; attachments are skipped.
pub fn find_snippet_part(s: &BodyStructure) -> Option<SnippetPart> {
    if !s.children.is_empty() {
        let mut plain: Option<SnippetPart> = None;
        let mut html: Option<SnippetPart> = None;
        for child in &s.children {
            let Some(found) = find_snippet_part(child) else { continue };
            if found.mime_type == "text/plain" {
                if plain.is_none() {
                    plain = Some(found);
                }
            } else if html.is_none() {
                html = Some(found);
            }
        }
        if let Some(mut p) = plain {
            if p.html_fallback.is_none() {
                p.html_fallback = html.map(Box::new);
            }
            return Some(p);
        }
        return html;
    }
    if s.disposition.as_deref().is_some_and(|d| d.eq_ignore_ascii_case("attachment")) {
        return None;
    }
    let t = s.mime_type.to_lowercase();
    if t == "text/plain" || t == "text/html" {
        return Some(SnippetPart {
            part: if s.part.is_empty() { "1".into() } else { s.part.clone() },
            mime_type: t,
            encoding: s.encoding.to_lowercase(),
            charset: s.charset.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| "utf-8".into()),
            html_fallback: None,
        });
    }
    None
}

/// True when any node has `Content-Disposition: attachment`.
pub fn detect_attachments(s: &BodyStructure) -> bool {
    s.disposition.as_deref() == Some("attachment") || s.children.iter().any(detect_attachments)
}

/// Quoted-printable decoding (soft line breaks removed, invalid escapes kept literally).
pub fn decode_quoted_printable(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'=' {
            if raw.get(i + 1) == Some(&b'\r') && raw.get(i + 2) == Some(&b'\n') {
                i += 3;
                continue;
            }
            if raw.get(i + 1) == Some(&b'\n') {
                i += 2;
                continue;
            }
            if let (Some(a), Some(b)) = (raw.get(i + 1), raw.get(i + 2))
                && a.is_ascii_hexdigit()
                && b.is_ascii_hexdigit()
            {
                let hex = [*a, *b];
                out.push(u8::from_str_radix(std::str::from_utf8(&hex).unwrap_or("00"), 16).unwrap_or(0));
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    out
}

/// Remove the transfer encoding.
pub fn decode_transfer(raw: &[u8], encoding: &str) -> Vec<u8> {
    match encoding.to_ascii_lowercase().as_str() {
        "base64" => {
            let ascii: String = raw.iter().filter(|b| !b.is_ascii_whitespace()).map(|b| *b as char).collect();
            base64_lenient(&ascii)
        }
        "quoted-printable" => decode_quoted_printable(raw),
        _ => raw.to_vec(),
    }
}

/// Decode a fetched body part to text using its transfer encoding and charset.
pub fn decode_body_part(raw: &[u8], encoding: &str, charset: &str) -> String {
    decode_charset(&decode_transfer(raw, encoding), charset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_transfer_encodings() {
        assert_eq!(decode_body_part(b"Y2Fm\r\nw6k=", "base64", "utf-8"), "café");
        assert_eq!(decode_body_part(b"caf=C3=A9 =\r\nok =ZZ", "quoted-printable", "utf-8"), "café ok =ZZ");
        assert_eq!(decode_body_part(b"caf\xe9", "8bit", "iso-8859-1"), "café");
        assert_eq!(decode_body_part("\u{FEFF}bom".as_bytes(), "7bit", "utf-8"), "bom");
    }

    #[test]
    fn prefers_plain_with_html_fallback() {
        let leaf = |part: &str, t: &str| BodyStructure { part: part.into(), mime_type: t.into(), ..Default::default() };
        let s = BodyStructure {
            mime_type: "multipart/alternative".into(),
            children: vec![leaf("1", "text/plain"), leaf("2", "text/html")],
            ..Default::default()
        };
        let p = find_snippet_part(&s).unwrap();
        assert_eq!(p.part, "1");
        assert_eq!(p.html_fallback.unwrap().part, "2");
    }
}
